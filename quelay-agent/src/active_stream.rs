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
//!   and writes them to the ephemeral port → client reads from disk.
//!   Errors on the TCP write half are sent back as [`WormholeMsg::Error`].
//!
//! # Lifecycle
//!
//! Both modes fire [`CallbackCmd::StreamStarted`] after the ephemeral port
//! is open, then [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`]
//! at the end.  Progress callbacks are fired periodically while active.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

// ---

use quelay_domain::{QueLayStreamPtr, StreamInfo};
use quelay_thrift::FailReason;

// ---

use super::{read_wormhole_msg, CallbackCmd, CallbackTx, WormholeMsg};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Read/write buffer size for the pump loops.
const PUMP_BUF: usize = 64 * 1024; // 64 KiB

// ---------------------------------------------------------------------------
// ActiveStream
// ---------------------------------------------------------------------------

/// Per-stream actor.  Constructed by [`SessionManager`] and spawned as a
/// tokio task via [`ActiveStream::spawn_uplink`] or [`ActiveStream::spawn_downlink`].
pub struct ActiveStream {
    uuid: Uuid,
    _info: StreamInfo,
    quic: QueLayStreamPtr,
    cb_tx: CallbackTx,
}

// ---

impl ActiveStream {
    // ---

    /// Spawn an uplink (sender-side) pump task.
    ///
    /// 1. Opens an ephemeral TCP listener on `127.0.0.1:0`.
    /// 2. Fires [`CallbackCmd::StreamStarted`] with the assigned port.
    /// 3. Accepts one client connection.
    /// 4. Pipes TCP → QUIC stream; concurrently reads [`WormholeMsg`] from
    ///    the QUIC read half.
    /// 5. Fires [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`].
    ///
    /// Returns immediately after the task is spawned — the pump runs in the
    /// background.  Errors opening the listener or accepting the connection
    /// are returned before the task is spawned.
    pub async fn spawn_uplink(
        uuid: Uuid,
        info: StreamInfo,
        quic: QueLayStreamPtr,
        cb_tx: CallbackTx,
    ) -> anyhow::Result<()> {
        // ---
        // Bind the ephemeral listener before spawning so we can return the
        // error synchronously if the OS rejects the bind.
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

        let actor = ActiveStream {
            uuid,
            _info: info,
            quic,
            cb_tx,
        };

        tokio::spawn(async move {
            actor.run_uplink(listener).await;
        });

        Ok(())
    }

    // ---

    /// Uplink pump body.  Runs inside a spawned task.
    async fn run_uplink(mut self, listener: TcpListener) {
        // ---

        // Accept exactly one client connection.
        let mut tcp = match listener.accept().await {
            Ok((tcp, addr)) => {
                tracing::info!(uuid = %self.uuid, %addr, "uplink: client connected");
                tcp
            }
            Err(e) => {
                tracing::warn!(uuid = %self.uuid, "uplink: accept failed: {e}");
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

        let mut buf = vec![0u8; PUMP_BUF];
        let mut bytes_sent: u64 = 0;
        let mut tcp_done = false;

        loop {
            tokio::select! {
                // --- TCP read half: client → QUIC stream ---
                // Disabled once EOF is received to stop spinning on Ok(0).
                result = tcp.read(&mut buf), if !tcp_done => {
                    match result {
                        Ok(0) => {
                            // Client closed write half — signal EOF to receiver.
                            tracing::info!(
                                uuid = %self.uuid,
                                bytes = bytes_sent,
                                "uplink: EOF from client, shutting down QUIC stream",
                            );
                            tcp_done = true;
                            if let Err(e) = self.quic.shutdown().await {
                                tracing::warn!(uuid = %self.uuid, "uplink: QUIC shutdown error: {e}");
                            }
                            // Only the wormhole branch remains — wait for Done or Error.
                        }
                        Ok(n) => {
                            if let Err(e) = self.quic.write_all(&buf[..n]).await {
                                tracing::warn!(uuid = %self.uuid, "uplink: QUIC write error: {e}");
                                self.cb_tx.send(CallbackCmd::StreamFailed {
                                    uuid:   self.uuid,
                                    code:   FailReason::UNKNOWN,
                                    reason: format!("QUIC write error: {e}"),
                                }).await;
                                return;
                            }
                            bytes_sent += n as u64;
                        }
                        Err(e) => {
                            tracing::warn!(uuid = %self.uuid, "uplink: TCP read error: {e}");
                            self.cb_tx.send(CallbackCmd::StreamFailed {
                                uuid:   self.uuid,
                                code:   FailReason::SENDER_CLOSED,
                                reason: format!("TCP read error: {e}"),
                            }).await;
                            return;
                        }
                    }
                }

                // --- QUIC read half: WormholeMsg from receiver ---
                result = read_wormhole_msg(&mut self.quic) => {
                    match result {
                        Ok(WormholeMsg::Ack { bytes_received }) => {
                            tracing::debug!(
                                uuid = %self.uuid,
                                bytes_received,
                                "uplink: receiver ack",
                            );
                        }
                        Ok(WormholeMsg::Done { checksum }) => {
                            tracing::info!(
                                uuid = %self.uuid,
                                bytes = bytes_sent,
                                ?checksum,
                                "uplink: receiver done",
                            );
                            self.cb_tx.send(CallbackCmd::StreamDone {
                                uuid:  self.uuid,
                                bytes: bytes_sent,
                            }).await;
                            return;
                        }
                        Ok(WormholeMsg::Error { code, reason }) => {
                            tracing::warn!(
                                uuid = %self.uuid,
                                code,
                                %reason,
                                "uplink: receiver error",
                            );
                            self.cb_tx.send(CallbackCmd::StreamFailed {
                                uuid:   self.uuid,
                                code:   FailReason(code as i32),
                                reason,
                            }).await;
                            return;
                        }
                        Err(e) => {
                            // QUIC read half closed — receiver side gone.
                            tracing::warn!(uuid = %self.uuid, "uplink: wormhole read error: {e}");
                            self.cb_tx.send(CallbackCmd::StreamFailed {
                                uuid:   self.uuid,
                                code:   FailReason::LINK_FAILED,
                                reason: format!("wormhole closed: {e}"),
                            }).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    // ---

    /// Spawn a downlink (receiver-side) pump task.
    ///
    /// 1. Reads the [`StreamHeader`] from the inbound QUIC stream.
    /// 2. Opens an ephemeral TCP listener on `127.0.0.1:0`.
    /// 3. Fires [`CallbackCmd::StreamStarted`] with the assigned port.
    /// 4. Accepts one client connection.
    /// 5. Pipes QUIC stream → TCP socket; sends [`WormholeMsg`] feedback
    ///    back on the QUIC write half.
    /// 6. Fires [`CallbackCmd::StreamDone`] or [`CallbackCmd::StreamFailed`].
    pub async fn spawn_downlink(
        quic: quelay_domain::QueLayStreamPtr,
        cb_tx: CallbackTx,
    ) -> anyhow::Result<()> {
        // ---

        use super::read_header;

        let mut quic = quic;
        let header = read_header(&mut quic).await?;
        let uuid = header.uuid;

        let info = quelay_domain::StreamInfo {
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

        tokio::spawn(async move {
            actor.run_downlink(listener).await;
        });

        Ok(())
    }

    // ---

    /// Downlink pump body.  Runs inside a spawned task.
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

        let mut buf = vec![0u8; PUMP_BUF];
        let mut bytes_written: u64 = 0;

        loop {
            match self.quic.read(&mut buf).await {
                Ok(0) => {
                    // Sender closed QUIC stream — transfer complete.
                    tracing::info!(
                        uuid = %self.uuid,
                        bytes = bytes_written,
                        "downlink: QUIC EOF, transfer complete",
                    );
                    let _ =
                        write_wormhole_msg(&mut self.quic, &WormholeMsg::Done { checksum: None })
                            .await;
                    self.cb_tx
                        .send(CallbackCmd::StreamDone {
                            uuid: self.uuid,
                            bytes: bytes_written,
                        })
                        .await;
                    return;
                }
                Ok(n) => {
                    if let Err(e) = tcp.write_all(&buf[..n]).await {
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
                    bytes_written += n as u64;
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
