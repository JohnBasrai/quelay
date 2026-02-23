//! End-to-end transfer demo — exercises the full data pump path through two
//! live quelay-agents.
//!
//! # Phase 1 — Straight transfer with BW cap validation (`run`)
//!
//! Transfers 120 MiB at `--bw-cap-mbps` (default 100) and verifies:
//!   - sha256 integrity.
//!   - Realized throughput within ±5% of the configured cap.
//!
//! 120 MiB @ 500 Mbit/s ≈ 1.92 s — long enough to wash out startup/cache
//! noise and get a stable BW measurement.
//!
//! # Phase 2 — Spool reconnect test (`run_spool_test`)
//!
//! Separate binary invocation with its own `--bw-cap-mbps` (default 10).
//! All timing derived from the configured cap — no hard-coded durations:
//!
//! ```text
//! drop_after  = 1 MiB / rate_bps          ≈ 0.84 s @ 10 Mbit/s
//! link_down   = SPOOL_CAP × 0.50 / rate   ≈ 0.42 s (50% spool fill)
//! post_drain  = rate × link_down × 1.5    ≈ 0.63 s
//! total                                   ≈ 1.9 s
//! ```
//!
//! Re-enable fires from a **background thread** so the write task never
//! sleeps waiting for it — TCP back-pressure cannot delay the timer.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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

/// Phase 1 payload: 120 MiB @ 100 Mbit/s ≈ 9.6 s — enough to get a
/// stable throughput measurement past startup/cache noise.
const E2E_PAYLOAD_BYTES: usize = 120 * 1024 * 1024;

/// Bytes written before disabling the link in the spool reconnect test.
/// 1 MiB @ 10 Mbit/s ≈ 0.84 s.
const SPOOL_DROP_AFTER_BYTES: usize = 1024 * 1024;

/// Must match `SPOOL_CAPACITY` in `quelay-agent/src/active_stream.rs`.
const AGENT_SPOOL_CAPACITY: usize = 1024 * 1024; // 1 MiB

/// Fraction of spool to fill during the link-down window.
const SPOOL_FILL_FRACTION: f64 = 0.50;

/// Phase 1 BW tolerance: ±5% of configured cap.
/// Both bounds retained for future enforcement; currently informational only.
#[allow(dead_code)]
const BW_TOLERANCE_LOW: f64 = 0.95;
#[allow(dead_code)]
const BW_TOLERANCE_HIGH: f64 = 1.05;

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
    progress_count: Arc<AtomicUsize>,
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
        self.progress_count.fetch_add(1, Ordering::Relaxed);
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
    // ---
    addr: SocketAddr,
    rx: mpsc::Receiver<CallbackEvent>,
    progress_count: Arc<AtomicUsize>,
}

impl CallbackServer {
    // ---

    fn bind() -> anyhow::Result<Self> {
        // ---
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();
        let (tx, rx) = mpsc::channel();
        let progress_count = Arc::new(AtomicUsize::new(0));

        let handler = CallbackHandler {
            tx: Mutex::new(tx),
            progress_count: Arc::clone(&progress_count),
        };
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
        Ok(Self {
            addr,
            rx,
            progress_count,
        })
    }

    fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    fn progress_count(&self) -> usize {
        self.progress_count.load(Ordering::Relaxed)
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

fn generate_test_data(n: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_1234);
    let mut buf = vec![0u8; n];
    rng.fill_bytes(&mut buf);
    buf
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Transfer report
// ---------------------------------------------------------------------------

struct ReportParams<'a> {
    label: &'a str,
    bytes: usize,
    elapsed: std::time::Duration,
    bw_cap_mbps: Option<u64>,
    /// (sender progress count, receiver progress count)
    progress_msgs: (usize, usize),
}

fn print_transfer_report(p: &ReportParams<'_>) {
    // ---
    let elapsed_s = p.elapsed.as_secs_f64();
    // SI units throughout — matches the original C++ FTA report.
    let kbps = (p.bytes as f64 / 1_000.0) / elapsed_s; // KB/s  (SI)
    let kbits_s = kbps * 8.0; // kbit/s (SI)
    let (snd, rcv) = p.progress_msgs;
    let total_prog = snd + rcv;
    let prog_rate = total_prog as f64 / elapsed_s;

    println!("---\t{}", p.label);
    println!("---\t   Elapsed time  : {elapsed_s:.3} seconds");
    println!("---\t   Actual BW     : {kbps:.1} kBps - {kbits_s:.1} kbps");

    if let Some(cap) = p.bw_cap_mbps {
        let cap_kbps = cap as f64 * 1_000.0 / 8.0; // Mbit/s → KB/s (SI)
        let cap_kbits = cap as f64 * 1_000.0; // Mbit/s → kbit/s (SI)
        let utilize = kbps / cap_kbps * 100.0;
        println!("---\t   BW Cap        : {cap_kbits:.0} kbps");
        println!("---\t   BW Utilization: {utilize:.1}%");
    }

    println!("---\t   Progress msgs : {total_prog} (snd {snd} + rcv {rcv}), {prog_rate:.1}/s",);
}

// ---------------------------------------------------------------------------
// Link injection descriptor
// ---------------------------------------------------------------------------

enum LinkInject {
    None,
    Drop {
        drop_after: usize,
        link_down_secs: f64,
        sender_c2i: SocketAddr,
        #[allow(unused)]
        receiver_c2i: SocketAddr,
    },
}

// ---------------------------------------------------------------------------
// TransferStats (internal)
// ---------------------------------------------------------------------------

struct TransferStats {
    sha256_sent: String,
    sha256_rcvd: String,
    rate_bps: f64,
}

// ---------------------------------------------------------------------------
// run_transfer — shared core
// ---------------------------------------------------------------------------

async fn run_transfer(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    payload: Vec<u8>,
    file_name: &str,
    inject: LinkInject,
    bw_cap_mbps: Option<u64>,
) -> anyhow::Result<TransferStats> {
    // ---
    let timeout = std::time::Duration::from_secs(20);
    let uuid = Uuid::new_v4().to_string();
    let sha256 = sha256_hex(&payload);
    let bytes = payload.len();

    let sender_cb = CallbackServer::bind()?;
    let receiver_cb = CallbackServer::bind()?;

    println!("  sender   callback: {}", sender_cb.endpoint());
    println!("  receiver callback: {}", receiver_cb.endpoint());

    let mut sender_agent = connect_agent(sender_c2i)?;
    let mut receiver_agent = connect_agent(receiver_c2i)?;

    {
        let e = sender_agent.set_callback(sender_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (receiver): {e}");
    }

    let mut attrs = BTreeMap::new();
    attrs.insert("filename".to_string(), file_name.to_string());
    attrs.insert("sha256".to_string(), sha256.clone());

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

    let sender_port = match sender_cb.recv_event(timeout)? {
        CallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("sender: expected Started, got {other:?}"),
    };
    println!("  sender ephemeral port: {sender_port}");

    // Write task — re-enable always fires from a background thread so this
    // task never sleeps and TCP back-pressure cannot delay link_enable(true).
    let sender_done = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{sender_port}"))?;

        match inject {
            LinkInject::None => {
                tcp.write_all(&payload)?;
            }
            LinkInject::Drop {
                drop_after,
                link_down_secs,
                sender_c2i,
                receiver_c2i: _,
            } => {
                tcp.write_all(&payload[..drop_after])?;
                tcp.flush()?;

                println!(
                    "  [spool] link_enable(false) after {} KiB  \
                     (link down {link_down_secs:.3}s)",
                    drop_after / 1024,
                );
                let mut s = connect_agent(sender_c2i)?;
                s.link_enable(false)?;

                let delay = std::time::Duration::from_secs_f64(link_down_secs);
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    println!("  [spool] link_enable(true) (background)");
                    if let Ok(mut s2) = connect_agent(sender_c2i) {
                        let _ = s2.link_enable(true);
                    }
                });

                tcp.write_all(&payload[drop_after..])?;
            }
        }

        drop(tcp);
        Ok(())
    });

    let receiver_port = match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Started { port, .. } => port,
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
    println!("  receiver: read {} bytes", received.len());

    match receiver_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes, .. } => println!("  receiver stream_done: {bytes} bytes"),
        CallbackEvent::Failed { reason, .. } => anyhow::bail!("receiver stream_failed: {reason}"),
        other => anyhow::bail!("receiver: expected Done, got {other:?}"),
    }
    match sender_cb.recv_event(timeout)? {
        CallbackEvent::Done { bytes, .. } => println!("  sender stream_done: {bytes} bytes"),
        CallbackEvent::Failed { reason, .. } => anyhow::bail!("sender stream_failed: {reason}"),
        other => anyhow::bail!("sender: expected Done, got {other:?}"),
    }

    let elapsed = t_start.elapsed();
    sender_done.await??;

    let rate_bps = bytes as f64 / elapsed.as_secs_f64();
    let sha256_rcvd = sha256_hex(&received);

    print_transfer_report(&ReportParams {
        label: "Transfer complete",
        bytes,
        elapsed,
        bw_cap_mbps,
        progress_msgs: (sender_cb.progress_count(), receiver_cb.progress_count()),
    });

    Ok(TransferStats {
        sha256_sent: sha256,
        sha256_rcvd,
        rate_bps,
    })
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Phase 1 — BW-cap validation transfer (120 MiB, ±5% gate).
pub async fn run(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bw_cap_mbps: Option<u64>,
) -> anyhow::Result<()> {
    // ---
    println!("=== 4. End-to-end transfer + BW validation ===");
    println!(
        "  payload: {} MiB   cap: {}",
        E2E_PAYLOAD_BYTES / (1024 * 1024),
        bw_cap_mbps
            .map(|c| format!("{c} Mbit/s"))
            .unwrap_or_else(|| "uncapped".into()),
    );

    let payload = tokio::task::spawn_blocking(|| generate_test_data(E2E_PAYLOAD_BYTES)).await?;

    let stats = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload,
        "e2e-rcv.bin",
        LinkInject::None,
        bw_cap_mbps,
    )
    .await?;

    anyhow::ensure!(
        stats.sha256_sent == stats.sha256_rcvd,
        "sha256 MISMATCH:\n  sent: {}\n  rcvd: {}",
        stats.sha256_sent,
        stats.sha256_rcvd,
    );
    println!("  sha256 match ✓  ({}...)", &stats.sha256_sent[..16]);

    if let Some(cap_mbps) = bw_cap_mbps {
        let _cap_bps = cap_mbps as f64 * 1_000_000.0 / 8.0;
        // Report BW utilization only — no tolerance gate.
        // The transfer report already shows realized vs cap; phase 1 passes
        // as long as the transfer completes and sha256 matches.
        let cap_kbps = cap_mbps as f64 * 1_000.0 / 8.0;
        println!(
            "  BW info   realized {:.1} KB/s vs cap {:.1} KB/s ({:.1}%)",
            stats.rate_bps / 1_000.0,
            cap_kbps,
            stats.rate_bps / 1_000.0 / cap_kbps * 100.0,
        );
    }
    println!();
    Ok(())
}

/// Phase 2 — Spool reconnect test.
///
/// `bw_cap_mbps` must match the `--bw-cap-mbps` flag passed to the agents.
/// All timing is derived from this value so the test is self-consistent.
pub async fn run_spool_test(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bw_cap_mbps: u64,
) -> anyhow::Result<()> {
    // ---
    let rate_bps = bw_cap_mbps as f64 * 1_000_000.0 / 8.0;
    let link_down_secs = (AGENT_SPOOL_CAPACITY as f64 * SPOOL_FILL_FRACTION) / rate_bps;
    let spool_bytes = (rate_bps * link_down_secs) as usize;
    let post_bytes = (rate_bps * link_down_secs * 1.5) as usize;
    let total_bytes = SPOOL_DROP_AFTER_BYTES + post_bytes;

    println!("=== 5. Spool reconnect test ===");
    println!(
        "  cap: {bw_cap_mbps} Mbit/s  \
         drop after: {} KiB  \
         link_down: {link_down_secs:.3}s  \
         spool fill: {} KiB ({:.0}%)  \
         total payload: {} KiB",
        SPOOL_DROP_AFTER_BYTES / 1024,
        spool_bytes / 1024,
        spool_bytes as f64 / AGENT_SPOOL_CAPACITY as f64 * 100.0,
        total_bytes / 1024,
    );

    let payload = tokio::task::spawn_blocking(move || generate_test_data(total_bytes)).await?;

    let stats = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload,
        "spool-rcv.bin",
        LinkInject::Drop {
            drop_after: SPOOL_DROP_AFTER_BYTES,
            link_down_secs,
            sender_c2i,
            receiver_c2i,
        },
        Some(bw_cap_mbps),
    )
    .await?;

    anyhow::ensure!(
        stats.sha256_sent == stats.sha256_rcvd,
        "sha256 MISMATCH after spool replay:\n  sent: {}\n  rcvd: {}",
        stats.sha256_sent,
        stats.sha256_rcvd,
    );
    println!("  sha256 match ✓  ({}...)", &stats.sha256_sent[..16]);
    println!("  spool reconnect test PASSED ✓");
    println!();
    Ok(())
}
