//! End-to-end transfer demo — exercises the full data pump through two live
//! agents.
//!
//! # What this tests
//!
//! 1. Sender calls `set_callback` → agent registers callback endpoint.
//! 2. Sender calls `stream_start` → agent opens QUIC stream, writes header,
//!    opens ephemeral TCP port, fires `stream_started` callback.
//! 3. Sender connects to ephemeral port, writes payload bytes, closes.
//! 4. Agent pipes TCP → QUIC; receiver agent reads QUIC → ephemeral TCP port,
//!    fires `stream_started` on the receiver callback.
//! 5. Receiver client connects to its ephemeral port, reads bytes.
//! 6. Receiver agent fires `stream_done`; sender agent receives
//!    `WormholeMsg::Done`, fires `stream_done` on sender callback.
//! 7. Assert: received bytes match sent bytes.
//!
//! # Architecture
//!
//! `CallbackServer` runs a `TServer` on a background thread.
//! `CallbackHandler` implements `QueLayCallbackSyncHandler` and uses
//! `std::sync::mpsc` to signal the async test task.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{mpsc, Mutex};

// ---

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

/// Events delivered from the callback handler to the async test task.
#[derive(Debug)]
#[allow(dead_code)]
enum CallbackEvent {
    // ---
    Started { uuid: String, port: u16 },
    Done { uuid: String, bytes: u64 },
    Failed { uuid: String, reason: String },
}

// ---------------------------------------------------------------------------
// CallbackHandler
// ---------------------------------------------------------------------------

/// Implements `QueLayCallbackSyncHandler`.  Forwards events to the test task
/// via an `mpsc` channel.
struct CallbackHandler {
    // ---
    tx: Mutex<mpsc::Sender<CallbackEvent>>,
}

// ---

impl QueLayCallbackSyncHandler for CallbackHandler {
    // ---

    fn handle_ping(&self) -> thrift::Result<()> {
        // ---
        Ok(())
    }

    fn handle_stream_started(
        &self,
        uuid: String,
        _info: StreamInfo,
        port: i32,
    ) -> thrift::Result<()> {
        // ---
        tracing::info!(%uuid, port, "callback: stream_started");
        let _ = self.tx.lock().unwrap().send(CallbackEvent::Started {
            uuid,
            port: port as u16,
        });
        Ok(())
    }

    fn handle_stream_progress(
        &self,
        uuid: String,
        _progress: quelay_thrift::ProgressInfo,
    ) -> thrift::Result<()> {
        // ---
        tracing::debug!(%uuid, "callback: stream_progress");
        Ok(())
    }

    fn handle_stream_done(&self, uuid: String, bytes_transferred: i64) -> thrift::Result<()> {
        tracing::info!(%uuid, bytes_transferred, "callback: stream_done");
        // ---

        let _ = self.tx.lock().unwrap().send(CallbackEvent::Done {
            uuid,
            bytes: bytes_transferred as u64,
        });
        Ok(())
    }

    fn handle_stream_failed(
        &self,
        uuid: String,
        _code: FailReason,
        reason: String,
    ) -> thrift::Result<()> {
        // ---
        tracing::warn!(%uuid, %reason, "callback: stream_failed");

        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(CallbackEvent::Failed { uuid, reason });
        Ok(())
    }

    fn handle_link_status_update(&self, _link_state: LinkState) -> thrift::Result<()> {
        // ---
        Ok(())
    }

    fn handle_queue_status_update(&self, _queue_status: QueueStatus) -> thrift::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CallbackServer
// ---------------------------------------------------------------------------

/// Thin wrapper that binds a `TServer` on an OS-assigned port and returns
/// the bound address and an event receiver.
struct CallbackServer {
    addr: SocketAddr,
    rx: mpsc::Receiver<CallbackEvent>,
}

// ---

impl CallbackServer {
    // ---
    /// Bind on `127.0.0.1:0`, spawn server thread, return handle.
    ///
    /// A `ready_tx` channel signals the caller as soon as `TServer::listen`
    /// has bound the socket, eliminating the TOCTOU probe-loop approach.
    fn bind() -> anyhow::Result<Self> {
        // ---
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();

        let (tx, rx) = mpsc::channel();
        let handler = CallbackHandler { tx: Mutex::new(tx) };
        let processor = QueLayCallbackSyncProcessor::new(handler);

        // One-shot channel: server thread sends `()` once listen() is about
        // to block (i.e. the socket is bound and accepting).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

        std::thread::Builder::new()
            .name("quelay-cb-server".into())
            .spawn(move || {
                drop(listener); // release so TServer can re-bind the same addr
                let mut server = TServer::new(
                    TBufferedReadTransportFactory::new(),
                    TBinaryInputProtocolFactory::new(),
                    TBufferedWriteTransportFactory::new(),
                    TBinaryOutputProtocolFactory::new(),
                    processor,
                    4,
                );
                // Signal ready before entering the blocking accept loop.
                // Small race remains (bind vs listen), but TServer::listen
                // binds synchronously before blocking — good enough for tests.
                let _ = ready_tx.send(());
                if let Err(e) = server.listen(&addr_str) {
                    tracing::warn!("callback server exiting: {e}");
                }
            })?;

        // Wait up to 2 s for the server to signal ready.
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("callback server did not start within 2s"))?;

        // Brief yield so the OS has time to complete the bind before we
        // hand the address to the agent.
        std::thread::sleep(std::time::Duration::from_millis(10));

        Ok(CallbackServer { addr, rx })
    }

    fn endpoint(&self) -> String {
        // ---
        self.addr.to_string()
    }

    fn recv_timeout(&self, timeout: std::time::Duration) -> anyhow::Result<CallbackEvent> {
        // ---
        self.rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("callback recv timeout: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Agent C2I client helper
// ---------------------------------------------------------------------------

fn connect_agent(addr: SocketAddr) -> anyhow::Result<impl TQueLayAgentSyncClient> {
    // ---

    let mut channel = TTcpChannel::new();
    channel.open(addr.to_string())?;
    let (rx, tx) = channel.split()?;
    Ok(QueLayAgentSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    ))
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// Run the end-to-end transfer test against two live agents.
///
/// `sender_c2i`   — C2I address of the sending agent (agent-1).
/// `receiver_c2i` — C2I address of the receiving agent (agent-2).
pub async fn run(sender_c2i: SocketAddr, receiver_c2i: SocketAddr) -> anyhow::Result<()> {
    // ---

    let timeout = std::time::Duration::from_secs(10);
    let payload: Vec<u8> = (0u8..=255).cycle().take(256 * 1024).collect(); // 256 KiB
    let uuid = Uuid::new_v4().to_string();

    println!("=== 4. End-to-end transfer demo ===");
    println!("  payload: {} bytes, uuid: {}", payload.len(), uuid);

    // --- spin up callback servers for both sides ---
    let sender_cb = CallbackServer::bind()?;
    let receiver_cb = CallbackServer::bind()?;

    println!("  sender   callback server: {}", sender_cb.endpoint());
    println!("  receiver callback server: {}", receiver_cb.endpoint());

    // --- connect to both agents ---
    let mut sender_agent = connect_agent(sender_c2i)?;
    let mut receiver_agent = connect_agent(receiver_c2i)?;

    // --- register callbacks ---
    let err = sender_agent.set_callback(sender_cb.endpoint())?;
    anyhow::ensure!(err.is_empty(), "set_callback (sender): {err}");

    let err = receiver_agent.set_callback(receiver_cb.endpoint())?;
    anyhow::ensure!(err.is_empty(), "set_callback (receiver): {err}");

    // --- start the stream ---
    let mut attrs = BTreeMap::new();
    attrs.insert("filename".to_string(), "e2e-test.bin".to_string());

    let result = sender_agent.stream_start(
        uuid.clone(),
        StreamInfo {
            size_bytes: Some(payload.len() as i64),
            attrs: Some(attrs),
        },
        0, // priority: lowest
    )?;

    anyhow::ensure!(
        result.err_msg.as_deref().unwrap_or("").is_empty(),
        "stream_start failed: {:?}",
        result.err_msg
    );
    println!(
        "  stream_start accepted, queue position: {:?}",
        result.queue_position
    );

    // --- wait for sender stream_started → connect and write ---
    let sender_port = match sender_cb.recv_timeout(timeout)? {
        CallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("sender: expected StreamStarted, got {other:?}"),
    };
    println!("  sender ephemeral port: {sender_port}");

    let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{sender_port}"))?;
    {
        use std::io::Write;
        tcp.write_all(&payload)?;
    }
    drop(tcp); // EOF → agent detects and shuts down QUIC stream

    println!("  sender: wrote {} bytes, closed TCP", payload.len());

    // --- wait for receiver stream_started → connect and read ---
    let receiver_port = match receiver_cb.recv_timeout(timeout)? {
        CallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("receiver: expected StreamStarted, got {other:?}"),
    };
    println!("  receiver ephemeral port: {receiver_port}");

    let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
    let mut received = Vec::new();
    {
        use std::io::Read;
        tcp.read_to_end(&mut received)?;
    }
    println!("  receiver: read {} bytes", received.len());

    // --- wait for both stream_done callbacks ---
    match receiver_cb.recv_timeout(timeout)? {
        CallbackEvent::Done { bytes, .. } => {
            println!("  receiver stream_done: {bytes} bytes");
        }
        other => anyhow::bail!("receiver: expected StreamDone, got {other:?}"),
    }

    match sender_cb.recv_timeout(timeout)? {
        CallbackEvent::Done { bytes, .. } => {
            println!("  sender stream_done: {bytes} bytes");
        }
        other => anyhow::bail!("sender: expected StreamDone, got {other:?}"),
    }

    // --- assert ---
    anyhow::ensure!(
        received == payload,
        "payload mismatch: sent {} bytes, received {} bytes, match: {}",
        payload.len(),
        received.len(),
        received == payload,
    );

    println!("  payload matches: true ✓");
    println!();

    Ok(())
}
