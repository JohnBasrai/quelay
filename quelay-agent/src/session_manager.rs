//! [`SessionManager`] — owns the remote session and drives the reconnection loop.
//!
//! # Current scope (single remote)
//!
//! Today `SessionManager` manages exactly one remote peer, held in
//! `self.remote: Option<RemoteState>`.  The `RemoteState` struct is already
//! the unit of per-peer state so that the future expansion is a mechanical
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
//! QUIC session drops, the loop retries with exponential back-off until it
//! re-establishes, then re-drains the pending UUID map.
//!
//! # Spool (stubbed)
//!
//! When `LinkState` transitions to `Failed` the spool path is logged and a
//! `TODO` marker is left.  The actual disk I/O will be added in the next
//! iteration.

use std::collections::HashMap;
use std::path::PathBuf;
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

use super::{write_header, CallbackTx, StreamHeader};

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
    // --
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
/// Future: when spool-to-disk is added, streams that were partially sent
/// before the link dropped will carry a `bytes_acked` watermark here.
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
}

// ---

impl RemoteState {
    // ---

    fn new(session: QueLaySessionPtr) -> Self {
        // ---

        Self {
            session: Some(session),
            pending: HashMap::new(),
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

    /// Spool directory.  Data is written here when the link is `Failed`.\
    /// Stubbed: directory is created but no data is written yet.
    spool_dir: PathBuf,

    /// Sender handle to the [`CallbackAgent`] thread.
    ///
    /// Cloned into each [`ActiveStream`] task so it can fire lifecycle
    /// events (stream_started, stream_done, stream_failed) directly.
    cb_tx: CallbackTx,

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
        spool_dir: PathBuf,
        cb_tx: CallbackTx,
    ) -> Self {
        // ---

        let remote = RemoteState::new(session);
        Self {
            remote: Arc::new(Mutex::new(Some(remote))),
            transport_cfg: Mutex::new(transport_cfg),
            link_state,
            spool_dir,
            cb_tx,
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
                // Remote slot not yet populated (shouldn't happen after init,
                // but handle it gracefully).
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
                match Self::open_stream_on_session(session, &pending, self.cb_tx.clone()).await {
                    Ok(()) => {
                        tracing::info!(%uuid, "stream opened on live session");
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
    /// On recovery, drains `pending`.
    ///
    /// Also spawns the inbound accept loop, which runs concurrently and is
    /// re-armed via `session_restored` after each reconnect.
    pub async fn run(self: Arc<Self>) {
        // ---
        // Obtain the initial session's link state receiver.
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

                let mut guard = self.remote.lock().await;
                if let Some(remote) = guard.as_mut() {
                    state_rx = new_session.link_state_rx();
                    remote.session = Some(new_session);
                    *self.link_state.lock().await = LinkState::Normal;
                    tracing::info!("session restored — draining pending queue");
                    Self::drain_pending(remote, self.cb_tx.clone()).await;
                    // Re-arm the accept loop on the new session.
                    self.session_restored.notify_one();
                }
            }
        }
    }

    // ---

    /// Inbound accept loop — runs as a sibling task to [`run`].
    ///
    /// Loops on `accept_stream()` and spawns a downlink [`ActiveStream`] for
    /// each inbound QUIC stream.  When the session fails `accept_stream()`
    /// returns an error; the loop then waits on `session_restored` before
    /// resuming with the new session.
    async fn accept_loop(self: Arc<Self>) {
        // ---
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

            match session.accept_stream().await {
                Ok(stream) => {
                    tracing::info!("downlink: inbound QUIC stream accepted");
                    let cb_tx = self.cb_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = super::ActiveStream::spawn_downlink(stream, cb_tx).await {
                            tracing::warn!("downlink: spawn_downlink failed: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept_stream error ({e}) — waiting for session restore");
                    self.session_restored.notified().await;
                }
            }
        }
    }

    // ---

    /// Called when the link transitions to `Failed`.
    ///
    /// Clears the dead session from `RemoteState`.
    /// STUB: spool-to-disk will be added in the next iteration.
    async fn on_link_failed(&self) {
        // ---
        tracing::warn!("link failed — clearing dead session");

        let mut guard = self.remote.lock().await;
        if let Some(remote) = guard.as_mut() {
            remote.session = None;
        }

        // TODO(spool): write pending stream data to self.spool_dir/<uuid>/
        //   For each entry in remote.pending that has bytes_acked < size_bytes,
        //   open spool_dir/<uuid>.part and begin writing received bytes.
        tracing::warn!(
            spool_dir = %self.spool_dir.display(),
            "STUB: spool-to-disk not yet implemented — in-flight data may be lost"
        );
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
                // The `listen()` accept loop is already running on the endpoint
                // from startup.  We just wait for the next incoming connection.
                let session = sess_rx
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("accept channel closed — endpoint shut down"))?;
                Ok(Arc::new(session))
            }
        }
    }

    // ---

    /// Re-issue all pending streams onto a freshly reconnected session.
    async fn drain_pending(remote: &mut RemoteState, cb_tx: CallbackTx) {
        // ---
        let session = match remote.session.as_ref() {
            Some(s) => s,
            None => return,
        };

        let uuids: Vec<Uuid> = remote.pending.keys().copied().collect();
        for uuid in uuids {
            if let Some(pending) = remote.pending.get(&uuid) {
                match Self::open_stream_on_session(session, pending, cb_tx.clone()).await {
                    Ok(()) => {
                        tracing::info!(%uuid, "pending stream re-issued after reconnect");
                        remote.pending.remove(&uuid);
                    }
                    Err(e) => {
                        tracing::warn!(%uuid, "re-issue failed: {e} — leaving in pending");
                    }
                }
            }
        }
    }

    // ---

    /// Open one QUIC stream, write the framed header, open an ephemeral TCP
    /// listener, fire `stream_started` callback, then spawn an [`ActiveStream`]
    /// uplink task to pipe bytes from the client TCP socket into the QUIC stream.
    ///
    /// # Why an associated function rather than `&self`?
    ///
    /// Both call sites hold a `MutexGuard<Option<RemoteState>>` when invoking
    /// this.  An `&self` method would require a second borrow of `self`
    /// overlapping the live guard — the borrow checker rejects this even though
    /// the accessed fields are disjoint.  Taking only the arguments actually
    /// needed sidesteps the conflict entirely.  Same reasoning applies to
    /// [`Self::drain_pending`].
    async fn open_stream_on_session(
        session: &QueLaySessionPtr,
        pending: &PendingStream,
        cb_tx: CallbackTx,
    ) -> anyhow::Result<()> {
        // ---
        let mut stream = session.open_stream(pending.priority).await?;

        let file_name = pending
            .info
            .attrs
            .get("filename")
            .cloned()
            .unwrap_or_else(|| pending.uuid.to_string());

        let header = StreamHeader {
            uuid: pending.uuid,
            priority: match pending.priority {
                Priority::C2I => 64,
                Priority::BulkTransfer => 0,
            },
            file_name,
            size_bytes: pending.info.size_bytes,
            attrs: pending.info.attrs.clone(),
        };

        write_header(&mut stream, &header).await?;

        // Spawn the uplink pump — takes ownership of the QUIC stream and the
        // CallbackTx; fires stream_started then pipes TCP → QUIC.
        super::ActiveStream::spawn_uplink(pending.uuid, pending.info.clone(), stream, cb_tx)
            .await?;

        Ok(())
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
}
