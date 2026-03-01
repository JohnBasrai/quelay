//! [`CallbackAgent`] — persistent actor that owns the Thrift callback socket
//! to the local Quelay client.
//!
//! # Design
//!
//! The Thrift-generated [`TQueLayCallbackSyncClient`] is synchronous and not
//! `Send`, so it cannot live inside a tokio task directly.  Instead:
//!
//! - A dedicated `std::thread` owns the client socket and loops on a
//!   [`std::sync::mpsc`] channel (via `blocking_recv` on the tokio receiver).
//! - Callers clone [`CallbackTx`] and send [`CallbackCmd`] messages.
//! - All callback methods except `ping` are `oneway` in the IDL — they write
//!   to the OS send buffer and return immediately. `TCP_NODELAY` is set on
//!   connect so the kernel flushes each write without Nagle delay.
//! - `ping` is the sole blocking Call/Reply; it is sent by a periodic timer
//!   task every 60 seconds to detect dead clients.
//! - On connect failure, ping timeout, or send error the agent sets its
//!   client to `None` and logs. It reconnects when a new `Register` command
//!   arrives (i.e. the client calls `set_callback` again).

use std::time::Duration;

// ---

use tokio::sync::mpsc;
use uuid::Uuid;

// ---

use quelay_domain::{
    //
    LinkState,
    QueLayError,
    QueueStatus,
    Result,
    StreamInfo,
};

// ---

use quelay_thrift::{
    // ---
    FailReason,
    QueLayCallbackSyncClient,
    TBinaryInputProtocol,
    TBinaryOutputProtocol,
    TBufferedReadTransport,
    TBufferedWriteTransport,
    TIoChannel,
    TQueLayCallbackSyncClient,
    TTcpChannel,
};

// ---

use thrift::transport::{ReadHalf, WriteHalf};

// ---------------------------------------------------------------------------
// CallbackCmd
// ---------------------------------------------------------------------------

/// Commands sent to the [`CallbackAgent`] thread via [`CallbackTx`].
#[derive(Debug)]
pub enum CallbackCmd {
    // ---
    /// (Re)connect to the given endpoint.
    ///
    /// Sent by the Thrift handler when the client calls `set_callback`.
    /// If a connection already exists it is dropped and replaced ("hijack",
    /// matching the C++ predecessor behaviour).
    Register(String),

    /// Liveness probe — triggers a blocking `ping` RPC.
    ///
    /// Sent by the ping timer task every 60 seconds.
    /// A failed ping sets the client to `None`.
    Ping,

    // --- stream lifecycle ---------------------------------------------------
    StreamStarted {
        uuid: Uuid,
        info: StreamInfo,
        port: u16,
    },

    StreamProgress {
        uuid: Uuid,
        bytes: u64,
        percent: Option<f64>,
    },

    StreamDone {
        uuid: Uuid,
        bytes: u64,
    },

    StreamFailed {
        uuid: Uuid,
        code: FailReason,
        reason: String,
    },

    // --- system status ------------------------------------------------------
    LinkStatus(LinkState),

    QueueStatus(QueueStatus),
}

// ---------------------------------------------------------------------------
// CallbackTx
// ---------------------------------------------------------------------------

/// Cheap-clone sender handle.  Cloned into `AgentHandler`, `SessionManager`,
/// `ActiveStream` actors, and the ping timer.
#[derive(Clone)]
pub struct CallbackTx {
    // ---
    tx: mpsc::Sender<CallbackCmd>,
}

// ---

impl CallbackTx {
    // ---
    /// Send a command. Returns `false` if the channel has closed (agent exited).
    pub async fn send(&self, cmd: CallbackCmd) -> bool {
        if self.tx.send(cmd).await.is_err() {
            tracing::info!("CallbackAgent channel closed — dropping callback event");
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// CallbackAgent
// ---------------------------------------------------------------------------

/// Owns the Thrift callback socket to the local client.
///
/// Constructed via [`CallbackAgent::spawn`], which returns a [`CallbackTx`]
/// for sending commands and a [`PingTimerTx`] handle to start the ping timer.
pub struct CallbackAgent {
    // ---
    rx: mpsc::Receiver<CallbackCmd>,
}

// ---

impl CallbackAgent {
    // ---
    /// Spawn the callback agent thread and return the sender handle.
    ///
    /// The agent runs on a dedicated `std::thread` so the sync Thrift client
    /// never blocks the tokio runtime.1
    pub fn spawn() -> Result<CallbackTx> {
        // ---
        let (tx, rx) = mpsc::channel(64);
        let agent = CallbackAgent { rx };

        std::thread::Builder::new()
            .name("quelay-callback".into())
            .spawn(move || agent.run())
            .map_err(QueLayError::from)?; // or map_err(|e| QueLayError::Transport(e.to_string()))?

        Ok(CallbackTx { tx })
    }

    // ---

    fn run(mut self) {
        // ---
        // `Option<Client>` — `None` until `Register` arrives or after a dead ping.
        let mut client: Option<BoxedCallbackClient> = None;

        loop {
            // Block on the async channel from a sync thread.
            let cmd = match self.rx.blocking_recv() {
                Some(cmd) => cmd,
                None => {
                    tracing::debug!("callback channel closed — agent exiting");
                    return;
                }
            };

            match cmd {
                CallbackCmd::Register(endpoint) => {
                    client = None; // drop existing connection first
                    match connect(&endpoint) {
                        Ok(c) => {
                            tracing::debug!(%endpoint, "callback socket connected");
                            client = Some(c);
                        }
                        Err(e) => {
                            tracing::warn!(%endpoint, "callback connect failed: {e}");
                        }
                    }
                }

                CallbackCmd::Ping => {
                    if let Some(ref mut c) = client {
                        if let Err(e) = c.ping() {
                            tracing::info!("callback ping failed ({e}) — marking client dead");
                            client = None;
                        }
                    }
                    // If no client, ping is silently dropped — nothing to probe.
                }

                CallbackCmd::StreamStarted { uuid, info, port } => {
                    fire(&mut client, |c| {
                        let wire_info = quelay_thrift::StreamInfo::from(info);
                        c.stream_started(uuid.to_string(), wire_info, port as i32)
                    });
                }

                CallbackCmd::StreamProgress {
                    uuid,
                    bytes,
                    percent,
                } => {
                    fire(&mut client, |c| {
                        let progress = quelay_thrift::ProgressInfo {
                            bytes_transferred: Some(bytes as i64),
                            size_bytes: None,
                            percent_done: percent.map(thrift::OrderedFloat::from),
                        };
                        c.stream_progress(uuid.to_string(), progress)
                    });
                }

                CallbackCmd::StreamDone { uuid, bytes } => {
                    fire(&mut client, |c| {
                        c.stream_done(uuid.to_string(), bytes as i64)
                    });
                }

                CallbackCmd::StreamFailed { uuid, code, reason } => {
                    fire(&mut client, |c| {
                        c.stream_failed(uuid.to_string(), code, reason.clone())
                    });
                }

                CallbackCmd::LinkStatus(state) => {
                    let wire: quelay_thrift::LinkState = state.into();
                    fire(&mut client, |c| c.link_status_update(wire));
                }

                CallbackCmd::QueueStatus(status) => {
                    let wire: quelay_thrift::QueueStatus = status.into();
                    fire(&mut client, |c| c.queue_status_update(wire));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ping timer
// ---------------------------------------------------------------------------

/// Spawn the ping timer task.
///
/// Sends [`CallbackCmd::Ping`] every `interval` (default 60 s) via `tx`.
/// The task exits when the channel closes.
pub fn spawn_ping_timer(tx: CallbackTx, interval: Duration) {
    // ---
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !tx.send(CallbackCmd::Ping).await {
                break; // CallbackAgent has exited
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

type BoxedCallbackClient = QueLayCallbackSyncClient<
    TBinaryInputProtocol<TBufferedReadTransport<ReadHalf<TTcpChannel>>>,
    TBinaryOutputProtocol<TBufferedWriteTransport<WriteHalf<TTcpChannel>>>,
>;

// ---

/// Open a TCP connection to `endpoint`, set `TCP_NODELAY`, and return a
/// ready-to-use [`QueLayCallbackSyncClient`].
///
/// `TTcpChannel::set_nodelay` was not added until thrift 0.23 (THRIFT-5739).
/// On 0.17 we must build a `std::net::TcpStream` first, set `TCP_NODELAY`
/// on it directly, then wrap it with `TTcpChannel::with_stream`.
fn connect(endpoint: &str) -> anyhow::Result<BoxedCallbackClient> {
    // ---
    let tcp = std::net::TcpStream::connect(endpoint)?;
    tcp.set_nodelay(true)?;

    let channel = TTcpChannel::with_stream(tcp);
    let (rx, tx) = channel.split()?;

    let client = QueLayCallbackSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    );
    Ok(client)
}

// ---

/// Call `f(client)` if connected. On error, log and mark client dead.
///
/// All `oneway` calls return `Ok(())` immediately after flushing, so errors
/// here indicate a broken socket (e.g. client has exited).
fn fire<F>(client: &mut Option<BoxedCallbackClient>, f: F)
where
    F: FnOnce(&mut BoxedCallbackClient) -> thrift::Result<()>,
{
    let dead = match client.as_mut() {
        None => return, // no client registered yet
        Some(c) => match f(c) {
            Ok(()) => false,
            Err(e) => {
                tracing::warn!("callback send failed ({e}) — marking client dead");
                true
            }
        },
    };
    if dead {
        *client = None;
    }
}
