//! End-to-end transfer demo — exercises the full data pump path through two
//! live quelay-agents.
//!
//! # Straight transfer (`run`)
//!
//! 1. Generate test data (pseudo-random, deterministic seed).
//! 2. Compute sha256 of the sent bytes.
//! 3. Call `stream_start` on the sender agent with `attrs["sha256"]` and
//!    `attrs["filename"]` = `"e2e-rcv.bin"`.
//! 4. Write bytes to the sender's ephemeral TCP port.
//! 5. Read bytes from the receiver's ephemeral TCP port.
//! 6. Verify sha256 of received bytes matches sent bytes.
//! 7. Report realized throughput vs configured rate cap.
//!
//! # Spool / reconnect test (`run_spool_test`)
//!
//! Same as above, but after 5 MiB are written to the sender port:
//!   - Call `link_enable(false)` on **both** agents simultaneously.
//!   - Continue writing from the sender (data spools in-memory).
//!   - After 1 s call `link_enable(true)` on both agents.
//!   - Wait for transfer to complete, verify sha256.
//!
//! The test file is large enough (~20 MiB) that the link drop reliably
//! happens mid-transfer at the 5 MiB mark.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{mpsc, Mutex};
use std::time::Instant;

// ---

use rand::{RngCore, SeedableRng};
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
// Constants
// ---------------------------------------------------------------------------

/// Test payload size for the straight e2e transfer.
///
/// 4 MiB — fast enough on loopback that CI finishes in < 1 s, large enough
/// to exercise multi-chunk framing.
const E2E_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Test payload size for the spool reconnect test.
///
/// 20 MiB — large enough that the 5 MiB drop point is reliably mid-transfer
/// even at the bandwidth-capped rate configured in `ci-spool-test.sh`.
const SPOOL_PAYLOAD_BYTES: usize = 20 * 1024 * 1024;

/// Bytes written before calling `link_enable(false)` in the spool test.
const SPOOL_DROP_AFTER_BYTES: usize = 5 * 1024 * 1024;

/// Seconds to hold the link down before calling `link_enable(true)`.
const SPOOL_LINK_DOWN_SECS: u64 = 1;

// ---------------------------------------------------------------------------
// CallbackEvent
// ---------------------------------------------------------------------------

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

struct CallbackHandler {
    // ---
    tx: Mutex<mpsc::Sender<CallbackEvent>>,
}

// ---

impl QueLayCallbackSyncHandler for CallbackHandler {
    // ---

    fn handle_ping(&self) -> thrift::Result<()> {
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
        // ---
        tracing::info!(%uuid, bytes_transferred, "callback: stream_done");
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
        // ---
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();

        let (tx, rx) = mpsc::channel();
        let handler = CallbackHandler { tx: Mutex::new(tx) };
        let processor = QueLayCallbackSyncProcessor::new(handler);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

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
// Agent C2I client
// ---------------------------------------------------------------------------

fn connect_agent(addr: SocketAddr) -> anyhow::Result<impl TQueLayAgentSyncClient> {
    // ---
    let mut ch = TTcpChannel::new();
    ch.open(addr.to_string())?;
    let (rx, tx) = ch.split()?;
    Ok(QueLayAgentSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    ))
}

// ---------------------------------------------------------------------------
// Test data generation
// ---------------------------------------------------------------------------

/// Generate `n` pseudo-random bytes from a fixed seed.
///
/// Using a fixed seed makes the sha256 precomputable and reproducible
/// across runs, which is useful for debugging.  The RNG is not
/// cryptographically important here.
fn generate_test_data(n: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_1234);
    let mut buf = vec![0u8; n];
    rng.fill_bytes(&mut buf);
    buf
}

/// SHA-256 digest of `data`, returned as a lowercase hex string.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Throughput report
// ---------------------------------------------------------------------------

fn print_transfer_report(
    label: &str,
    bytes: usize,
    elapsed: std::time::Duration,
    bw_cap_mbps: Option<u64>,
) {
    // ---
    let elapsed_s = elapsed.as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let mbit_s = (bytes as f64 * 8.0) / 1_000_000.0 / elapsed_s;

    println!("--- {label}");
    println!("---    File size     : {mib:.2} MiB");
    println!("---    Elapsed time  : {elapsed_s:.3} s");
    println!("---    Realized rate : {mbit_s:.1} Mbit/s");

    if let Some(cap) = bw_cap_mbps {
        let utilized = mbit_s / cap as f64 * 100.0;
        println!("---    Rate cap      : {cap} Mbit/s");
        println!("---    BW utilized   : {utilized:.1}%");
    }
}

// ---------------------------------------------------------------------------
// Core transfer logic (shared by straight and spool tests)
// ---------------------------------------------------------------------------

/// What to do mid-transfer for the spool test.
enum LinkInject {
    /// Straight transfer — do nothing.
    None,
    /// Drop the link after `drop_after` bytes, hold down for `hold_secs`,
    /// then re-enable.  Uses separate Thrift connections to both agents so
    /// the timing is as tight as possible.
    Drop {
        drop_after: usize,
        hold_secs: u64,
        sender_c2i: SocketAddr,
        receiver_c2i: SocketAddr,
    },
}

struct TransferResult {
    sha256_sent: String,
    sha256_rcvd: String,
}

async fn run_transfer(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    payload: Vec<u8>,
    file_name: &str,
    inject: LinkInject,
    bw_cap_mbps: Option<u64>,
) -> anyhow::Result<TransferResult> {
    // ---
    let timeout = std::time::Duration::from_secs(30);
    let uuid = Uuid::new_v4().to_string();
    let sha256 = sha256_hex(&payload);
    let bytes = payload.len();

    // Spin up callback servers.
    let sender_cb = CallbackServer::bind()?;
    let receiver_cb = CallbackServer::bind()?;

    println!("  sender   callback: {}", sender_cb.endpoint());
    println!("  receiver callback: {}", receiver_cb.endpoint());

    // Connect to both agents.
    let mut sender_agent = connect_agent(sender_c2i)?;
    let mut receiver_agent = connect_agent(receiver_c2i)?;

    // Register callbacks.
    {
        let e = sender_agent.set_callback(sender_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (receiver): {e}");
    }

    // Build attrs.
    let mut attrs = BTreeMap::new();
    attrs.insert("filename".to_string(), file_name.to_string());
    attrs.insert("sha256".to_string(), sha256.clone());

    // stream_start — start the clock here.
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

    // Wait for sender stream_started.
    let sender_port = match sender_cb.recv_event(timeout)? {
        CallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("sender: expected Started, got {other:?}"),
    };
    println!("  sender ephemeral port: {sender_port}");

    // Write payload to sender in a blocking task (may be large).
    let sender_done = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;

        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{sender_port}"))?;

        match inject {
            LinkInject::None => {
                tcp.write_all(&payload)?;
            }
            LinkInject::Drop {
                drop_after,
                hold_secs,
                sender_c2i,
                receiver_c2i,
            } => {
                // Write first `drop_after` bytes.
                tcp.write_all(&payload[..drop_after])?;
                tcp.flush()?;

                // Disable link on both agents simultaneously.
                println!("  [spool] link_enable(false) after {drop_after} bytes");
                let mut s = connect_agent(sender_c2i)?;
                let mut r = connect_agent(receiver_c2i)?;
                s.link_enable(false)?;
                r.link_enable(false)?;

                // Keep writing — data will spool in-memory.
                // (TCP is still connected to the agent's ephemeral port)
                std::thread::sleep(std::time::Duration::from_secs(hold_secs));

                // Re-enable link on both agents.
                println!("  [spool] link_enable(true) after {hold_secs}s");
                s.link_enable(true)?;
                r.link_enable(true)?;

                // Write remainder.
                tcp.write_all(&payload[drop_after..])?;
            }
        }

        drop(tcp); // EOF
        Ok(())
    });

    // Wait for receiver stream_started.
    let receiver_port = match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("receiver: expected Started, got {other:?}"),
    };
    println!("  receiver ephemeral port: {receiver_port}");

    // Read received bytes.
    let mut received = Vec::with_capacity(bytes);
    {
        use std::io::Read;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.read_to_end(&mut received)?;
    }
    println!("  receiver: read {} bytes", received.len());

    // Wait for stream_done on both sides.
    match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes, .. } => println!("  receiver stream_done: {bytes} bytes"),
        other => anyhow::bail!("receiver: expected Done, got {other:?}"),
    }
    match sender_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes, .. } => println!("  sender stream_done: {bytes} bytes"),
        other => anyhow::bail!("sender: expected Done, got {other:?}"),
    }

    let elapsed = t_start.elapsed();

    // Ensure sender write task completed without error.
    sender_done.await??;

    let sha256_rcvd = sha256_hex(&received);
    print_transfer_report("Transfer complete", bytes, elapsed, bw_cap_mbps);

    Ok(TransferResult {
        sha256_sent: sha256,
        sha256_rcvd,
    })
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Straight end-to-end transfer test.
pub async fn run(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bw_cap_mbps: Option<u64>,
) -> anyhow::Result<()> {
    // ---
    println!("=== 4. End-to-end transfer demo ===");
    println!(
        "  payload: {E2E_PAYLOAD_BYTES} bytes ({:.1} MiB)",
        E2E_PAYLOAD_BYTES as f64 / 1024.0 / 1024.0
    );

    let payload = tokio::task::spawn_blocking(|| generate_test_data(E2E_PAYLOAD_BYTES)).await?;

    let result = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload,
        "e2e-rcv.bin",
        LinkInject::None,
        bw_cap_mbps,
    )
    .await?;

    anyhow::ensure!(
        result.sha256_sent == result.sha256_rcvd,
        "sha256 MISMATCH:\n  sent: {}\n  rcvd: {}",
        result.sha256_sent,
        result.sha256_rcvd,
    );
    println!("  sha256 match ✓  ({})", &result.sha256_sent[..16]);
    println!();

    Ok(())
}

/// Spool reconnect test — drops the link mid-transfer and verifies the
/// spool replays correctly so the received file is identical to the sent file.
pub async fn run_spool_test(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bw_cap_mbps: Option<u64>,
) -> anyhow::Result<()> {
    // ---
    println!("=== 5. Spool reconnect test ===");
    println!(
        "  payload: {SPOOL_PAYLOAD_BYTES} bytes ({:.1} MiB), drop after {} MiB",
        SPOOL_PAYLOAD_BYTES as f64 / 1024.0 / 1024.0,
        SPOOL_DROP_AFTER_BYTES / (1024 * 1024),
    );

    let payload = tokio::task::spawn_blocking(|| generate_test_data(SPOOL_PAYLOAD_BYTES)).await?;

    let result = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload,
        "spool-rcv.bin",
        LinkInject::Drop {
            drop_after: SPOOL_DROP_AFTER_BYTES,
            hold_secs: SPOOL_LINK_DOWN_SECS,
            sender_c2i,
            receiver_c2i,
        },
        bw_cap_mbps,
    )
    .await?;

    anyhow::ensure!(
        result.sha256_sent == result.sha256_rcvd,
        "sha256 MISMATCH after spool replay:\n  sent: {}\n  rcvd: {}",
        result.sha256_sent,
        result.sha256_rcvd,
    );
    println!("  sha256 match ✓  ({})", &result.sha256_sent[..16]);
    println!("  spool reconnect test PASSED ✓");
    println!();

    Ok(())
}
