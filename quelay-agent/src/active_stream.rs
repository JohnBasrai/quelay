//! Per-stream actors for uplink and downlink data pumps.
//!
//! # Modes
//!
//! - **Uplink** (sender side): client connects to an ephemeral TCP port and
//!   writes bytes → agent pipes them through a [`SpoolBuffer`] into the QUIC
//!   stream.  The [`RateLimiter`] timer task reads directly from the spool and
//!   meters bytes onto the QUIC stream at the configured rate.
//!
//! - **Downlink** (receiver side): agent reads chunks from the QUIC stream
//!   and writes them to an ephemeral TCP port → client reads from it.
//!   Errors on the TCP write half are sent back as [`WormholeMsg::Error`].
//!
//! # Uplink spool
//!
//! The uplink maintains a [`SpoolBuffer`] — an in-memory ring buffer shared
//! between three actors:
//!
//! - **TCP reader**: reads from the ephemeral socket → `push` into the spool.
//!   Pauses when the spool is full; resumes when space is freed by an Ack.
//! - **Timer task** (inside `RateLimiter`): drains spool → QUIC stream.  On
//!   link failure the timer task rewinds to `A` (last acked byte) and waits
//!   for a fresh write half via `RateCmd::LinkUp`.
//! - **Ack task**: owns the QUIC read half exclusively, processes
//!   `WormholeMsg` feedback (Ack/Done/Error), advances `spool.bytes_acked`,
//!   and drives reconnect on stream errors.
//!
//! ## Three pointers
//!
//! ```text
//! spool [capacity: SPOOL_CAPACITY]
//! [..........................................................]
//!  ^               ^                                        ^
//!  A               Q                                        T
//!  bytes_acked     next_quic_write (timer task-local)    head (next TCP write)
//! ```
//!
//! - `T` advances as the TCP reader pushes bytes in.
//! - `Q` advances as the [`StreamPump`] drains bytes out.  Owned by `StreamPump`.
//! - `A` advances on `WormholeMsg::Ack { bytes_received }`.  Owned by ack task.
//! - On link down: timer task rewinds `Q = A`, TCP reader keeps filling.
//! - On reconnect: timer task replays `A..T` on the new stream, then resumes.
//! - Buffer full (`T − A == SPOOL_CAPACITY`): TCP reader pauses (back-pressure).
//!
//! # Downlink reconnect
//!
//! The downlink pump holds a `stream_rx` channel.  On QUIC read error it
//! waits for the `accept_loop` to deliver a fresh stream (identified by a
//! [`ReconnectHeader`]).  It resumes reading chunks using its own
//! `bytes_written` counter for duplicate detection — chunks with
//! `stream_offset < bytes_written` are silently skipped.  If the sender's
//! `replay_from > bytes_written` there is an unrecoverable gap and the
//! stream is failed.
//!
//! # Lifecycle
//!
//! Both modes fire [`CallbackCmd::StreamStarted`] after the ephemeral port
//! is open, then [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`]
//! at the end.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex, Notify};
use uuid::Uuid;

// ---

use quelay_domain::{Priority, QueLayStreamPtr, StreamInfo};
use quelay_thrift::FailReason;

// ---

use super::{
    // ---
    read_chunk,
    read_wormhole_msg,
    write_wormhole_msg,
    AggregateRateLimiter,
    AllocTicket,
    CallbackCmd,
    CallbackTx,
    Chunk,
    RateCmd,
    RateLimiter,
    WormholeMsg,
    ACK_INTERVAL,
    CHUNK_SIZE,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// In-memory spool capacity per uplink stream.
///
/// Must be large enough to absorb bytes in-flight inside QUIC plus any bytes
/// buffered during a link outage before TCP back-pressure kicks in.
///
/// TODO: wire to per-stream or global config.
const SPOOL_CAPACITY: usize = 1024 * 1024; // 1 MiB

/// How often to fire [`CallbackCmd::StreamProgress`] on both uplink and
/// downlink, measured in bytes transferred.  Fires once per interval boundary
/// crossed, so a 10 MiB transfer at this default produces ~10 callbacks.
const PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024; // 1 MiB

// ---------------------------------------------------------------------------
// SpoolBuffer
// ---------------------------------------------------------------------------

/// Shared ring-buffer state between the TCP reader task, the ack task, and
/// the `RateLimiter` timer task.
///
/// All three logical pointers are absolute byte offsets into the stream.
/// The timer task's `Q` pointer is task-local and not stored here.
pub(crate) struct SpoolBuffer {
    // ---
    buf: VecDeque<u8>,

    /// Absolute offset of the oldest byte still retained (`A`).
    /// Bytes below this offset have been acknowledged by the receiver.
    pub(crate) bytes_acked: u64,

    /// Absolute offset of the next byte the TCP reader will write (`T`).
    /// Invariant: `head − bytes_acked == buf.len()`.
    ///
    /// Set to `u64::MAX` as an EOF sentinel when the TCP reader closes.
    pub(crate) head: u64,
}

// ---

impl SpoolBuffer {
    // ---

    pub(crate) fn new() -> Self {
        // ---
        Self {
            buf: VecDeque::with_capacity(SPOOL_CAPACITY),
            bytes_acked: 0,
            head: 0,
        }
    }

    // ---

    pub(crate) fn available(&self) -> usize {
        SPOOL_CAPACITY - self.buf.len()
    }

    // ---

    /// Absolute offset of the byte one past the last byte in the spool (`T`),
    /// treating the EOF sentinel as opaque.  Used by the timer task to check
    /// whether new data has arrived since `Q`.
    ///
    /// When `head == u64::MAX` (EOF), returns `bytes_acked + buf.len()` so
    /// the timer task can drain the remaining bytes then detect the sentinel.
    pub(crate) fn head_offset(&self) -> u64 {
        // ---
        if self.head == u64::MAX {
            self.bytes_acked + self.buf.len() as u64
        } else {
            self.head
        }
    }

    // ---

    /// Append `data`, advancing `T`.  Caller must ensure `data.len() <= available()`.
    pub(crate) fn push(&mut self, data: &[u8]) {
        // ---
        debug_assert!(data.len() <= self.available());
        self.buf.extend(data.iter().copied());
        self.head += data.len() as u64;
    }

    // ---

    /// Advance `A` to `up_to`, freeing buffer space.  Called on `WormholeMsg::Ack`.
    pub(crate) fn ack(&mut self, up_to: u64) {
        // ---
        if up_to <= self.bytes_acked {
            return;
        }
        let n = ((up_to - self.bytes_acked) as usize).min(self.buf.len());
        self.buf.drain(..n);
        self.bytes_acked += n as u64;
    }

    // ---

    /// Contiguous slice of spool bytes starting at absolute offset `from`.
    ///
    /// May return only the first VecDeque segment if the data wraps the ring;
    /// callers advance `Q` by the returned slice length and call again.
    pub(crate) fn slice_from(&self, from: u64) -> &[u8] {
        // ---
        debug_assert!(from >= self.bytes_acked);
        debug_assert!(from <= self.head || self.head == u64::MAX);
        let start = (from - self.bytes_acked) as usize;
        let (first, second) = self.buf.as_slices();
        if start < first.len() {
            &first[start..]
        } else {
            &second[start - first.len()..]
        }
    }
}

// ---------------------------------------------------------------------------
// UplinkContext
// ---------------------------------------------------------------------------

/// Arguments derived from [`AggregateRateLimiter::register`] for one uplink
/// stream.  Bundled so [`ActiveStream::spawn_uplink`] stays within the
/// clippy argument-count limit and the three fields always travel together.
pub(crate) struct UplinkContext {
    // ---
    /// Allocation ticket receiver — `Some` in capped mode, `None` uncapped.
    pub alloc_rx: Option<mpsc::Receiver<AllocTicket>>,

    /// Backlog atomic written by [`StreamPump`] after each drain; read by
    /// the ARL timer before calling `DrrScheduler::schedule`.
    pub backlog: Arc<AtomicU64>,

    /// Shared ARL — deregistered by the [`AckTask`] when the stream exits.
    pub arl: Arc<AggregateRateLimiter>,
}

// ---------------------------------------------------------------------------
// UplinkHandle
// ---------------------------------------------------------------------------

/// Held by `SessionManager` for each active uplink stream.
///
/// On reconnect, `SessionManager` opens a new QUIC stream, writes a
/// [`ReconnectHeader`] with `replay_from = spool.bytes_acked`, then sends
/// the stream via `stream_tx` so the ack task can split it and deliver the
/// write half to the pump via `RateCmd::LinkUp`.
pub struct UplinkHandle {
    // ---
    /// Stable stream identity.  Used by `SessionManager` to deregister the
    /// stream from the [`AggregateRateLimiter`] when the pump exits.
    pub uuid: Uuid,

    /// Deliver a fresh `QueLayStreamPtr` here after reconnect.
    /// Drop the sender to permanently fail the stream.
    pub stream_tx: mpsc::Sender<QueLayStreamPtr>,

    /// Shared spool — read `bytes_acked` to populate `ReconnectHeader::replay_from`.
    spool: Arc<Mutex<SpoolBuffer>>,

    /// Retained so `SessionManager` can re-open the stream on a new session.
    pub info: StreamInfo,

    /// Priority for re-opening.
    pub priority: Priority,

    /// Rate limiter link-down notifier.  `None` when uncapped.
    ///
    /// Call `notify_link_down()` when `link_enable(false)` fires so the
    /// [`StreamPump`] rewinds `Q = A` and waits for a fresh write half.
    link_down_tx: Option<mpsc::Sender<RateCmd>>,
}

impl UplinkHandle {
    // ---
    /// Return the current spool `A` pointer (last byte acknowledged by receiver).
    /// Used by `session_manager::restore_active` to populate `ReconnectHeader`.
    pub async fn bytes_acked(&self) -> u64 {
        self.spool.lock().await.bytes_acked
    }

    // ---

    /// Signal the rate limiter timer task that the link is going down.
    ///
    /// The timer task rewinds `Q = A` and waits for a fresh write half.
    /// No-op when uncapped.
    pub fn notify_link_down(&self) {
        if let Some(tx) = &self.link_down_tx {
            let _ = tx.try_send(RateCmd::LinkDown);
        }
    }
}

// ---------------------------------------------------------------------------
// DownlinkHandle
// ---------------------------------------------------------------------------

/// Held by `SessionManager` for each active downlink stream.
///
/// On reconnect, the `accept_loop` receives an `OP_RECONNECT` stream,
/// looks up the handle by UUID, and delivers the fresh stream via `stream_tx`.
pub struct DownlinkHandle {
    // ---
    /// Deliver a fresh `QueLayStreamPtr` here after reconnect.
    /// Drop the sender to permanently fail the stream.
    pub stream_tx: mpsc::Sender<QueLayStreamPtr>,
    /// Live count of bytes the pump has written to the TCP client.
    /// Updated atomically by `run_downlink`; read by `accept_loop` to
    /// validate `replay_from` before delivering a reconnect stream.
    pub bytes_written: Arc<AtomicU64>,
}

// ---------------------------------------------------------------------------
// ActiveStream  (downlink only — uplink uses free functions below)
// ---------------------------------------------------------------------------

pub(crate) struct ActiveStream {
    // ---
    uuid: Uuid,
    _info: StreamInfo,
    cb_tx: CallbackTx,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

impl ActiveStream {
    // ---

    /// Spawn an uplink (sender-side) pump.
    ///
    /// Binds an ephemeral TCP listener, fires [`CallbackCmd::StreamStarted`],
    /// then spawns:
    /// - a TCP-reader task that fills the [`SpoolBuffer`], and
    /// - an ack task that owns the QUIC read half, processes `WormholeMsg`
    ///   feedback, and drives reconnect signalling to the [`StreamPump`] task.
    ///
    /// In capped mode a [`StreamPump`] (spawned inside [`RateLimiter::new`])
    /// reads directly from the spool, driven by [`AllocTicket`]s from the
    /// [`AggregateRateLimiter`].  In uncapped mode the write half is held
    /// directly with no metering.
    ///
    /// Returns an [`UplinkHandle`] for `SessionManager` to use on reconnect.
    pub async fn spawn_uplink(
        // ---
        uuid: Uuid,
        info: StreamInfo,
        priority: Priority,
        quic: QueLayStreamPtr,
        cb_tx: CallbackTx,
        ctx: UplinkContext,
    ) -> anyhow::Result<UplinkHandle> {
        // ---
        let UplinkContext {
            alloc_rx,
            backlog,
            arl,
        } = ctx;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tracing::info!(%uuid, port, "uplink: ephemeral TCP port open, firing stream_started");

        cb_tx
            .send(CallbackCmd::StreamStarted {
                uuid,
                info: info.clone(),
                port,
            })
            .await;

        let (stream_tx, stream_rx) = mpsc::channel::<QueLayStreamPtr>(4);

        let spool = Arc::new(Mutex::new(SpoolBuffer::new()));
        let data_ready = Arc::new(Notify::new());
        let space_ready = Arc::new(Notify::new());

        // Split the initial QUIC stream.
        let (initial_rx, initial_tx) = tokio::io::split(quic);

        // Construct the rate limiter; in capped mode the StreamPump is
        // spawned inside new() and driven by alloc_rx tickets from the ARL.
        let capped = alloc_rx.is_some();
        tracing::info!(%uuid, capped, "uplink: rate limiter mode");

        let q_atomic = Arc::<AtomicU64>::new(AtomicU64::new(0));

        let rate_limiter = RateLimiter::new(
            initial_tx,
            alloc_rx,
            Arc::clone(&spool),
            Arc::clone(&backlog),
            Arc::clone(&q_atomic),
        );
        let link_down_tx = rate_limiter.link_down_tx_clone();

        let handle = UplinkHandle {
            uuid,
            stream_tx,
            spool: Arc::clone(&spool),
            info: info.clone(),
            priority,
            link_down_tx,
        };

        let tcp_ctx = TcpReaderCtx {
            // ---
            listener,
            spool: Arc::clone(&spool),
            data_ready: Arc::clone(&data_ready),
            space_ready: Arc::clone(&space_ready),
            cb_tx: cb_tx.clone(),
            backlog: Arc::clone(&backlog),
            q_atomic: Arc::clone(&q_atomic),
        };

        tokio::spawn(run_tcp_reader(uuid, tcp_ctx));

        tokio::spawn(async move {
            // ---
            let ack_ctx = AckTaskCtx {
                spool,
                space_ready,
                data_ready,
                stream_rx,
                cb_tx,
                rate_limiter,
                size_bytes: info.size_bytes,
                initial_rx,
                arl,
            };

            match AckTask::new(uuid, ack_ctx).await {
                Some(task) => task.run().await,
                None => tracing::warn!(
                    %uuid,
                    "uplink: ack reader gone before initial stream delivered"
                ),
            }
        });

        Ok(handle)
    }

    // ---

    /// Spawn a downlink (receiver-side) pump for a new stream.
    ///
    /// Called by `accept_loop` after it has already read and decoded the
    /// [`StreamHeader`] from the inbound QUIC stream.  The stream is positioned
    /// immediately after the header — chunk data is next.
    ///
    /// 1. Binds an ephemeral TCP listener.
    /// 2. Fires [`CallbackCmd::StreamStarted`].
    /// 3. Spawns [`run_downlink`] which accepts one TCP client and pipes
    ///    QUIC chunks to it, handling reconnect via `stream_rx`.
    ///
    /// Returns a [`DownlinkHandle`] for `SessionManager` to use on reconnect.
    pub async fn spawn_downlink_from(
        // ---
        uuid: Uuid,
        info: StreamInfo,
        quic: QueLayStreamPtr,
        cb_tx: CallbackTx,
    ) -> anyhow::Result<DownlinkHandle> {
        // ---

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tracing::info!(%uuid, port, "downlink: ephemeral TCP port open, firing stream_started");

        cb_tx
            .send(CallbackCmd::StreamStarted {
                uuid,
                info: info.clone(),
                port,
            })
            .await;

        // Capacity 2: one slot for the initial stream, one for a reconnect
        // stream that may arrive before the pump drains the first entry.
        let (stream_tx, stream_rx) = mpsc::channel::<QueLayStreamPtr>(2);
        let _ = stream_tx.try_send(quic);

        let bytes_written = Arc::new(AtomicU64::new(0));

        let handle = DownlinkHandle {
            stream_tx,
            bytes_written: Arc::clone(&bytes_written),
        };

        let actor = ActiveStream {
            uuid,
            _info: info,
            cb_tx,
        };

        tokio::spawn(async move { actor.run_downlink(listener, stream_rx, bytes_written).await });

        Ok(handle)
    }

    // ---

    /// Deliver a fresh QUIC stream to an existing downlink pump after reconnect.
    ///
    /// Called by `accept_loop` when it receives an `OP_RECONNECT` stream.
    /// Validates that `replay_from <= bytes_written` before delivering;
    /// if the sender has freed spool data the receiver never saw the gap is
    /// unrecoverable and the handle's sender is dropped to fail the stream.
    ///
    /// The `bytes_written` value must be read from the downlink pump's
    /// internal state; it is passed in by the caller which holds the handle.
    pub fn deliver_reconnect_stream(
        // ---
        handle: &DownlinkHandle,
        replay_from: u64,
        new_stream: QueLayStreamPtr,
        uuid: Uuid,
    ) {
        // ---
        let bytes_written = handle.bytes_written.load(Ordering::Acquire);
        if replay_from > bytes_written {
            tracing::error!(
                %uuid,
                replay_from,
                bytes_written,
                "downlink: reconnect gap — sender freed spool data receiver never saw; failing stream",
            );
            drop(new_stream);
            return;
        }

        if handle.stream_tx.try_send(new_stream).is_err() {
            tracing::warn!(%uuid, "downlink: pump already exited or channel full on reconnect");
        } else {
            tracing::info!(%uuid, replay_from, bytes_written, "downlink: fresh stream delivered to pump");
        }
    }

    // ---

    async fn run_downlink(
        // ---
        self,
        listener: TcpListener,
        mut stream_rx: mpsc::Receiver<QueLayStreamPtr>,
        bytes_written_atomic: Arc<AtomicU64>,
    ) {
        // ---
        let uuid = self.uuid;

        // Accept the initial QUIC stream from the channel.
        let mut quic = match stream_rx.recv().await {
            Some(s) => s,
            None => {
                tracing::warn!(%uuid, "downlink: stream handle dropped before TCP accept");
                return;
            }
        };

        let mut tcp = match listener.accept().await {
            Ok((tcp, addr)) => {
                tracing::info!(%uuid, %addr, "downlink: client connected");
                tcp
            }
            Err(e) => {
                tracing::warn!(%uuid, "downlink: accept failed: {e}");
                let _ = write_wormhole_msg(
                    &mut quic,
                    &WormholeMsg::Error {
                        code: FailReason::UNKNOWN.0 as u32,
                        reason: format!("TCP accept failed: {e}"),
                    },
                )
                .await;
                self.cb_tx
                    .send(CallbackCmd::StreamFailed {
                        uuid,
                        code: FailReason::UNKNOWN,
                        reason: format!("TCP accept failed: {e}"),
                    })
                    .await;
                return;
            }
        };

        // Initialise from the atomic so that if a reconnect stream arrives
        // before the pump starts (channel capacity 2), the gap check uses
        // the correct baseline rather than zero.
        let mut bytes_written = bytes_written_atomic.load(Ordering::Acquire);
        let mut last_acked = bytes_written;
        let mut next_progress: u64 = PROGRESS_INTERVAL_BYTES;
        let size_bytes = self._info.size_bytes;

        // Split the initial QUIC stream.  We need separate halves so that
        // on clean EOF we can shutdown the write half (sending Done + FIN)
        // without closing the read half, and without triggering a stream error
        // on the sender's ack task.  On reconnect we re-split the new stream.
        let (mut quic_rx, mut quic_tx) = tokio::io::split(quic);

        loop {
            match read_chunk(&mut quic_rx).await {
                Ok(None) => {
                    // Sender closed its write half (QUIC FIN) — all chunks
                    // delivered.  Stop reading; write Done and close our write
                    // half cleanly so the sender's ack task reads Done then
                    // sees a clean EOF on the read half it holds.
                    tracing::info!(
                        %uuid,
                        bytes = bytes_written,
                        "downlink: QUIC EOF, transfer complete",
                    );
                    let _ = write_wormhole_msg(&mut quic_tx, &WormholeMsg::Done).await;
                    let _ = quic_tx.shutdown().await;
                    // quic_rx dropped here — no more reading needed.
                    self.cb_tx
                        .send(CallbackCmd::StreamDone {
                            uuid,
                            bytes: bytes_written,
                        })
                        .await;
                    return;
                }

                Ok(Some(Chunk {
                    stream_offset,
                    payload,
                })) => {
                    // Duplicate: sender replayed a chunk already delivered.
                    // Silently skip — bytes_written is ground truth.
                    if stream_offset + payload.len() as u64 <= bytes_written {
                        tracing::debug!(
                            %uuid,
                            stream_offset,
                            bytes_written,
                            "downlink: duplicate chunk — skipping",
                        );
                        continue;
                    }

                    // Partial overlap: skip already-delivered prefix.
                    let skip = if stream_offset < bytes_written {
                        (bytes_written - stream_offset) as usize
                    } else {
                        0
                    };

                    // Gap: offset ahead of bytes_written — unrecoverable.
                    if stream_offset > bytes_written {
                        tracing::error!(
                            %uuid,
                            stream_offset,
                            bytes_written,
                            "downlink: chunk offset gap — logic error in pump",
                        );
                        let _ = write_wormhole_msg(
                            &mut quic_tx,
                            &WormholeMsg::Error {
                                code: FailReason::UNKNOWN.0 as u32,
                                reason: format!(
                                    "chunk gap: expected {bytes_written}, got {stream_offset}"
                                ),
                            },
                        )
                        .await;
                        self.cb_tx
                            .send(CallbackCmd::StreamFailed {
                                uuid,
                                code: FailReason::UNKNOWN,
                                reason: "chunk offset gap — pump logic error".into(),
                            })
                            .await;
                        return;
                    }

                    // Write (possibly trimmed) payload to the client socket.
                    let to_write = &payload[skip..];
                    if !to_write.is_empty() {
                        if let Err(e) = tcp.write_all(to_write).await {
                            tracing::warn!(%uuid, "downlink: TCP write error: {e}");
                            let _ = write_wormhole_msg(
                                &mut quic_tx,
                                &WormholeMsg::Error {
                                    code: FailReason::UNKNOWN.0 as u32,
                                    reason: format!("TCP write error: {e}"),
                                },
                            )
                            .await;
                            self.cb_tx
                                .send(CallbackCmd::StreamFailed {
                                    uuid,
                                    code: FailReason::UNKNOWN,
                                    reason: format!("TCP write error: {e}"),
                                })
                                .await;
                            return;
                        }
                        bytes_written += to_write.len() as u64;
                        bytes_written_atomic.store(bytes_written, Ordering::Release);

                        while bytes_written >= next_progress {
                            let percent =
                                size_bytes.map(|total| bytes_written as f64 / total as f64 * 100.0);
                            self.cb_tx
                                .send(CallbackCmd::StreamProgress {
                                    uuid,
                                    bytes: bytes_written,
                                    percent,
                                })
                                .await;
                            next_progress += PROGRESS_INTERVAL_BYTES;
                        }
                    }

                    // Periodic ack so the sender can advance its spool `A`.
                    if bytes_written - last_acked >= ACK_INTERVAL {
                        last_acked = bytes_written;
                        let _ = write_wormhole_msg(
                            &mut quic_tx,
                            &WormholeMsg::Ack {
                                bytes_received: bytes_written,
                            },
                        )
                        .await;
                        tracing::debug!(
                            %uuid,
                            bytes_received = bytes_written,
                            "downlink: sent ack",
                        );
                    }
                }

                Err(e) => {
                    // QUIC read error — link went down.  Wait for a fresh
                    // stream from the accept_loop via stream_rx.
                    tracing::warn!(%uuid, "downlink: QUIC read error: {e} — waiting for reconnect");

                    match stream_rx.recv().await {
                        Some(new_stream) => {
                            tracing::info!(
                                %uuid,
                                bytes_written,
                                "downlink: fresh stream received — resuming",
                            );
                            // Re-split the new stream; old halves are dropped.
                            let (new_rx, new_tx) = tokio::io::split(new_stream);
                            quic_rx = new_rx;
                            quic_tx = new_tx;
                            // Continue the loop: duplicate detection handles
                            // any replayed chunks via bytes_written.
                        }
                        None => {
                            tracing::warn!(%uuid, "downlink: stream handle dropped — failing");
                            self.cb_tx
                                .send(CallbackCmd::StreamFailed {
                                    uuid,
                                    code: FailReason::LINK_FAILED,
                                    reason: "link failed permanently".into(),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Uplink sub-tasks (free async functions)
// ---------------------------------------------------------------------------

/// Reads from the client TCP socket into the spool.
///
/// Pauses when the spool is full (back-pressure to client).
/// Wakes when the ack task signals `space_ready` after an incoming Ack.
/// Signals EOF by setting `spool.head = u64::MAX`.
/// Constructor context for the uplink TCP reader task.
///
/// Bundles parameters so call sites stay readable.
struct TcpReaderCtx {
    // ---
    listener: TcpListener,
    spool: Arc<Mutex<SpoolBuffer>>,
    data_ready: Arc<Notify>,
    space_ready: Arc<Notify>,
    cb_tx: CallbackTx,
    backlog: Arc<AtomicU64>,
    q_atomic: Arc<AtomicU64>,
}

async fn run_tcp_reader(
    // ---
    uuid: Uuid,
    ctx: TcpReaderCtx,
) {
    // ---
    let TcpReaderCtx {
        listener,
        spool,
        data_ready,
        space_ready,
        cb_tx,
        backlog,
        q_atomic,
    } = ctx;

    // ---
    let mut tcp = match listener.accept().await {
        Ok((tcp, addr)) => {
            tracing::info!(%uuid, %addr, "uplink: client connected");
            tcp
        }
        Err(e) => {
            tracing::warn!(%uuid, "uplink: TCP accept failed: {e}");
            cb_tx
                .send(CallbackCmd::StreamFailed {
                    uuid,
                    code: FailReason::UNKNOWN,
                    reason: format!("TCP accept failed: {e}"),
                })
                .await;
            return;
        }
    };

    // Read at most CHUNK_SIZE bytes at a time so spool entries align to
    // chunk boundaries — makes offset arithmetic in the timer task exact.
    let mut tcp_buf = vec![0u8; CHUNK_SIZE];

    let mut first_read_logged = false;

    loop {
        // Block until the spool has room for one full chunk.
        let mut logged_full = false;
        loop {
            if spool.lock().await.available() >= CHUNK_SIZE {
                break;
            }
            if !logged_full {
                tracing::debug!(
                    %uuid,
                    spool_capacity = SPOOL_CAPACITY,
                    "uplink: spool full — pausing TCP reader",
                );
                logged_full = true;
            } else {
                tracing::trace!(%uuid, "uplink: spool still full — waiting");
            }
            space_ready.notified().await;
        }
        if logged_full {
            tracing::info!(%uuid, "uplink: spool space available — resuming TCP reader");
        }

        match tcp.read(&mut tcp_buf).await {
            Ok(0) => {
                let mut s = spool.lock().await;
                s.head = u64::MAX; // EOF sentinel
                                   // Backlog: remaining unacked bytes still to be pumped.
                let q = q_atomic.load(Ordering::Acquire);
                let b = s.head_offset().saturating_sub(q);
                drop(s);

                tracing::info!(%uuid, %b, %q, "uplink: TCP EOF from client");

                backlog.store(b, Ordering::Release);
                data_ready.notify_one();
                return;
            }
            Ok(n) => {
                let mut s = spool.lock().await;
                s.push(&tcp_buf[..n]);
                let q = q_atomic.load(Ordering::Acquire);
                let b = s.head_offset().saturating_sub(q);
                drop(s);

                if !first_read_logged {
                    tracing::info!(%uuid, bytes = n, "uplink: first TCP bytes received");
                    first_read_logged = true;
                } else {
                    tracing::debug!(%uuid, backlog = %b, %q, bytes = n, "uplink: TCP bytes received");
                }

                backlog.store(b, Ordering::Release);
                data_ready.notify_one();
            }
            Err(e) => {
                tracing::warn!(%uuid, "uplink: TCP read error: {e}");
                cb_tx
                    .send(CallbackCmd::StreamFailed {
                        uuid,
                        code: FailReason::SENDER_CLOSED,
                        reason: format!("TCP read error: {e}"),
                    })
                    .await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AckMsg  (ack-reader sub-task → AckTask main loop)
// ---------------------------------------------------------------------------

enum AckMsg {
    Ack { bytes_received: u64 },
    Done,
    Error { code: u32, reason: String },
    StreamError(String),
}

// ---------------------------------------------------------------------------
// AckTask
// ---------------------------------------------------------------------------

/// Owns the QUIC read half for one uplink stream lifetime.
///
/// Processes `WormholeMsg` feedback from the receiver and drives reconnect
/// signalling to the `RateLimiter` timer task.  Spawned as a tokio task by
/// [`spawn_uplink`]; not publicly visible.
struct AckTask {
    // ---
    uuid: Uuid,
    spool: Arc<Mutex<SpoolBuffer>>,
    space_ready: Arc<Notify>,
    data_ready: Arc<Notify>,
    stream_rx: mpsc::Receiver<QueLayStreamPtr>,
    cb_tx: CallbackTx,
    rate_limiter: RateLimiter,
    size_bytes: Option<u64>,
    // ---
    /// Forwards new QUIC read halves to the ack-reader sub-task on reconnect.
    stream_ack_tx: mpsc::Sender<tokio::io::ReadHalf<QueLayStreamPtr>>,
    /// Messages from the ack-reader sub-task.
    ack_rx: mpsc::Receiver<AckMsg>,
    // ---
    bytes_sent: u64,
    next_progress: u64,
    /// True once `RateCmd::Finish` has been sent to the pump task.
    finish_sent: bool,
    /// Shared ARL — deregistered when this task exits (done or failed).
    arl: Arc<AggregateRateLimiter>,
}

// ---

/// Constructor context for [`AckTask`].
///
/// This keeps the public call site readable and avoids long argument lists.
struct AckTaskCtx {
    // ---
    spool: Arc<Mutex<SpoolBuffer>>,
    space_ready: Arc<Notify>,
    data_ready: Arc<Notify>,
    stream_rx: mpsc::Receiver<QueLayStreamPtr>,
    cb_tx: CallbackTx,
    rate_limiter: RateLimiter,
    size_bytes: Option<u64>,
    initial_rx: tokio::io::ReadHalf<QueLayStreamPtr>,
    arl: Arc<AggregateRateLimiter>,
}

impl AckTask {
    // ---

    /// Construct an `AckTask`, spawn the ack-reader sub-task, and deliver
    /// the initial QUIC read half to it.
    ///
    /// Returns `None` if the channel send fails (ack reader already gone).
    async fn new(uuid: Uuid, ctx: AckTaskCtx) -> Option<Self> {
        // ---
        let AckTaskCtx {
            spool,
            space_ready,
            data_ready,
            stream_rx,
            cb_tx,
            rate_limiter,
            size_bytes,
            initial_rx,
            arl,
        } = ctx;

        let (stream_ack_tx, ack_rx) = Self::spawn_reader(uuid);

        let task = Self {
            uuid,
            spool,
            space_ready,
            data_ready,
            stream_rx,
            cb_tx,
            rate_limiter,
            size_bytes,
            stream_ack_tx,
            ack_rx,
            bytes_sent: 0,
            next_progress: PROGRESS_INTERVAL_BYTES,
            finish_sent: false,
            arl,
        };

        if task.stream_ack_tx.send(initial_rx).await.is_err() {
            return None;
        }
        Some(task)
    }

    // ---

    /// Spawn the ack-reader sub-task.
    ///
    /// The sub-task owns the QUIC read half exclusively, forwarding
    /// `WormholeMsg` values as `AckMsg` until the stream errors.  On error
    /// it waits for a fresh read half via `stream_ack_rx` and resumes.
    ///
    /// Returns `(stream_ack_tx, ack_rx)` for the caller to store.
    fn spawn_reader(
        uuid: Uuid,
    ) -> (
        mpsc::Sender<tokio::io::ReadHalf<QueLayStreamPtr>>,
        mpsc::Receiver<AckMsg>,
    ) {
        // ---
        let (stream_ack_tx, mut stream_ack_rx) =
            mpsc::channel::<tokio::io::ReadHalf<QueLayStreamPtr>>(1);
        let (ack_tx, ack_rx) = mpsc::channel::<AckMsg>(32);

        tokio::spawn(async move {
            loop {
                let mut rx = match stream_ack_rx.recv().await {
                    Some(r) => r,
                    None => return, // AckTask dropped sender
                };
                loop {
                    let msg = match read_wormhole_msg(&mut rx).await {
                        Ok(WormholeMsg::Ack { bytes_received }) => {
                            tracing::debug!(%uuid, bytes_received, "uplink: receiver ack");
                            AckMsg::Ack { bytes_received }
                        }
                        Ok(WormholeMsg::Done) => {
                            tracing::info!(%uuid, "uplink: receiver done (ack reader)");
                            let _ = ack_tx.send(AckMsg::Done).await;
                            return;
                        }
                        Ok(WormholeMsg::Error { code, reason }) => {
                            tracing::warn!(%uuid, code, %reason, "uplink: receiver error (ack reader)");
                            let _ = ack_tx.send(AckMsg::Error { code, reason }).await;
                            return;
                        }
                        Err(e) => {
                            tracing::debug!(%uuid, "uplink: ACK READER stream error: {e}");
                            let _ = ack_tx.send(AckMsg::StreamError(e.to_string())).await;
                            break; // wait for a new read half
                        }
                    };
                    if ack_tx.send(msg).await.is_err() {
                        return; // AckTask dropped receiver
                    }
                }
            }
        });

        (stream_ack_tx, ack_rx)
    }

    // ---

    /// Main event loop.  Runs until the transfer completes or fails,
    /// then deregisters the stream from the [`AggregateRateLimiter`].
    async fn run(mut self) {
        // ---
        tracing::debug!(%self.uuid, "uplink: ack task starting");

        self.run_inner().await;

        tracing::debug!(%self.uuid, bytes_sent = self.bytes_sent, "uplink: deregistering from ARL");
        self.arl.deregister(self.uuid).await;
        tracing::debug!(%self.uuid, "uplink: ack task exit");
    }

    // ---

    async fn run_inner(&mut self) {
        // ---
        loop {
            // Always check for TCP EOF regardless of how we wake up.
            self.try_send_finish().await;

            // Select on ack messages OR data_ready (fires on TCP push + EOF).
            // The data_ready arm handles small files (< ACK_INTERVAL = 64 KiB)
            // that receive no acks, preventing an infinite wait for Finish.
            let msg = tokio::select! {
                msg = self.ack_rx.recv() => msg,
                _ = self.data_ready.notified() => {
                    self.try_send_finish().await;
                    continue;
                }
            };

            match msg {
                Some(AckMsg::Ack { bytes_received }) => self.on_ack(bytes_received).await,
                Some(AckMsg::Done) => {
                    tracing::info!(uuid = %self.uuid, bytes = self.bytes_sent, "uplink: receiver done");
                    self.cb_tx
                        .send(CallbackCmd::StreamDone {
                            uuid: self.uuid,
                            bytes: self.bytes_sent,
                        })
                        .await;
                    return;
                }
                Some(AckMsg::Error { code, reason }) => {
                    self.cb_tx
                        .send(CallbackCmd::StreamFailed {
                            uuid: self.uuid,
                            code: FailReason(code as i32),
                            reason,
                        })
                        .await;
                    return;
                }
                Some(AckMsg::StreamError(e)) => {
                    if !self.on_stream_error(e).await {
                        return;
                    }
                }
                None => {
                    self.on_reader_gone().await;
                    return;
                }
            }

            // After processing each message, check whether TCP EOF arrived.
            self.try_send_finish().await;
        }
    }

    // ---

    /// Process an `Ack` from the receiver: advance spool `A`, signal
    /// `space_ready`, fire progress callbacks.
    async fn on_ack(&mut self, bytes_received: u64) {
        // ---
        let prev = {
            let mut s = self.spool.lock().await;
            let prev = s.bytes_acked;
            s.ack(bytes_received);
            prev
        };
        if bytes_received > prev {
            self.bytes_sent = self.bytes_sent.max(bytes_received);
            self.space_ready.notify_one();

            while self.bytes_sent >= self.next_progress {
                let percent = self
                    .size_bytes
                    .map(|total| self.bytes_sent as f64 / total as f64 * 100.0);
                self.cb_tx
                    .send(CallbackCmd::StreamProgress {
                        uuid: self.uuid,
                        bytes: self.bytes_sent,
                        percent,
                    })
                    .await;
                self.next_progress += PROGRESS_INTERVAL_BYTES;
            }
        }
    }

    // ---

    /// Handle a QUIC read-half error: signal link-down, poll `data_ready`
    /// while waiting for a fresh stream so TCP EOF is not missed during
    /// the reconnect window, then deliver the new halves to the timer task
    /// and ack reader.
    ///
    /// Returns `true` to continue the loop, `false` on permanent failure.
    async fn on_stream_error(&mut self, e: String) -> bool {
        // ---
        tracing::warn!(uuid = %self.uuid,
                       "uplink: ACK READER stream error: {e} — waiting for reconnect");

        self.rate_limiter.link_down();

        // Check immediately in case TCP EOF arrived before the link went down.
        self.try_send_finish().await;

        let new_stream = loop {
            tokio::select! {
                _ = self.data_ready.notified() => {
                    self.try_send_finish().await;
                }
                result = self.stream_rx.recv() => {
                    break result;
                }
            }
        };

        match new_stream {
            Some(new_stream) => {
                let replay_from = self.spool.lock().await.bytes_acked;
                tracing::info!(uuid = %self.uuid, replay_from, "uplink: new stream — resuming");
                let (rx, tx) = tokio::io::split(new_stream);

                if self.stream_ack_tx.send(rx).await.is_err() {
                    tracing::warn!(uuid = %self.uuid, "uplink: ack reader gone on reconnect");
                    return false;
                }
                if let Err(e) = self.rate_limiter.link_up(tx).await {
                    tracing::warn!(uuid = %self.uuid, "uplink: link_up failed: {e}");
                    return false;
                }
                self.finish_sent = false;
                true
            }
            None => {
                tracing::warn!(uuid = %self.uuid,
                               "uplink: stream handle dropped — failing stream");
                self.cb_tx
                    .send(CallbackCmd::StreamFailed {
                        uuid: self.uuid,
                        code: FailReason::LINK_FAILED,
                        reason: "link failed permanently".into(),
                    })
                    .await;
                false
            }
        }
    }

    // ---

    /// Handle `ack_rx` channel closure (ack reader exited without Done/Error).
    async fn on_reader_gone(&mut self) {
        // ---
        tracing::warn!(uuid = %self.uuid, "uplink: ack reader exited without Done");
        self.try_send_finish().await;
        self.cb_tx
            .send(CallbackCmd::StreamFailed {
                uuid: self.uuid,
                code: FailReason::LINK_FAILED,
                reason: "ack reader exited without Done".into(),
            })
            .await;
    }

    // ---

    /// If the TCP reader has set the EOF sentinel and `Finish` hasn't been
    /// sent yet, send `RateCmd::Finish` to the timer task.
    async fn try_send_finish(&mut self) {
        // ---
        if self.finish_sent {
            return;
        }
        if self.spool.lock().await.head == u64::MAX {
            self.finish_sent = true;
            tracing::info!(uuid = %self.uuid, "uplink: TCP EOF — sending Finish to timer task");
            if let Err(e) = self.rate_limiter.finish().await {
                tracing::warn!(uuid = %self.uuid, "uplink: finish() failed: {e}");
            }
        }
    }
}
