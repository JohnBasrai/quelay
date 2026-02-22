//! [`ActiveStream`] — per-stream actor that owns the QUIC stream and its
//! paired ephemeral TCP socket to the local client.
//!
//! # Modes
//!
//! The same struct handles both directions of the wormhole:
//!
//! - **Uplink** (sender side): client connects to the ephemeral port and
//!   writes bytes → agent pipes them into the QUIC stream.  The QUIC read
//!   half carries [`WormholeMsg`] feedback from the receiver.
//!
//! - **Downlink** (receiver side): agent reads bytes from the QUIC stream
//!   and writes them to the ephemeral port → client reads from it.
//!   Errors on the TCP write half are sent back as [`WormholeMsg::Error`].
//!
//! # Uplink spool
//!
//! The uplink maintains a [`SpoolBuffer`] — an in-memory ring buffer shared
//! between two decoupled sub-tasks:
//!
//! - **TCP reader**: reads from the ephemeral socket → `push_back` into the
//!   spool.  Pauses when the spool is full; resumes when space is freed by
//!   an incoming Ack.
//! - **QUIC pump**: reads from the spool → writes to the QUIC stream.
//!   On link failure the pump pauses at `A` (last acked byte).
//!   On reconnect it replays `A..T` on a fresh QUIC stream, then resumes.
//!   The TCP reader continues filling the spool concurrently.
//!
//! ## Three pointers
//!
//! ```text
//! spool [capacity: SPOOL_CAPACITY]
//! [....................................................]
//!  ^               ^                                  ^
//!  A               Q                                  T
//!  bytes_acked     next_quic_write                    head (next TCP write)
//! ```
//!
//! - `T` advances as TCP reader pushes bytes in.
//! - `Q` advances as QUIC pump drains bytes out.  Local to pump task.
//! - `A` advances as `WormholeMsg::Ack { bytes_received }` arrives.
//! - On link down: pump sets `Q = A`, TCP reader keeps filling until full.
//! - On reconnect: pump replays `A..T`, TCP reader resumes immediately.
//! - Buffer full (`T - A == SPOOL_CAPACITY`): TCP reader pauses (back-pressure).
//!   If the link is still down when the buffer fills, the stream is failed.
//!
//! # Lifecycle
//!
//! Both modes fire [`CallbackCmd::StreamStarted`] after the ephemeral port
//! is open, then [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`]
//! at the end.

use std::collections::VecDeque;
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
    write_chunk,
    CallbackCmd,
    CallbackTx,
    Chunk,
    WormholeMsg,
    CHUNK_SIZE,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// In-memory spool capacity per uplink stream.
///
/// Must be large enough to absorb the data in-flight inside QUIC plus any
/// bytes buffered during a link outage before TCP back-pressure kicks in.
///
/// TODO: wire to per-stream or global config.
const SPOOL_CAPACITY: usize = 1024 * 1024; // 1 MiB

/// Send a `WormholeMsg::Ack` every this many bytes delivered to the client.
const ACK_INTERVAL: u64 = 64 * 1024; // 64 KiB

// ---------------------------------------------------------------------------
// SpoolBuffer
// ---------------------------------------------------------------------------

/// Shared ring-buffer state between the TCP reader task and the QUIC pump.
///
/// All three pointers are absolute byte offsets into the logical stream.
/// The pump's `Q` pointer is task-local and not stored here.
struct SpoolBuffer {
    // ---
    buf: VecDeque<u8>,

    /// Absolute offset of the oldest byte still retained (`A`).
    /// Bytes before this offset have been acknowledged by the receiver.
    bytes_acked: u64,

    /// Absolute offset of the next byte to be written by the TCP reader (`T`).
    /// Invariant: `head - bytes_acked == buf.len()`.
    ///
    /// Set to `u64::MAX` as a sentinel when the TCP reader reaches EOF.
    head: u64,
}

// ---

impl SpoolBuffer {
    // ---

    fn new() -> Self {
        // ---
        Self {
            buf: VecDeque::with_capacity(SPOOL_CAPACITY),
            bytes_acked: 0,
            head: 0,
        }
    }

    // ---

    fn available(&self) -> usize {
        SPOOL_CAPACITY - self.buf.len()
    }

    // ---

    /// Append `data`, advancing `T`.  Caller must ensure `data.len() <= available()`.
    fn push(&mut self, data: &[u8]) {
        // ---
        debug_assert!(data.len() <= self.available());
        self.buf.extend(data.iter().copied());
        self.head += data.len() as u64;
    }

    // ---

    /// Advance `A` to `up_to`, freeing space.  Called on `WormholeMsg::Ack`.
    fn ack(&mut self, up_to: u64) {
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
    /// Returns one or both VecDeque segments — callers may need to call
    /// twice if the data wraps around the internal ring.
    fn slice_from(&self, from: u64) -> &[u8] {
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
// UplinkHandle
// ---------------------------------------------------------------------------

/// Held by [`remote`] in `SessionManager`.
///
/// On reconnect, `SessionManager` opens a new QUIC stream and sends it via
/// `stream_tx`.  On permanent failure it sends `None` (or drops the sender).
pub struct UplinkHandle {
    // ---
    /// Deliver a new `QueLayStreamPtr` here after reconnect, or `None` to
    /// permanently fail the stream.
    pub stream_tx: mpsc::Sender<QueLayStreamPtr>,

    /// Retained so `SessionManager` can re-open the stream on a new session.
    pub info: StreamInfo,

    /// Priority for re-opening.
    pub priority: Priority,
}

// ---------------------------------------------------------------------------
// ActiveStream  (downlink only — uplink uses free functions below)
// ---------------------------------------------------------------------------

pub(crate) struct ActiveStream {
    // ---
    uuid: Uuid,
    _info: StreamInfo,
    quic: QueLayStreamPtr,
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
    /// then spawns a TCP-reader task and a QUIC-pump task sharing a
    /// [`SpoolBuffer`].
    ///
    /// Returns an [`UplinkHandle`] for [`SessionManager`] to use on reconnect.
    pub async fn spawn_uplink(
        // ---
        uuid: Uuid,
        info: StreamInfo,
        priority: Priority,
        quic: QueLayStreamPtr,
        cb_tx: CallbackTx,
    ) -> anyhow::Result<UplinkHandle> {
        // ---

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

        let (stream_tx, stream_rx) = mpsc::channel::<QueLayStreamPtr>(1);
        let _ = stream_tx.try_send(quic);

        let handle = UplinkHandle {
            stream_tx,
            info: info.clone(),
            priority,
        };

        let spool = Arc::new(Mutex::new(SpoolBuffer::new()));
        let data_ready = Arc::new(Notify::new());
        let space_ready = Arc::new(Notify::new());

        tokio::spawn(run_tcp_reader(
            uuid,
            listener,
            Arc::clone(&spool),
            Arc::clone(&data_ready),
            Arc::clone(&space_ready),
            cb_tx.clone(),
        ));

        tokio::spawn(run_quic_pump(
            uuid,
            spool,
            data_ready,
            space_ready,
            stream_rx,
            cb_tx,
        ));

        Ok(handle)
    }

    // ---

    /// Spawn a downlink (receiver-side) pump task.
    ///
    /// 1. Reads the [`StreamHeader`] from the inbound QUIC stream.
    /// 2. Binds an ephemeral TCP listener.
    /// 3. Fires [`CallbackCmd::StreamStarted`].
    /// 4. Accepts one client connection.
    /// 5. Pipes QUIC → TCP; sends [`WormholeMsg::Ack`] periodically and
    ///    [`WormholeMsg::Done`] on QUIC EOF.
    /// 6. Fires [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`].
    pub async fn spawn_downlink(quic: QueLayStreamPtr, cb_tx: CallbackTx) -> anyhow::Result<()> {
        // ---

        use super::read_header;

        let mut quic = quic;
        let header = read_header(&mut quic).await?;
        let uuid = header.uuid;

        let info = StreamInfo {
            size_bytes: header.size_bytes,
            attrs: header.attrs,
        };

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

        let actor = ActiveStream {
            uuid,
            _info: info,
            quic,
            cb_tx,
        };

        tokio::spawn(async move { actor.run_downlink(listener).await });

        Ok(())
    }

    // ---

    async fn run_downlink(mut self, listener: TcpListener) {
        // ---
        use super::write_wormhole_msg;

        let mut tcp = match listener.accept().await {
            Ok((tcp, addr)) => {
                tracing::info!(uuid = %self.uuid, %addr, "downlink: client connected");
                tcp
            }
            Err(e) => {
                tracing::warn!(uuid = %self.uuid, "downlink: accept failed: {e}");
                let _ = write_wormhole_msg(
                    &mut self.quic,
                    &WormholeMsg::Error {
                        code: FailReason::UNKNOWN.0 as u32,
                        reason: format!("TCP accept failed: {e}"),
                    },
                )
                .await;
                self.cb_tx
                    .send(CallbackCmd::StreamFailed {
                        uuid: self.uuid,
                        code: FailReason::UNKNOWN,
                        reason: format!("TCP accept failed: {e}"),
                    })
                    .await;
                return;
            }
        };

        let mut bytes_written = 0u64;
        let mut last_acked = 0u64;

        loop {
            match read_chunk(&mut self.quic).await {
                Ok(None) => {
                    // Clean QUIC EOF — sender closed write half.
                    tracing::info!(
                        uuid  = %self.uuid,
                        bytes = bytes_written,
                        "downlink: QUIC EOF, transfer complete",
                    );
                    let _ = write_wormhole_msg(&mut self.quic, &WormholeMsg::Done).await;
                    self.cb_tx
                        .send(CallbackCmd::StreamDone {
                            uuid: self.uuid,
                            bytes: bytes_written,
                        })
                        .await;
                    return;
                }
                Ok(Some(Chunk {
                    stream_offset,
                    payload,
                })) => {
                    // Duplicate: sender replayed a chunk we already delivered.
                    if stream_offset < bytes_written {
                        tracing::debug!(
                            uuid = %self.uuid,
                            stream_offset,
                            bytes_written,
                            "downlink: duplicate chunk — skipping",
                        );
                        continue;
                    }

                    // Gap: offset ahead of what we expect — this is a bug.
                    if stream_offset > bytes_written {
                        tracing::error!(
                            uuid = %self.uuid,
                            stream_offset,
                            bytes_written,
                            "downlink: chunk offset gap — logic error in pump",
                        );
                        let _ = write_wormhole_msg(
                            &mut self.quic,
                            &WormholeMsg::Error {
                                code:   FailReason::UNKNOWN.0 as u32,
                                reason: format!(
                                    "chunk gap: expected offset {bytes_written}, got {stream_offset}"
                                ),
                            },
                        )
                        .await;
                        self.cb_tx
                            .send(CallbackCmd::StreamFailed {
                                uuid: self.uuid,
                                code: FailReason::UNKNOWN,
                                reason: "chunk offset gap — pump logic error".into(),
                            })
                            .await;
                        return;
                    }

                    // In-order chunk — write to client TCP socket.
                    if let Err(e) = tcp.write_all(&payload).await {
                        tracing::warn!(uuid = %self.uuid, "downlink: TCP write error: {e}");
                        let _ = write_wormhole_msg(
                            &mut self.quic,
                            &WormholeMsg::Error {
                                code: FailReason::UNKNOWN.0 as u32,
                                reason: format!("TCP write error: {e}"),
                            },
                        )
                        .await;
                        self.cb_tx
                            .send(CallbackCmd::StreamFailed {
                                uuid: self.uuid,
                                code: FailReason::UNKNOWN,
                                reason: format!("TCP write error: {e}"),
                            })
                            .await;
                        return;
                    }
                    bytes_written += payload.len() as u64;

                    // Periodic ack so sender can advance its spool tail (A).
                    if bytes_written - last_acked >= ACK_INTERVAL {
                        last_acked = bytes_written;
                        let _ = write_wormhole_msg(
                            &mut self.quic,
                            &WormholeMsg::Ack {
                                bytes_received: bytes_written,
                            },
                        )
                        .await;
                        tracing::debug!(
                            uuid           = %self.uuid,
                            bytes_received = bytes_written,
                            "downlink: sent ack",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(uuid = %self.uuid, "downlink: QUIC read error: {e}");
                    self.cb_tx
                        .send(CallbackCmd::StreamFailed {
                            uuid: self.uuid,
                            code: FailReason::LINK_FAILED,
                            reason: format!("QUIC read error: {e}"),
                        })
                        .await;
                    return;
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
/// Pauses when spool is full (back-pressure to client).
/// Wakes when the pump signals `space_ready` after an incoming Ack.
/// Signals EOF via `spool.head = u64::MAX`.
async fn run_tcp_reader(
    uuid: Uuid,
    listener: TcpListener,
    spool: Arc<Mutex<SpoolBuffer>>,
    data_ready: Arc<Notify>,
    space_ready: Arc<Notify>,
    cb_tx: CallbackTx,
) {
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

    // Read exactly CHUNK_SIZE bytes at a time so spool entries align to
    // chunk boundaries — makes offset arithmetic in the pump exact.
    let mut tcp_buf = vec![0u8; CHUNK_SIZE];

    loop {
        // Block until spool has space for one full chunk.
        loop {
            if spool.lock().await.available() >= CHUNK_SIZE {
                break;
            }
            tracing::warn!(%uuid, "uplink: spool full — pausing TCP reader");
            space_ready.notified().await;
        }

        match tcp.read(&mut tcp_buf).await {
            Ok(0) => {
                tracing::info!(%uuid, "uplink: TCP EOF from client");
                spool.lock().await.head = u64::MAX; // EOF sentinel
                data_ready.notify_one();
                return;
            }
            Ok(n) => {
                spool.lock().await.push(&tcp_buf[..n]);
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

// ---

/// Drains the spool into the QUIC stream.
///
/// On QUIC write error: rewinds `Q` to `A`, waits for a new stream via
/// `stream_rx`, then replays `A..T` on the new stream before resuming.
async fn run_quic_pump(
    uuid: Uuid,
    spool: Arc<Mutex<SpoolBuffer>>,
    data_ready: Arc<Notify>,
    space_ready: Arc<Notify>,
    mut stream_rx: mpsc::Receiver<QueLayStreamPtr>,
    cb_tx: CallbackTx,
) {
    // ---
    // Receive the initial QUIC stream from spawn_uplink.
    let mut quic = match stream_rx.recv().await {
        Some(s) => s,
        None => {
            tracing::warn!(%uuid, "uplink pump: stream_tx dropped before initial stream");
            return;
        }
    };

    // Q: absolute offset of the next byte to write to QUIC.
    let mut q: u64 = 0;
    let mut bytes_sent: u64 = 0;

    loop {
        // --- Wait for data in spool (Q < T) ---
        loop {
            let s = spool.lock().await;
            let done = s.head == u64::MAX;
            if done || s.head > q {
                break;
            }
            drop(s);
            data_ready.notified().await;
        }

        let (head, a, chunk) = {
            let s = spool.lock().await;
            // Collect a contiguous slice from Q.
            let chunk: Vec<u8> = s.slice_from(q).to_vec();
            (s.head, s.bytes_acked, chunk)
        };

        // --- EOF sentinel: spool drained, shutdown QUIC write half ---
        if head == u64::MAX && chunk.is_empty() {
            tracing::info!(%uuid, bytes = bytes_sent, "uplink: flushed, shutting down QUIC stream");
            let _ = quic.shutdown().await;
            // Wait for WormholeMsg::Done from receiver.
            loop {
                match read_wormhole_msg(&mut quic).await {
                    Ok(WormholeMsg::Ack { bytes_received }) => {
                        spool.lock().await.ack(bytes_received);
                        space_ready.notify_one();
                    }
                    Ok(WormholeMsg::Done) => {
                        tracing::info!(%uuid, bytes = bytes_sent, "uplink: receiver done");
                        cb_tx
                            .send(CallbackCmd::StreamDone {
                                uuid,
                                bytes: bytes_sent,
                            })
                            .await;
                        return;
                    }
                    Ok(WormholeMsg::Error { code, reason }) => {
                        tracing::warn!(%uuid, code, %reason, "uplink: receiver error");
                        cb_tx
                            .send(CallbackCmd::StreamFailed {
                                uuid,
                                code: FailReason(code as i32),
                                reason,
                            })
                            .await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(%uuid, "uplink: wormhole closed after EOF: {e}");
                        cb_tx
                            .send(CallbackCmd::StreamFailed {
                                uuid,
                                code: FailReason::LINK_FAILED,
                                reason: format!("wormhole closed: {e}"),
                            })
                            .await;
                        return;
                    }
                }
            }
        }

        // --- Check spool capacity while link is down ---
        if (head != u64::MAX) && ((head - a) as usize >= SPOOL_CAPACITY) {
            tracing::error!(
                %uuid,
                held = head - a,
                "uplink: spool capacity exceeded — failing stream (increase SPOOL_CAPACITY)",
            );
            cb_tx
                .send(CallbackCmd::StreamFailed {
                    uuid,
                    code: FailReason::UNKNOWN,
                    reason: "spool capacity exceeded — link outage bridging window exhausted"
                        .into(),
                })
                .await;
            return;
        }

        if chunk.is_empty() {
            continue;
        }

        // --- Write chunk to QUIC (framed with stream offset) ---
        if let Err(e) = write_chunk(&mut quic, q, &chunk).await {
            tracing::warn!(%uuid, "uplink: QUIC write error: {e} — waiting for reconnect");

            // Rewind Q to A — replay from last-acked byte on new stream.
            q = spool.lock().await.bytes_acked;

            // Wait for SessionManager to deliver a fresh QUIC stream.
            match stream_rx.recv().await {
                Some(new_stream) => {
                    tracing::info!(%uuid, replay_from = q, "uplink: new stream — replaying spool");
                    quic = new_stream;
                }
                None => {
                    tracing::warn!(%uuid, "uplink: stream handle dropped — failing stream");
                    cb_tx
                        .send(CallbackCmd::StreamFailed {
                            uuid,
                            code: FailReason::LINK_FAILED,
                            reason: "link failed permanently".into(),
                        })
                        .await;
                    return;
                }
            }
            // Q is at A — outer loop will replay from there.
            continue;
        }

        q += chunk.len() as u64;
        bytes_sent += chunk.len() as u64;

        // --- Non-blocking wormhole poll ---
        tokio::select! {
            biased;
            result = read_wormhole_msg(&mut quic) => {
                match result {
                    Ok(WormholeMsg::Ack { bytes_received }) => {
                        let mut s = spool.lock().await;
                        s.ack(bytes_received);
                        space_ready.notify_one();
                        tracing::debug!(%uuid, bytes_received, "uplink: receiver ack");
                    }
                    Ok(WormholeMsg::Done) => {
                        tracing::info!(%uuid, bytes = bytes_sent, "uplink: receiver done early");
                        cb_tx.send(CallbackCmd::StreamDone { uuid, bytes: bytes_sent }).await;
                        return;
                    }
                    Ok(WormholeMsg::Error { code, reason }) => {
                        tracing::warn!(%uuid, code, %reason, "uplink: receiver error");
                        cb_tx.send(CallbackCmd::StreamFailed {
                            uuid,
                            code: FailReason(code as i32),
                            reason,
                        }).await;
                        return;
                    }
                    Err(_) => { /* wormhole not ready — keep pumping */ }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::ZERO) => {}
        }
    }
}
