//! Tuner actor — one sender and one receiver per stream pair.
//!
//! # Sender tuner lifecycle
//!
//! 1. Waits for [`TunerCmd::Port`] from CIC
//! 2. Connects TCP to that port, pumps `payload` in a `spawn_blocking` task
//! 3. On [`TunerCmd::Shutdown`] — signals the writer to close the socket (EOF)
//! 4. Waits for [`TunerCmd::Done`] or [`TunerCmd::Failed`] from CIC
//! 5. Sends [`CicMsg::TunerFinished`] and returns `Ok(())`
//!
//! # Receiver tuner lifecycle
//!
//! 1. Waits for [`TunerCmd::Port`] from CIC
//! 2. Connects TCP, reads until EOF counting bytes (no buffering)
//! 3. Waits for [`TunerCmd::Done`] or [`TunerCmd::Failed`] from CIC
//! 4. Sends [`CicMsg::TunerFinished`] and returns `Ok(())`
//!
//! sha256 verification is **skipped** — transfers are intentionally partial.

use std::time::{Duration, Instant};

// ---

use tokio::sync::mpsc;

// ---

use super::CicHandle;
use super::CicMsg;
use super::Role;

// ---------------------------------------------------------------------------
// TunerCmd
// ---------------------------------------------------------------------------

/// Commands sent from CIC to a tuner task.
#[derive(Debug, Clone)]
pub enum TunerCmd {
    /// CIC → tuner: agent opened this ephemeral port; connect to it.
    Port(u16),

    /// CIC → tuner: agent confirmed the stream finished normally.
    Done { role: Role, bytes: u64 },

    /// CIC → tuner: agent reported a stream failure.
    Failed { role: Role, reason: String },

    /// CIC → sender tuner: duration elapsed, close the write socket.
    Shutdown,

    /// CIC → any tuner: failsafe abort.
    Kill,
}

// ---------------------------------------------------------------------------
// TunerOutcome / TunerResult
// ---------------------------------------------------------------------------

/// Outcome of a single tuner task.
#[derive(Debug, Clone)]
pub enum TunerOutcome {
    Pass,
    Fail { reason: String },
}

// ---

/// Result reported by a tuner task to CIC via [`CicMsg::TunerFinished`].
#[derive(Debug, Clone)]
pub struct TunerResult {
    pub uuid: String,
    pub role: Role,
    pub outcome: TunerOutcome,
    pub bytes: u64,
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// spawn_sender
// ---------------------------------------------------------------------------

/// Spawn a sender tuner task.  The join handle returns `Ok(())` on clean exit
/// or `Err` on panic — results are reported to CIC via [`CicMsg::TunerFinished`].
pub fn spawn_sender(
    uuid: String,
    payload: Vec<u8>,
    mut cmd_rx: mpsc::Receiver<TunerCmd>,
    cic: CicHandle,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    // ---
    tokio::spawn(async move {
        // ---

        tracing::debug!("spawn_sender: starting ...");

        let t_start = Instant::now();
        let port = match wait_for_port(&uuid, Role::Sender, &mut cmd_rx, &cic, t_start).await {
            Some(p) => p,
            None => {
                tracing::info!("spawn_sender: wait_for_port returns None");
                return Ok(());
            }
        };
        tracing::info!(%uuid, port, "sender: connecting");
        // --- spawn blocking TCP writer ---
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
        let write_handle = tokio::spawn(async_tcp_writer(port, payload, abort_rx));

        let _bytes_written = match wait_for_shutdown(
            &uuid,
            &mut cmd_rx,
            abort_tx,
            &cic,
            write_handle,
            t_start,
        )
        .await
        {
            Some(n) => {
                tracing::info!(%uuid, %n, "sender: got wait_for_shutdown");
                n
            }
            None => {
                tracing::info!(%uuid, "sender: got NONE return from wait_for_shutdown");
                return Ok(());
            }
        };
        tracing::info!(%uuid, "sender: after wait_for_shutdown");

        // --- wait for agent's Done / Failed after the socket was closed ---
        let final_bytes =
            match wait_for_agent_done(&uuid, Role::Sender, &mut cmd_rx, &cic, t_start).await {
                Some(n) => n,
                None => return Ok(()),
            };

        finish(
            &cic,
            TunerResult {
                uuid: uuid.clone(),
                role: Role::Sender,
                outcome: TunerOutcome::Pass,
                bytes: final_bytes,
                elapsed: t_start.elapsed(),
            },
        )
        .await;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// spawn_receiver
// ---------------------------------------------------------------------------

/// Spawn a receiver tuner task.
pub fn spawn_receiver(
    uuid: String,
    mut cmd_rx: mpsc::Receiver<TunerCmd>,
    cic: CicHandle,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    // ---
    tokio::spawn(async move {
        // ---
        let t_start = Instant::now();

        let port = match wait_for_port(&uuid, Role::Receiver, &mut cmd_rx, &cic, t_start).await {
            Some(p) => p,
            None => {
                tracing::info!(%uuid, "receiver: Got None return from wait_for_port");
                return Ok(());
            }
        };

        tracing::info!(%uuid, port, "receiver: connecting");

        // --- spawn blocking TCP reader ---
        let read_handle = tokio::task::spawn_blocking(move || blocking_tcp_reader(port));

        // --- wait for Done / Failed from CIC ---
        let outcome =
            match wait_for_receiver_done(&uuid, &mut cmd_rx, &cic, &read_handle, t_start).await {
                Some(o) => o,
                None => return Ok(()),
            };

        let bytes = match read_handle.await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!(%uuid, "receiver read error: {e}");
                0
            }
            Err(_) => 0,
        };

        finish(
            &cic,
            TunerResult {
                uuid: uuid.clone(),
                role: Role::Receiver,
                outcome,
                bytes,
                elapsed: t_start.elapsed(),
            },
        )
        .await;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Phase helpers — sender
// ---------------------------------------------------------------------------

/// Wait for the first [`TunerCmd::Port`] from CIC.
///
/// Returns `Some(port)` on success.  Handles `Kill` and channel-close by
/// reporting to CIC and returning `None` (caller should `return Ok(())`).
async fn wait_for_port(
    uuid: &str,
    role: Role,
    cmd_rx: &mut mpsc::Receiver<TunerCmd>,
    cic: &CicHandle,
    t_start: Instant,
) -> Option<u16> {
    // ---
    tracing::debug!(uuid, "wait_for_port");
    loop {
        match cmd_rx.recv().await {
            Some(TunerCmd::Port(p)) => {
                tracing::debug!("wait_for_port: Got TunerCmd::Port from rx.recv");
                return Some(p);
            }
            Some(TunerCmd::Kill) => {
                tracing::debug!("wait_for_port: Got TunerCmd::Kill from rx.recv");
                finish(cic, killed(uuid, role, t_start)).await;
                return None;
            }
            Some(other) => {
                tracing::debug!("wait_for_port: Got Some({other:?}) from rx.recv",);
                tracing::warn!(%uuid, ?role, ?other, "tuner: unexpected cmd awaiting port");
            }
            None => {
                tracing::debug!("wait_for_port: Got None from rx.recv");
                finish(cic, disconnected(uuid, role, t_start)).await;
                return None;
            }
        }
    }
}

/// Wait for [`TunerCmd::Shutdown`] (or early `Done`/`Failed`/`Kill`).
///
/// Sends the abort signal to the writer task, waits for it to drain, and
/// returns `Some(bytes_written)`.  Returns `None` and reports to CIC on
/// `Kill`, `Failed`, or channel-close.
async fn wait_for_shutdown(
    uuid: &str,
    cmd_rx: &mut mpsc::Receiver<TunerCmd>,
    abort_tx: tokio::sync::oneshot::Sender<()>,
    cic: &CicHandle,
    write_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    t_start: Instant,
) -> Option<u64> {
    // ---

    tracing::info!(%uuid, "sender: wait_for_shutdown, starting...");

    loop {
        let cmd = cmd_rx.recv().await;
        tracing::info!(%uuid, "sender: wait_for_shutdown: {:?}", cmd);

        match cmd {
            Some(TunerCmd::Shutdown) => {
                let _ = abort_tx.send(());

                tracing::info!(%uuid, "wait_for_shutdown: TunerCmd::Shutdown, aborting sender");

                match write_handle.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(%uuid, "write task error: {e}"),
                    Err(_) => {}
                }
                return Some(0);
            }

            Some(TunerCmd::Kill) => {
                let _ = abort_tx.send(());

                tracing::info!(%uuid, "wait_for_shutdown: TunerCmd::Kill, aborting sender");
                write_handle.abort();
                finish(cic, killed(uuid, Role::Sender, t_start)).await;
                return None;
            }

            // Payload exhausted before shutdown — unlikely in bw-cap-test
            // but handle it gracefully.
            Some(TunerCmd::Done { bytes, .. }) => {
                let _ = abort_tx.send(());

                tracing::info!(%uuid, "wait_for_shutdown: TunerCmd::Done");
                let _ = write_handle.await;
                return Some(bytes);
            }

            Some(TunerCmd::Failed { reason, .. }) => {
                let _ = abort_tx.send(());

                tracing::info!(%uuid, "wait_for_shutdown: TunerCmd::Failed");
                write_handle.abort();
                finish(cic, fail(uuid, Role::Sender, reason, t_start)).await;
                return None;
            }

            Some(other) => {
                tracing::warn!(%uuid, ?other, "sender: wait_for_shutdown: unexpected cmd")
            }
            None => {
                let _ = abort_tx.send(());

                tracing::info!(%uuid, "wait_for_shutdown: None");
                write_handle.abort();
                finish(cic, disconnected(uuid, Role::Sender, t_start)).await;
                return None;
            }
        }
    }
}

/// Wait for the agent's `Done` or `Failed` after the sender socket has closed.
///
/// Returns `Some(bytes)` on `Done`.  Returns `None` and reports to CIC on
/// `Failed`, `Kill`, or channel-close.
async fn wait_for_agent_done(
    uuid: &str,
    role: Role,
    cmd_rx: &mut mpsc::Receiver<TunerCmd>,
    cic: &CicHandle,
    t_start: Instant,
) -> Option<u64> {
    // ---
    tracing::debug!("wait_for_agent_done: ...");
    loop {
        match cmd_rx.recv().await {
            Some(TunerCmd::Done { bytes, .. }) => {
                tracing::debug!("wait_for_agent_done: Got TunerCmd::Done ...");
                return Some(bytes);
            }
            Some(TunerCmd::Failed { reason, .. }) => {
                tracing::debug!("wait_for_agent_done: Got TunerCmd::Finish ...");
                finish(cic, fail(uuid, role, reason, t_start)).await;
                return None;
            }
            Some(TunerCmd::Kill) => {
                tracing::debug!("wait_for_agent_done: Got TunerCmd::Kill ...");
                finish(cic, killed(uuid, role, t_start)).await;
                return None;
            }
            Some(other) => {
                tracing::warn!(%uuid, ?other, "wait_for_agent_done: unexpected cmd post-shutdown")
            }
            None => {
                tracing::debug!("wait_for_agent_done: none ...");
                finish(cic, disconnected(uuid, role, t_start)).await;
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase helpers — receiver
// ---------------------------------------------------------------------------

/// Wait for [`TunerCmd::Done`] or `Failed` from CIC for the receiver side.
///
/// Aborts the read handle on `Failed`/`Kill`/disconnect.  Returns
/// `Some(TunerOutcome)` on `Done` or `Failed`; `None` on `Kill` or
/// channel-close (caller should `return Ok(())`).
async fn wait_for_receiver_done(
    uuid: &str,
    cmd_rx: &mut mpsc::Receiver<TunerCmd>,
    cic: &CicHandle,
    read_handle: &tokio::task::JoinHandle<anyhow::Result<u64>>,
    t_start: Instant,
) -> Option<TunerOutcome> {
    // ---

    tracing::debug!(%uuid, "wait_for_receiver_done ...");

    loop {
        match cmd_rx.recv().await {
            Some(TunerCmd::Done { .. }) => {
                tracing::debug!(%uuid, "wait_for_receiver_done Got TunerCmd::Done...");
                return Some(TunerOutcome::Pass);
            }
            Some(TunerCmd::Failed { reason, .. }) => {
                tracing::debug!(%uuid, "wait_for_receiver_done Got TunerCmd::Failed...");
                read_handle.abort();
                return Some(TunerOutcome::Fail { reason });
            }
            Some(TunerCmd::Kill) => {
                tracing::debug!(%uuid, "wait_for_receiver_done Got TunerCmd::Kill...");
                read_handle.abort();
                finish(cic, killed(uuid, Role::Receiver, t_start)).await;
                return None;
            }
            Some(other) => tracing::warn!(%uuid, ?other, "receiver: unexpected cmd"),
            None => {
                tracing::debug!(%uuid, "wait_for_receiver_done Got None...");
                read_handle.abort();
                finish(cic, disconnected(uuid, Role::Receiver, t_start)).await;
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking I/O helpers
// ---------------------------------------------------------------------------

/// Write `payload` to a TCP connection on `port`.
///
/// Uses an async task so `write_handle.abort()` delivers TCP EOF promptly
/// without fighting kernel send buffers.
async fn async_tcp_writer(
    port: u16,
    payload: Vec<u8>,
    mut abort_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    // ---
    use tokio::io::AsyncWriteExt;
    let mut tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;

    for chunk in payload.chunks(64 * 1024) {
        tokio::select! {
            biased;
            _ = &mut abort_rx => {
                tracing::info!("async_tcp_writer: abort received");
                break;
            }
            result = tcp.write_all(chunk) => {
                result?;
            }
        }
    }
    let _ = tcp.shutdown().await;
    Ok(())
}

/// Read from a TCP connection on `port` until EOF, counting bytes received.
///
/// Intended for use inside `tokio::task::spawn_blocking`.
fn blocking_tcp_reader(port: u16) -> anyhow::Result<u64> {
    // ---
    use std::io::Read;
    let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;

    tracing::debug!("reader: starting ...");

    loop {
        tracing::debug!("reader: reading chunk !!");
        let n = tcp.read(&mut buf)?;
        if n == 0 {
            tracing::info!("reader: got EOF !!");
            break; // EOF
        }
        total += n as u64;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async fn finish(cic: &CicHandle, result: TunerResult) {
    // ---
    let _ = cic
        .tx
        .send(CicMsg::TunerFinished {
            uuid: result.uuid.clone(),
            role: result.role,
            result,
        })
        .await;
}

fn fail(uuid: &str, role: Role, reason: String, t_start: Instant) -> TunerResult {
    // ---
    TunerResult {
        uuid: uuid.to_string(),
        role,
        outcome: TunerOutcome::Fail { reason },
        bytes: 0,
        elapsed: t_start.elapsed(),
    }
}

fn killed(uuid: &str, role: Role, t_start: Instant) -> TunerResult {
    // ---
    tracing::warn!(%uuid, ?role, "tuner: killed by failsafe");
    fail(uuid, role, "killed by failsafe".into(), t_start)
}

fn disconnected(uuid: &str, role: Role, t_start: Instant) -> TunerResult {
    // ---
    tracing::warn!(%uuid, ?role, "tuner: CIC channel closed unexpectedly");
    fail(uuid, role, "CIC channel closed".into(), t_start)
}
