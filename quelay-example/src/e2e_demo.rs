//! End-to-end transfer demo — shows the minimal client code needed to push a
//! stream through two live quelay-agents.
//!
//! # What this demonstrates
//!
//! 1. Register a callback endpoint with the agent (`set_callback`).
//! 2. Start a stream (`stream_start`) and wait for the `stream_started` callback.
//! 3. Connect to the ephemeral TCP port and write payload bytes.
//! 4. On the receiver side, connect to the ephemeral port and read bytes.
//! 5. Wait for `stream_done` on both sides.
//! 6. Verify the SHA-256 of the received bytes matches the sent bytes.
//!
//! # What this does NOT demonstrate
//!
//! Link outage recovery, DRR priority scheduling, bandwidth cap validation,
//! and multi-file transfers are covered by the integration test binary
//! (`quelay-agent/src/bin/e2e_test.rs`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{mpsc, Mutex};
use std::time::Instant;

// ---

use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---

use quelay_thrift::{
    // ---
    FailReason,
    LinkState,
    QueLayAgentSyncClient,
    QueLayCallbackSyncHandler,
    QueLayCallbackSyncProcessor,
    QueueStatus,
    StreamInfo,
    TBinaryInputProtocol,
    TBinaryInputProtocolFactory,
    TBinaryOutputProtocol,
    TBinaryOutputProtocolFactory,
    TBufferedReadTransport,
    TBufferedReadTransportFactory,
    TBufferedWriteTransport,
    TBufferedWriteTransportFactory,
    TIoChannel,
    TQueLayAgentSyncClient,
    TServer,
    TTcpChannel,
};

// ---------------------------------------------------------------------------
// CallbackEvent
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CallbackEvent {
    // ---
    Started { port: u16 },
    Done { bytes: u64 },
    Failed { reason: String },
}

// ---------------------------------------------------------------------------
// CallbackHandler  (minimal — intentionally simple for readability)
// ---------------------------------------------------------------------------

struct CallbackHandler {
    tx: Mutex<mpsc::Sender<CallbackEvent>>,
}

impl QueLayCallbackSyncHandler for CallbackHandler {
    // ---

    fn handle_ping(&self) -> thrift::Result<()> {
        Ok(())
    }

    fn handle_stream_started(
        &self,
        _uuid: String,
        _info: StreamInfo,
        port: i32,
    ) -> thrift::Result<()> {
        // ---
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(CallbackEvent::Started { port: port as u16 });
        Ok(())
    }

    fn handle_stream_progress(
        &self,
        _uuid: String,
        _progress: quelay_thrift::ProgressInfo,
    ) -> thrift::Result<()> {
        Ok(())
    }

    fn handle_stream_done(&self, _uuid: String, bytes_transferred: i64) -> thrift::Result<()> {
        let _ = self.tx.lock().unwrap().send(CallbackEvent::Done {
            bytes: bytes_transferred as u64,
        });
        Ok(())
    }

    fn handle_stream_failed(
        &self,
        _uuid: String,
        _code: FailReason,
        reason: String,
    ) -> thrift::Result<()> {
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(CallbackEvent::Failed { reason });
        Ok(())
    }

    fn handle_link_status_update(&self, _state: LinkState) -> thrift::Result<()> {
        Ok(())
    }

    fn handle_queue_status_update(&self, _status: QueueStatus) -> thrift::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CallbackServer
// ---------------------------------------------------------------------------

struct CallbackServer {
    addr: SocketAddr,
    rx: mpsc::Receiver<CallbackEvent>,
}

impl CallbackServer {
    // ---

    fn bind() -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();
        let (tx, rx) = mpsc::channel();

        let handler = CallbackHandler { tx: Mutex::new(tx) };
        let processor = QueLayCallbackSyncProcessor::new(handler);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        std::thread::Builder::new()
            .name("quelay-cb".into())
            .spawn(move || {
                drop(listener);
                let mut server = TServer::new(
                    TBufferedReadTransportFactory::new(),
                    TBinaryInputProtocolFactory::new(),
                    TBufferedWriteTransportFactory::new(),
                    TBinaryOutputProtocolFactory::new(),
                    processor,
                    4,
                );
                let _ = ready_tx.send(());
                if let Err(e) = server.listen(&addr_str) {
                    tracing::warn!("callback server exiting: {e}");
                }
            })?;

        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("callback server did not start within 2s"))?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(Self { addr, rx })
    }

    fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    fn recv_event(&self, timeout: std::time::Duration) -> anyhow::Result<CallbackEvent> {
        self.rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("callback recv timeout: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn connect_agent(addr: SocketAddr) -> anyhow::Result<impl TQueLayAgentSyncClient> {
    let mut ch = TTcpChannel::new();
    ch.open(addr.to_string())?;
    let (rx, tx) = ch.split()?;
    Ok(QueLayAgentSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    ))
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// run — single straight transfer
// ---------------------------------------------------------------------------

/// Transfer `payload` from `sender_c2i` to `receiver_c2i` and verify the
/// received SHA-256 matches the sent SHA-256.
///
/// Prints a short timing and integrity report to stdout.
pub async fn run(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    let timeout = std::time::Duration::from_secs(60);
    let uuid = Uuid::new_v4().to_string();
    let sha256_sent = sha256_hex(&payload);
    let bytes = payload.len();

    let sender_cb = CallbackServer::bind()?;
    let receiver_cb = CallbackServer::bind()?;

    let mut sender_agent = connect_agent(sender_c2i)?;
    let mut receiver_agent = connect_agent(receiver_c2i)?;

    {
        let e = sender_agent.set_callback(sender_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (receiver): {e}");
    }

    let mut attrs = BTreeMap::new();
    attrs.insert("filename".to_string(), "e2e-demo.bin".to_string());
    attrs.insert("sha256".to_string(), sha256_sent.clone());

    let t_start = Instant::now();

    let result = sender_agent.stream_start(
        uuid.clone(),
        StreamInfo {
            size_bytes: Some(bytes as i64),
            attrs: Some(attrs),
        },
        0,
    )?;
    anyhow::ensure!(
        result.err_msg.as_deref().unwrap_or("").is_empty(),
        "stream_start failed: {:?}",
        result.err_msg
    );
    println!("  stream_start accepted, uuid: {uuid}");

    // Connect to sender's ephemeral port and write the payload.
    let sender_port = match sender_cb.recv_event(timeout)? {
        CallbackEvent::Started { port } => port,
        other => anyhow::bail!("sender: expected Started, got {other:?}"),
    };
    println!("  sender ephemeral port: {sender_port}");

    let write_task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{sender_port}"))?;
        tcp.write_all(&payload)?;
        Ok(())
    });

    // Connect to receiver's ephemeral port and read back the bytes.
    let receiver_port = match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Started { port } => port,
        other => anyhow::bail!("receiver: expected Started, got {other:?}"),
    };
    println!("  receiver ephemeral port: {receiver_port}");

    let mut received = Vec::with_capacity(bytes);
    {
        use std::io::Read;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.read_to_end(&mut received)?;
    }

    match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes } => println!("  receiver stream_done: {bytes} bytes"),
        CallbackEvent::Failed { reason } => anyhow::bail!("receiver stream_failed: {reason}"),
        other => anyhow::bail!("receiver: expected Done, got {other:?}"),
    }
    match sender_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes } => println!("  sender stream_done: {bytes} bytes"),
        CallbackEvent::Failed { reason } => anyhow::bail!("sender stream_failed: {reason}"),
        other => anyhow::bail!("sender: expected Done, got {other:?}"),
    }

    write_task.await??;

    let elapsed = t_start.elapsed();
    let sha256_rcvd = sha256_hex(&received);

    anyhow::ensure!(
        sha256_sent == sha256_rcvd,
        "sha256 MISMATCH:\n  sent: {}\n  rcvd: {}",
        sha256_sent,
        sha256_rcvd,
    );

    println!(
        "  sha256 match ✓  ({}...)   {:.3}s   {} bytes",
        &sha256_sent[..16],
        elapsed.as_secs_f64(),
        bytes,
    );
    println!();
    Ok(())
}
