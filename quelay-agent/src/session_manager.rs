//! [`SessionManager`] — owns the remote session and drives the reconnection loop.
//!
//! # Current scope (single remote)
//!
//! Today `SessionManager` manages exactly one remote peer, held in
//! `self.remote: Option<RemoteState>`.  The `RemoteState` struct is the unit
//! of per-peer state so future expansion to multiple peers is a mechanical
//! refactor:
//!
//! ```text
//! // Today
//! remote: Option<RemoteState>
//!
//! // Future (multi-peer)
//! remotes: HashMap<RemoteId, RemoteState>
//! ```
//!
//! # Reconnection loop
//!
//! `SessionManager` holds the transport config (bind address / peer address +
//! cert) so it can reconnect without involving `main.rs` or `Agent`.  When the
//! QUIC session drops the loop retries with exponential back-off until it
//! re-establishes, then re-drains the pending UUID map and re-opens every
//! in-flight uplink stream via `restore_active`.
//!
//! # Spool (stubbed)
//!
//! When `LinkState` transitions to `Failed` the spool path is logged and a
//! `TODO` marker is left.  The actual disk I/O will be added in the next
//! iteration.

use super::ActiveStream;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ---

use tokio::sync::{mpsc, Mutex, Notify};
use uuid::Uuid;

// ---

use quelay_domain::{
    //
    LinkState,
    Priority,
    QueLaySessionPtr,
    StreamInfo,
};

// ---

use super::{
    // ---
    write_connect_header,
    write_reconnect_header,
    AggregateRateLimiter,
    CallbackTx,
    ReconnectHeader,
    StreamHeader,
    UplinkContext,
};

// ---------------------------------------------------------------------------
// TransportConfig
// ---------------------------------------------------------------------------

/// Everything `SessionManager` needs to (re)establish the QUIC session.
///
/// Owned by the session manager so `main.rs` does not need to be involved
/// in reconnection.
///
/// For server mode we retain `sess_rx` — the channel the `listen()` accept
/// loop already writes into — rather than calling `listen()` again on
/// reconnect (which would spawn a competing accept loop on the same endpoint).
pub enum TransportConfig {
    // ---
    /// Server mode: hold the existing accept-loop receiver and `recv()` again
    /// after each disconnection.
    Server {
        sess_rx: mpsc::Receiver<quelay_quic::QuicSession>,
    },

    /// Client mode: reconnect by constructing a new `QuicTransport` and
    /// calling `connect()`.
    Client {
        peer: std::net::SocketAddr,
        server_name: String,
        cert_der: rustls_pki_types::CertificateDer<'static>,
    },
}

// ---------------------------------------------------------------------------
// PendingStream
// ---------------------------------------------------------------------------

/// A stream that has been accepted by the Thrift handler but not yet sent.
///
/// Survives a link outage inside `RemoteState::pending`.  When the session
/// reconnects, the session manager re-issues every pending stream in
/// priority order.
///
/// Partially-sent streams are tracked in `RemoteState::active_uplinks` (not
/// here) because their [`UplinkHandle`] carries the spool and reconnect channel.
#[derive(Debug)]
struct PendingStream {
    // ---
    uuid: Uuid,
    info: StreamInfo,
    priority: Priority,
}

// ---------------------------------------------------------------------------
// RemoteState
// ---------------------------------------------------------------------------

/// All per-peer state for one remote Quelay node.
///
/// Today there is exactly one of these.  Future multi-peer support promotes
/// this to a `HashMap<RemoteId, RemoteState>` in `SessionManager`.
struct RemoteState {
    // ---
    /// Live QUIC session.  `None` while reconnecting.
    session: Option<QueLaySessionPtr>,

    /// Streams queued but not yet opened on the wire.
    ///
    /// Key: stable UUID (survives reconnection).
    /// On reconnect every entry here is re-submitted in arrival order.
    /// Future: order by priority using the DRR scheduler.
    pending: HashMap<Uuid, PendingStream>,

    /// In-flight uplink streams.
    ///
    /// On link failure each handle is signalled with a fresh stream after
    /// reconnect so the pump can replay `A..T` and resume.
    active_uplinks: HashMap<Uuid, super::UplinkHandle>,

    /// In-flight downlink streams.
    ///
    /// On reconnect the `accept_loop` delivers a fresh QUIC stream to each
    /// pump via [`super::DownlinkHandle::stream_tx`].
    active_downlinks: HashMap<Uuid, super::DownlinkHandle>,
}

// ---

impl RemoteState {
    // ---

    fn new(session: QueLaySessionPtr) -> Self {
        // ---
        Self {
            session: Some(session),
            pending: HashMap::new(),
            active_uplinks: HashMap::new(),
            active_downlinks: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

pub struct SessionManager {
    // ---
    /// Single remote peer.
    ///
    /// Future: `HashMap<RemoteId, RemoteState>`
    remote: Arc<Mutex<Option<RemoteState>>>,

    /// Transport config retained for reconnection.
    ///
    /// Behind a `Mutex` because `TransportConfig::Server` owns `sess_rx`,
    /// which must be mutably consumed (`recv()`) on each reconnect.
    transport_cfg: Mutex<TransportConfig>,

    /// Shared link state observable by `Agent` and the Thrift handler.
    link_state: Arc<Mutex<LinkState>>,

    /// Sender handle to the [`CallbackAgent`] thread.
    ///
    /// Cloned into each [`ActiveStream`] task so it can fire lifecycle
    /// events (stream_started, stream_done, stream_failed) directly.
    cb_tx: CallbackTx,

    /// Shared aggregate rate limiter — distributes the configured `bw_cap_bps`
    /// across all concurrent uplink streams via DRR scheduling.
    ///
    /// In uncapped mode (`bw_cap_bps = None`) the ARL is still present but
    /// its timer task is not spawned; each stream gets a direct write half.
    arl: Arc<AggregateRateLimiter>,

    /// Notified by [`run`] after a successful reconnect so the accept loop
    /// can resume calling `accept_stream` on the new session.
    session_restored: Arc<Notify>,
}

// ---

impl SessionManager {
    // ---

    /// Create a new `SessionManager` with an already-established session.
    pub fn new(
        session: QueLaySessionPtr,
        transport_cfg: TransportConfig,
        link_state: Arc<Mutex<LinkState>>,
        cb_tx: CallbackTx,
        bw_cap_bps: Option<u64>,
    ) -> Self {
        // ---
        let remote = RemoteState::new(session);
        Self {
            remote: Arc::new(Mutex::new(Some(remote))),
            transport_cfg: Mutex::new(transport_cfg),
            link_state,
            cb_tx,
            arl: Arc::new(AggregateRateLimiter::new(bw_cap_bps)),
            session_restored: Arc::new(Notify::new()),
        }
    }

    // ---

    /// Enqueue a stream start request.
    ///
    /// If the session is live the stream is opened immediately.
    /// If the session is down the request is queued in `pending` and will
    /// be re-issued when the link recovers.
    pub async fn stream_start(&self, uuid: Uuid, info: StreamInfo, priority: Priority) {
        // ---
        let mut guard = self.remote.lock().await;
        let remote = match guard.as_mut() {
            Some(r) => r,
            None => {
                tracing::warn!(%uuid, "stream_start called but remote slot is empty — queuing");
                return;
            }
        };

        let pending = PendingStream {
            uuid,
            info: info.clone(),
            priority,
        };

        match remote.session.as_ref() {
            Some(session) => {
                match Self::open_stream_on_session(
                    session,
                    &pending,
                    self.cb_tx.clone(),
                    Arc::clone(&self.arl),
                )
                .await
                {
                    Ok(handle) => {
                        tracing::info!(%uuid, "stream opened on live session");
                        remote.active_uplinks.insert(uuid, handle);
                    }
                    Err(e) => {
                        tracing::warn!(%uuid, "open_stream failed ({e}), queuing for retry");
                        remote.pending.insert(uuid, pending);
                    }
                }
            }
            None => {
                tracing::info!(%uuid, "session down, queuing stream for reconnect");
                remote.pending.insert(uuid, pending);
            }
        }
    }

    // ---

    /// Drive the reconnection loop.  Runs forever; spawn with `tokio::spawn`.
    ///
    /// Watches the session's `link_state_rx`.  On `Failed`, clears the live
    /// session, invokes the spool stub, then retries with exponential back-off.
    /// On recovery, drains `pending` and restores `active_uplinks`.
    ///
    /// Also spawns the inbound accept loop, which runs concurrently and is
    /// re-armed via `session_restored` after each reconnect.
    pub async fn run(self: Arc<Self>) {
        // ---
        let mut state_rx = {
            let guard = self.remote.lock().await;
            match guard.as_ref().and_then(|r| r.session.as_ref()) {
                Some(s) => s.link_state_rx(),
                None => {
                    tracing::error!("SessionManager::run called with no initial session");
                    return;
                }
            }
        };

        // Spawn the inbound (downlink) accept loop.
        tokio::spawn(Arc::clone(&self).accept_loop());

        loop {
            if state_rx.changed().await.is_err() {
                tracing::info!("link_state watch channel closed — session manager exiting");
                break;
            }

            let new_state = *state_rx.borrow();
            *self.link_state.lock().await = new_state;
            tracing::info!("link state → {new_state:?}");

            if new_state == LinkState::Failed {
                self.on_link_failed().await;

                let new_session = self.reconnect_loop().await;

                // restore_active delivers fresh streams to pumps; each pump
                // splits the new stream and calls rate_limiter.link_up(tx)
                // which unblocks the timer task.  No link_enabled flag needed.
                let mut guard = self.remote.lock().await;
                if let Some(remote) = guard.as_mut() {
                    state_rx = new_session.link_state_rx();
                    remote.session = Some(new_session);
                    *self.link_state.lock().await = LinkState::Normal;
                    tracing::info!(
                        "session restored — restoring active streams and draining pending queue"
                    );
                    Self::restore_active(remote).await;
                    Self::drain_pending(remote, self.cb_tx.clone(), Arc::clone(&self.arl)).await;
                    // Re-arm the accept loop on the new session.
                    self.session_restored.notify_one();
                }
            }
        }
    }

    // ---

    /// Inbound accept loop — runs as a sibling task to [`run`].
    ///
    /// Calls `accept_stream()` in a loop.  Reads the stream-open header and
    /// dispatches:
    ///
    /// - `OP_NEW_STREAM` → [`ActiveStream::spawn_downlink`], stores the
    ///   returned [`DownlinkHandle`] in `active_downlinks`.
    /// - `OP_RECONNECT` → looks up the existing [`DownlinkHandle`] by UUID
    ///   and calls [`ActiveStream::deliver_reconnect_stream`].
    ///
    /// When the session fails `accept_stream()` returns an error; the loop
    /// waits on `session_restored` before resuming with the new session.
    async fn accept_loop(self: Arc<Self>) {
        // ---
        use super::{read_stream_open, StreamOpen};

        loop {
            // Snapshot the current session under a short-held lock.
            let session = {
                let guard = self.remote.lock().await;
                guard.as_ref().and_then(|r| r.session.clone())
            };

            let session = match session {
                Some(s) => s,
                None => {
                    self.session_restored.notified().await;
                    continue;
                }
            };

            let mut stream = match session.accept_stream().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "accept_stream error ({e}) — closing session to trigger reconnect"
                    );
                    // The peer closed the connection.  Our own run() loop only
                    // wakes on link_state_rx changes, but a remote close does
                    // not automatically update our link_state watch channel.
                    // Call close() which sets LinkState::Failed, waking run()
                    // into reconnect_loop → session_restored.notify_one().
                    let _ = session.close().await;
                    self.session_restored.notified().await;
                    continue;
                }
            };

            // Read the stream-open header to determine new vs reconnect.
            let open = match read_stream_open(&mut stream).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("accept_loop: read_stream_open failed: {e}");
                    continue;
                }
            };

            match open {
                StreamOpen::New(h) => {
                    // accept_loop already decoded the header; pass fields
                    // directly to spawn_downlink_from (stream is positioned
                    // past the header — chunk data is next).
                    //
                    // IMPORTANT: await spawn_downlink_from inline (not in a
                    // spawned task) so the DownlinkHandle is inserted into
                    // active_downlinks before accept_loop loops back to
                    // accept_stream().  If we spawned a task instead, an
                    // OP_RECONNECT stream could arrive before the task runs,
                    // find no entry in active_downlinks, and be dropped —
                    // leaving the downlink pump waiting forever.
                    let uuid = h.uuid;
                    let info = StreamInfo {
                        size_bytes: h.size_bytes,
                        attrs: h.attrs,
                    };
                    tracing::info!(%uuid, "downlink: new stream accepted");
                    let cb_tx = self.cb_tx.clone();
                    match ActiveStream::spawn_downlink_from(uuid, info, stream, cb_tx).await {
                        Ok(handle) => {
                            let mut guard = self.remote.lock().await;
                            if let Some(r) = guard.as_mut() {
                                tracing::debug!(%uuid, "downlink: inserting handle into active_downlinks");
                                r.active_downlinks.insert(uuid, handle);
                            } else {
                                tracing::warn!(%uuid, "downlink: remote is None after spawn — handle dropped, stream will not reconnect");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(%uuid, "downlink: spawn_downlink_from failed: {e}");
                        }
                    }
                }

                StreamOpen::Reconnect(h) => {
                    tracing::info!(uuid = %h.uuid, replay_from = h.replay_from, "downlink: reconnect stream accepted");
                    let mut guard = self.remote.lock().await;
                    if let Some(remote) = guard.as_mut() {
                        // Prune dead handles first.
                        remote.active_downlinks.retain(|u, h| {
                            let alive = !h.stream_tx.is_closed();
                            if !alive {
                                tracing::debug!(%u, "accept_loop: pruning completed downlink");
                            }
                            alive
                        });
                        tracing::debug!(
                            uuid = %h.uuid,
                            n_downlinks = remote.active_downlinks.len(),
                            known_uuids = ?remote.active_downlinks.keys().collect::<Vec<_>>(),
                            "accept_loop: active_downlinks at reconnect"
                        );
                        if let Some(handle) = remote.active_downlinks.get(&h.uuid) {
                            // bytes_written is read from the handle's shared
                            // atomic — the pump keeps it live as it writes.
                            ActiveStream::deliver_reconnect_stream(
                                handle,
                                h.replay_from,
                                stream,
                                h.uuid,
                            );
                        } else {
                            tracing::warn!(uuid = %h.uuid, "accept_loop: reconnect for unknown downlink — dropping");
                        }
                    }
                }
            }
        }
    }

    // ---

    /// Called when the link transitions to `Failed`.
    ///
    /// Clears the dead QUIC session.  Uplink pumps are already blocked on
    /// their write path; downlink pumps will get a QUIC read error and block
    /// on `stream_rx.recv()`.  Both will resume when `restore_active` /
    /// `accept_loop` deliver fresh streams after reconnect.
    async fn on_link_failed(&self) {
        // ---
        tracing::warn!("link failed — clearing dead session, pausing active streams");

        let mut guard = self.remote.lock().await;
        if let Some(remote) = guard.as_mut() {
            remote.session = None;
            tracing::debug!(
                "link failed — {} uplinks, {} downlinks paused",
                remote.active_uplinks.len(),
                remote.active_downlinks.len(),
            );
        }
    }

    // ---

    /// Retry establishing a session with exponential back-off (1s → 30s cap).
    ///
    /// Returns when a new live session is available.
    async fn reconnect_loop(&self) -> QueLaySessionPtr {
        // ---
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);

        loop {
            tracing::info!("attempting reconnect (backoff {}s)...", backoff.as_secs());

            match self.try_connect().await {
                Ok(session) => {
                    tracing::info!("reconnect succeeded");
                    return session;
                }
                Err(e) => {
                    tracing::warn!("reconnect failed: {e} — retrying in {}s", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    // ---

    /// Single attempt to (re)establish the session based on `transport_cfg`.
    async fn try_connect(&self) -> anyhow::Result<QueLaySessionPtr> {
        // ---
        let mut cfg_guard = self.transport_cfg.lock().await;
        match &mut *cfg_guard {
            TransportConfig::Client {
                peer,
                server_name,
                cert_der,
            } => {
                use quelay_domain::QueLayTransport;
                let transport =
                    quelay_quic::QuicTransport::client(cert_der.clone(), server_name.clone())?;
                let session = transport.connect(*peer).await?;
                Ok(Arc::new(session))
            }

            TransportConfig::Server { sess_rx } => {
                let session = sess_rx
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("accept channel closed — endpoint shut down"))?;
                Ok(Arc::new(session))
            }
        }
    }

    // ---

    // -----------------------------------------------------------------------
    // Test / debug — disabled in production builds
    // -----------------------------------------------------------------------

    /// Inject a link-down event (`enabled = false`) or re-enable the link
    /// (`enabled = true`).
    ///
    /// When `false`: sends `RateCmd::LinkDown` to every active uplink's timer
    /// task (so each pump drains queued chunks and rewinds `Q = A`), then
    /// closes the QUIC session to trigger the reconnect loop.
    ///
    /// When `true`: no-op — `restore_active` calls `link_up` after the
    /// reconnect loop delivers fresh streams.
    async fn link_enable(&self, enabled: bool) {
        // ---
        tracing::info!(enabled, "link_enable");

        if !enabled {
            // Signal all active uplinks to drain and rewind.
            {
                let guard = self.remote.lock().await;
                if let Some(remote) = guard.as_ref() {
                    for handle in remote.active_uplinks.values() {
                        handle.notify_link_down();
                    }
                }
            }

            let session = {
                let guard = self.remote.lock().await;
                guard.as_ref().and_then(|r| r.session.clone())
            };
            if let Some(s) = session {
                tracing::info!("link_enable(false) — closing session to trigger reconnect loop");
                let _ = s.close().await;
            }
        }
        // link_enable(true) is a no-op: restore_active calls link_up on reconnect.
    }

    // ---

    /// Re-issue all pending streams onto a freshly reconnected session.
    async fn drain_pending(
        remote: &mut RemoteState,
        cb_tx: CallbackTx,
        arl: Arc<AggregateRateLimiter>,
    ) {
        // ---
        let session = match remote.session.as_ref() {
            Some(s) => s,
            None => return,
        };

        let uuids: Vec<Uuid> = remote.pending.keys().copied().collect();
        for uuid in uuids {
            if let Some(pending) = remote.pending.get(&uuid) {
                match Self::open_stream_on_session(
                    session,
                    pending,
                    cb_tx.clone(),
                    Arc::clone(&arl),
                )
                .await
                {
                    Ok(handle) => {
                        tracing::info!(%uuid, "pending stream re-issued after reconnect");
                        remote.pending.remove(&uuid);
                        remote.active_uplinks.insert(uuid, handle);
                    }
                    Err(e) => {
                        tracing::warn!(%uuid, "re-issue failed: {e} — leaving in pending");
                    }
                }
            }
        }
    }

    // ---

    /// Open one QUIC stream, write the framed `StreamHeader`, and spawn an
    /// uplink [`ActiveStream`] task to pipe bytes from the client TCP socket
    /// into the QUIC stream.
    ///
    /// Registers the stream with the [`AggregateRateLimiter`] to obtain an
    /// `alloc_rx` channel (capped mode) or `None` (uncapped), then passes
    /// both to [`ActiveStream::spawn_uplink`].
    ///
    /// # Why an associated function rather than `&self`?
    ///
    /// Both call sites hold a `MutexGuard<Option<RemoteState>>` when invoking
    /// this.  An `&self` method would require a second borrow of `self`
    /// overlapping the live guard — the borrow checker rejects this even though
    /// the accessed fields are disjoint.  Same reasoning applies to
    /// [`Self::drain_pending`].
    async fn open_stream_on_session(
        session: &QueLaySessionPtr,
        pending: &PendingStream,
        cb_tx: CallbackTx,
        arl: Arc<AggregateRateLimiter>,
    ) -> anyhow::Result<super::UplinkHandle> {
        // ---
        let mut stream = session.open_stream(pending.priority).await?;

        let header = StreamHeader {
            uuid: pending.uuid,
            priority: match pending.priority {
                Priority::C2I => 64,
                Priority::BulkTransfer => 0,
            },
            size_bytes: pending.info.size_bytes,
            attrs: pending.info.attrs.clone(),
        };

        write_connect_header(&mut stream, &header).await?;

        // Register with ARL — get alloc_rx (capped) or None (uncapped),
        // plus head_offset/q atomics used by the ARL timer to compute backlog (T - Q).
        let (alloc_rx, head_offset, q_atomic) = arl.register(pending.uuid, pending.priority).await;

        let handle = ActiveStream::spawn_uplink(
            pending.uuid,
            pending.info.clone(),
            pending.priority,
            stream,
            cb_tx,
            UplinkContext {
                alloc_rx,
                head_offset,
                q_atomic,
                arl: Arc::clone(&arl),
            },
        )
        .await?;

        Ok(handle)
    }

    // ---

    /// On reconnect: open a fresh QUIC stream for every in-flight uplink and
    /// deliver it via the pump's channel so it can replay and resume.
    ///
    /// Writes a [`ReconnectHeader`] (not a `StreamHeader`) with `replay_from`
    /// taken from the handle's spool `bytes_acked` — so the receiver knows
    /// where the sender's replay starts.
    ///
    /// Handles whose pump has already exited are pruned.
    async fn restore_active(remote: &mut RemoteState) {
        // ---
        let session = match remote.session.as_ref() {
            Some(s) => s,
            None => return,
        };

        let uuids: Vec<Uuid> = remote.active_uplinks.keys().copied().collect();

        for uuid in uuids {
            let handle = match remote.active_uplinks.get(&uuid) {
                Some(h) => h,
                None => continue,
            };

            let replay_from = handle.bytes_acked().await;

            match session.open_stream(handle.priority).await {
                Ok(mut stream) => {
                    let reconnect_hdr = ReconnectHeader { uuid, replay_from };
                    if let Err(e) = write_reconnect_header(&mut stream, &reconnect_hdr).await {
                        tracing::warn!(%uuid, "restore_active: reconnect header write failed: {e}");
                        continue;
                    }
                    if handle.stream_tx.try_send(stream).is_err() {
                        tracing::debug!(%uuid, "restore_active: pump already exited, pruning");
                        remote.active_uplinks.remove(&uuid);
                    } else {
                        tracing::info!(%uuid, replay_from, "restore_active: fresh stream delivered to pump");
                    }
                }
                Err(e) => {
                    tracing::warn!(%uuid, "restore_active: open_stream failed: {e}");
                }
            }
        }

        // Prune handles whose pump exited cleanly.
        remote.active_uplinks.retain(|uuid, h| {
            let alive = !h.stream_tx.is_closed();
            if !alive {
                tracing::debug!(%uuid, "restore_active: pruning completed uplink");
            }
            alive
        });
    }
}

// ---------------------------------------------------------------------------
// SessionManagerHandle
// ---------------------------------------------------------------------------

/// Cheap clone handle used by `Agent` to submit commands without holding a
/// lock across await points.
#[derive(Clone)]
pub struct SessionManagerHandle {
    // ---
    inner: Arc<SessionManager>,
}

// ---

impl SessionManagerHandle {
    // ---

    /// Wrap an `Arc<SessionManager>` for use by `Agent`.
    pub fn new(sm: Arc<SessionManager>) -> Self {
        Self { inner: sm }
    }

    // ---

    /// Forward a stream start request to the session manager.
    pub async fn stream_start(&self, uuid: Uuid, info: StreamInfo, priority: Priority) {
        self.inner.stream_start(uuid, info, priority).await;
    }

    // -----------------------------------------------------------------------
    // Test / debug — disabled in production builds
    // -----------------------------------------------------------------------

    /// Simulate a link failure (`enabled = false`) or allow reconnect
    /// (`enabled = true`).
    pub async fn link_enable(&self, enabled: bool) {
        self.inner.link_enable(enabled).await;
    }
}
