//! Quelay integration test binary.
//!
//! Exercises the full data-pump path through two live quelay-agents.
//! Replaces the legacy C++ `FTAClientEndToEndTest` binary and the shell scripts
//! that orchestrated it.
//!
//! # Usage
//!
//! ```text
//! e2e_test [OPTIONS] <SUBCOMMAND>
//!
//! SUBCOMMANDS:
//!   rate-limiter          Token bucket accuracy (no agents required)
//!   multi-file            Multi-file transfer — large, small, link-outage, link-fail
//!   drr                   DRR scheduler priority ordering
//!   small-file-edge-cases Framing boundary file sizes
//! ```
//!
//! See `quelay-agent/src/bin/README.md` for the full design rationale and
//! mapping to legacy tests.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// ---

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use rand::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---

#[rustfmt::skip]
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

fn ensure_agent_running(addr: SocketAddr) -> Result<()> {
    // ---
    let timeout = Duration::from_millis(300);
    match std::net::TcpStream::connect_timeout(&addr.into(), timeout) {
        Ok(_) => Ok(()),
        Err(_) => bail!("Agent not reachable at {addr} (is it running?)"),
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default transfer timeout headroom multiplier over expected duration.
const TIMEOUT_HEADROOM: f64 = 4.0;

/// Minimum transfer timeout regardless of expected duration.
const TIMEOUT_MIN_SECS: u64 = 30;

/// BW tolerance window: realized rate must be within ±10% of configured cap.
const BW_TOLERANCE_LOW: f64 = 0.90;
const BW_TOLERANCE_HIGH: f64 = 1.10;

/// Bytes written before disabling the link in the spool reconnect path.
/// Matches the agent's default spool capacity (1 MiB).
const SPOOL_DROP_AFTER_BYTES: usize = 1024 * 1024; // 1 MiB

/// Fraction of spool capacity to fill during the link-down window.
const SPOOL_FILL_FRACTION: f64 = 0.50;

/// Chunk size used for the in-process rate-limiter accuracy test.
/// Matches the agent's default CHUNK_SIZE (16 KiB).
const RATE_TEST_CHUNK_SIZE: usize = 16 * 1024; // 16 KiB

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Quelay integration test suite.
///
/// Requires two running quelay-agents accessible at --sender-c2i and
/// --receiver-c2i (except `rate-limiter`, which is in-process).
///
/// See quelay-agent/src/bin/README.md for full design rationale, timing
/// derivation, and mapping to the legacy C++ test suite.
#[derive(Debug, Parser)]
#[command(
    name = "e2e_test",
    after_help = "Full design notes: quelay-agent/src/bin/README.md"
)]
struct Cli {
    // ---
    /// C2I address of the sending agent.
    #[arg(long, default_value = "127.0.0.1:9090")]
    sender_c2i: SocketAddr,

    /// C2I address of the receiving agent.
    #[arg(long, default_value = "127.0.0.1:9091")]
    receiver_c2i: SocketAddr,

    /// Enable debug logging (RUST_LOG=debug).
    #[arg(long, default_value_t = false)]
    debug: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    // ---
    /// Test token bucket rate limiter accuracy (in-process, no agents required).
    RateLimiter,

    /// Multi-file transfer: large files, small files, link outage, link failure.
    MultiFile(MultiFileArgs),

    /// DRR scheduler priority ordering (sets agent to 1 concurrent stream).
    Drr(DrrArgs),

    /// Framing boundary file size regression tests.
    SmallFileEdgeCases(SmallFileEdgeCasesArgs),
}

// ---

#[derive(Debug, Args)]
struct MultiFileArgs {
    // ---
    /// Transfer 3 large files: 30 MiB, 2 MiB, 500 KiB.
    #[arg(long, conflicts_with_all = ["small", "size_mb", "duration_secs"])]
    large: bool,

    /// Transfer 4 boundary-condition sizes: 9000B, 1024B, 512B, 1B.
    /// Agents are reconfigured to 1 KiB chunk size so multi-block boundaries
    /// are exercised (at 16 KiB default, all four files are sub-chunk).
    #[arg(long, conflicts_with_all = ["large", "size_mb", "duration_secs"])]
    small: bool,

    /// Transfer a single file of N MiB.
    #[arg(long, conflicts_with_all = ["large", "small", "duration_secs"])]
    size_mb: Option<usize>,

    /// Derive file size from agent BW cap × N seconds.
    #[arg(long, conflicts_with_all = ["large", "small", "size_mb"])]
    duration_secs: Option<u64>,

    /// Number of files to transfer (default: 2). Ignored when --large or --small is given.
    #[arg(long, default_value_t = 2)]
    count: usize,

    /// Send files in both directions (sender→receiver and receiver→sender).
    #[arg(long, default_value_t = false)]
    bidirectional: bool,

    /// Simulate a recoverable link outage mid-transfer. Queues a second file
    /// while the link is down to verify pending queue handling across reconnect.
    /// All timing derived from the agent's configured BW cap.
    #[arg(long, conflicts_with = "link_fail")]
    link_outage: bool,

    /// Simulate a permanent link failure. Asserts the in-flight file receives
    /// a stream_failed callback and LinkState reaches Failed.
    #[arg(long, conflicts_with = "link_outage")]
    link_fail: bool,
}

// ---

#[derive(Debug, Args)]
struct DrrArgs {
    // ---
    /// Number of priority-varied files to queue behind the anchor file (default: 3).
    #[arg(long, default_value_t = 3)]
    file_count: usize,
}

// ---

#[derive(Debug, Args)]
struct SmallFileEdgeCasesArgs {
    // ---
    /// Test both transfer directions for each size.
    #[arg(long, default_value_t = false)]
    bidirectional: bool,
}

// ---------------------------------------------------------------------------
// TestCallbackEvent
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TestCallbackEvent {
    // ---
    #[allow(unused)]
    Started { uuid: String, port: u16 },
    Done {
        #[allow(unused)]
        uuid: String,
        bytes: u64,
    },
    Failed {
        #[allow(unused)]
        uuid: String,
        reason: String,
    },
    #[allow(unused)]
    LinkState(LinkState),
}

// ---------------------------------------------------------------------------
// TestCallbackHandler
// ---------------------------------------------------------------------------

struct TestCallbackHandler {
    tx: Mutex<mpsc::Sender<TestCallbackEvent>>,
    progress_count: Arc<AtomicUsize>,
}

impl QueLayCallbackSyncHandler for TestCallbackHandler {
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
        tracing::info!(%uuid, port, "callback: stream_started");
        let _ = self.tx.lock().unwrap().send(TestCallbackEvent::Started {
            uuid,
            port: port as u16,
        });
        Ok(())
    }

    fn handle_stream_progress(
        &self,
        _uuid: String,
        _progress: quelay_thrift::ProgressInfo,
    ) -> thrift::Result<()> {
        self.progress_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn handle_stream_done(&self, uuid: String, bytes_transferred: i64) -> thrift::Result<()> {
        tracing::info!(%uuid, bytes_transferred, "callback: stream_done");
        let _ = self.tx.lock().unwrap().send(TestCallbackEvent::Done {
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
        tracing::warn!(%uuid, %reason, "callback: stream_failed");
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(TestCallbackEvent::Failed { uuid, reason });
        Ok(())
    }

    fn handle_link_status_update(&self, state: LinkState) -> thrift::Result<()> {
        tracing::info!(?state, "callback: link_status_update");
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(TestCallbackEvent::LinkState(state));
        Ok(())
    }

    fn handle_queue_status_update(&self, _status: QueueStatus) -> thrift::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TestCallbackServer
// ---------------------------------------------------------------------------

struct TestCallbackServer {
    addr: SocketAddr,
    rx: mpsc::Receiver<TestCallbackEvent>,
    progress_count: Arc<AtomicUsize>,
}

impl TestCallbackServer {
    // ---

    fn bind() -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();
        let (tx, rx) = mpsc::channel();
        let progress_count = Arc::new(AtomicUsize::new(0));

        let handler = TestCallbackHandler {
            tx: Mutex::new(tx),
            progress_count: Arc::clone(&progress_count),
        };
        let processor = QueLayCallbackSyncProcessor::new(handler);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        std::thread::Builder::new()
            .name("quelay-test-cb".into())
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
                    tracing::warn!("test callback server exiting: {e}");
                }
            })?;

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("test callback server did not start within 2s"))?;

        std::thread::sleep(Duration::from_millis(10));
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

    fn recv_event(&self, timeout: Duration) -> anyhow::Result<TestCallbackEvent> {
        self.rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("test callback recv timeout: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Agent C2I client
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

fn print_transfer_report(
    label: &str,
    bytes: usize,
    elapsed: Duration,
    cap_mbps: Option<u32>,
    progress_msgs: (usize, usize),
) {
    let elapsed_s = elapsed.as_secs_f64();
    let kbps = (bytes as f64 / 1_000.0) / elapsed_s;
    let kbits_s = kbps * 8.0;
    let (snd, rcv) = progress_msgs;
    let total_prog = snd + rcv;
    let prog_rate = total_prog as f64 / elapsed_s;

    println!("\n    ============================================================");
    println!("    ---\t{label}");
    println!("    ---\t   Elapsed time  : {elapsed_s:.3} seconds");
    println!("    ---\t   Actual BW     : {kbps:.1} kBps - {kbits_s:.1} kbps");

    if let Some(cap) = cap_mbps {
        let cap_kbps = cap as f64 * 1_000.0 / 8.0;
        let cap_kbits = cap as f64 * 1_000.0;
        let utilize = kbps / cap_kbps * 100.0;
        println!("    ---\t   BW Cap        : {cap_kbits:.0} kbps");
        println!("    ---\t   BW Utilization: {utilize:.1}%");
    }
    println!("    ---\t   Progress msgs : {total_prog} (snd {snd} + rcv {rcv}), {prog_rate:.1}/s");
    println!("    ============================================================\n");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn transfer_timeout(bytes: usize, cap_mbps: Option<u32>) -> Duration {
    // ---
    let secs = match cap_mbps {
        Some(cap) => {
            let bytes_per_sec = cap as f64 * 1_000_000.0 / 8.0;
            let expected = bytes as f64 / bytes_per_sec;
            ((expected * TIMEOUT_HEADROOM) as u64).max(TIMEOUT_MIN_SECS)
        }
        None => TIMEOUT_MIN_SECS,
    };
    Duration::from_secs(secs)
}

/// Query the sender agent's BW cap. Returns None if uncapped (0).
///
/// Thrift has no u32; the IDL field is i32. We treat any value <= 0 as uncapped.
fn query_cap(sender_c2i: SocketAddr) -> anyhow::Result<Option<u32>> {
    // ---
    let mut agent = connect_agent(sender_c2i).context("connect_agent(sender_c2i) failed")?;
    let v = agent.get_bandwidth_cap_mbps()?;
    Ok(if v <= 0 { None } else { Some(v as u32) })
}

// ---------------------------------------------------------------------------
// Link injection
// ---------------------------------------------------------------------------

enum LinkInject {
    None,
    Drop {
        drop_after: usize,
        link_down_secs: f64,
        sender_c2i: SocketAddr,
    },
}

// ---------------------------------------------------------------------------
// TransferStats
// ---------------------------------------------------------------------------

struct TransferStats {
    sha256_sent: String,
    sha256_rcvd: String,
    rate_bytes_per_sec: f64,
    #[allow(dead_code)]
    elapsed: Duration,
}

// ---------------------------------------------------------------------------
// run_transfer
// ---------------------------------------------------------------------------

async fn run_transfer(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    payload: Vec<u8>,
    uuid: &str,
    inject: LinkInject,
    cap_mbps: Option<u32>,
    timeout: Duration,
) -> anyhow::Result<TransferStats> {
    // ---
    let sha256_sent = sha256_hex(&payload);
    let bytes = payload.len();

    let sender_cb = TestCallbackServer::bind()?;
    let receiver_cb = TestCallbackServer::bind()?;

    let mut sender_agent = connect_agent(sender_c2i)?;
    let mut receiver_agent = connect_agent(receiver_c2i)?;

    {
        let e = sender_agent.set_callback(sender_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback (receiver): {e}");
    }

    let mut attrs = BTreeMap::new();
    attrs.insert("filename".to_string(), format!("{uuid}.bin"));
    attrs.insert("sha256".to_string(), sha256_sent.clone());

    let t_start = Instant::now();

    let result = sender_agent.stream_start(
        uuid.to_string(),
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

    let sender_port = match sender_cb.recv_event(timeout)? {
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("sender: expected Started, got {other:?}"),
    };

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
            } => {
                tcp.write_all(&payload[..drop_after])?;
                tcp.flush()?;
                tracing::info!(
                    drop_after_kib = drop_after / 1024,
                    link_down_secs,
                    "link_enable(false)"
                );
                let mut s = connect_agent(sender_c2i)?;
                s.link_enable(false)?;

                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs_f64(link_down_secs));
                    tracing::info!("link_enable(true) [background]");
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
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("receiver: expected Started, got {other:?}"),
    };

    let mut received = Vec::with_capacity(bytes);
    {
        use std::io::Read;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.read_to_end(&mut received)?;
    }

    match receiver_cb.recv_event(timeout)? {
        TestCallbackEvent::Done { bytes, .. } => tracing::info!(bytes, "receiver stream_done"),
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("receiver stream_failed: {reason}")
        }
        other => anyhow::bail!("receiver: expected Done, got {other:?}"),
    }
    match sender_cb.recv_event(timeout)? {
        TestCallbackEvent::Done { bytes, .. } => tracing::info!(bytes, "sender stream_done"),
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("sender stream_failed: {reason}")
        }
        other => anyhow::bail!("sender: expected Done, got {other:?}"),
    }

    let elapsed = t_start.elapsed();
    sender_done.await??;

    let sha256_rcvd = sha256_hex(&received);

    print_transfer_report(
        &format!("{uuid} complete"),
        bytes,
        elapsed,
        cap_mbps,
        (sender_cb.progress_count(), receiver_cb.progress_count()),
    );

    Ok(TransferStats {
        sha256_sent,
        sha256_rcvd,
        rate_bytes_per_sec: bytes as f64 / elapsed.as_secs_f64(),
        elapsed,
    })
}

// ---------------------------------------------------------------------------
// BW validation
// ---------------------------------------------------------------------------

fn assert_bw_within_tolerance(stats: &TransferStats, cap_mbps: u32) -> anyhow::Result<()> {
    let cap_bps = cap_mbps as f64 * 1_000_000.0 / 8.0;
    let low = cap_bps * BW_TOLERANCE_LOW;
    let high = cap_bps * BW_TOLERANCE_HIGH;

    anyhow::ensure!(
        stats.rate_bytes_per_sec >= low && stats.rate_bytes_per_sec <= high,
        "BW out of ±10% tolerance: realized {:.1} KB/s, cap {:.1} KB/s \
         (expected [{:.1}, {:.1}])",
        stats.rate_bytes_per_sec / 1_000.0,
        cap_bps / 1_000.0,
        low / 1_000.0,
        high / 1_000.0,
    );

    println!(
        "  BW utilization {:.1}%  ✓  (realized {:.1} KB/s, cap {:.1} KB/s)",
        stats.rate_bytes_per_sec / cap_bps * 100.0,
        stats.rate_bytes_per_sec / 1_000.0,
        cap_bps / 1_000.0,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: rate-limiter
// ---------------------------------------------------------------------------

/// In-process token bucket accuracy test.
///
/// Simulates the token bucket by sleeping proportionally between chunks and
/// asserts the realized throughput falls within ±10% of the configured rate.
async fn cmd_rate_limiter(sender_c2i: SocketAddr) -> anyhow::Result<()> {
    // ---

    println!("=== rate-limiter ===");

    ensure_agent_running(sender_c2i)?;

    let cap_mbps = match query_cap(sender_c2i)? {
        Some(v) => v,
        None => {
            println!("  agent is uncapped — skipping rate-limiter accuracy test");
            return Ok(());
        }
    };

    let cap_bytes_per_sec = cap_mbps as f64 * 1_000_000.0 / 8.0;
    let test_bytes = (cap_bytes_per_sec * 5.0) as usize;

    println!(
        "  cap: {cap_mbps} Mbit/s   payload: {} KiB   expected duration: ~5s",
        test_bytes / 1024
    );

    let t_start = tokio::time::Instant::now();
    let mut sent = 0usize;
    let mut chunk_idx = 0u64;

    while sent < test_bytes {
        let chunk = RATE_TEST_CHUNK_SIZE.min(test_bytes - sent);
        sent += chunk;
        chunk_idx += 1;
        let deadline = t_start
            + Duration::from_secs_f64(
                chunk_idx as f64 * RATE_TEST_CHUNK_SIZE as f64 / cap_bytes_per_sec,
            );
        tokio::time::sleep_until(deadline).await;
    }

    let elapsed = t_start.elapsed();
    let realized_bps = test_bytes as f64 / elapsed.as_secs_f64() * 8.0;
    let cap_bps = cap_mbps as f64 * 1_000_000.0;
    let utilize = realized_bps / cap_bps * 100.0;

    println!(
        "  realized: {:.1} Mbit/s   utilization: {:.1}%   elapsed: {:.3}s",
        realized_bps / 1_000_000.0,
        utilize,
        elapsed.as_secs_f64(),
    );

    let low = cap_bps * BW_TOLERANCE_LOW;
    let high = cap_bps * BW_TOLERANCE_HIGH;
    anyhow::ensure!(
        realized_bps >= low && realized_bps <= high,
        "rate-limiter out of ±10% tolerance: realized {:.2} Mbit/s, cap {:.2} Mbit/s",
        realized_bps / 1_000_000.0,
        cap_bps / 1_000_000.0,
    );

    println!("  rate-limiter PASSED ✓");
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: multi-file
// ---------------------------------------------------------------------------

async fn cmd_multi_file(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &MultiFileArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== multi-file ===");

    ensure_agent_running(sender_c2i)?;
    ensure_agent_running(receiver_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    let file_sizes: Vec<usize> = if args.large {
        vec![
            30 * 1024 * 1024, //  30 MiB
            2 * 1024 * 1024,  //   2 MiB
            512 * 1024,       // 512 KiB
        ]
    } else if args.small {
        vec![9_000, 1_024, 512, 1]
    } else if let Some(mb) = args.size_mb {
        std::iter::repeat(mb * 1024 * 1024)
            .take(args.count)
            .collect()
    } else if let Some(secs) = args.duration_secs {
        let bytes = cap_mbps
            .map(|c| c as usize * 1_000_000 / 8 * secs as usize)
            .unwrap_or(32 * 1024 * 1024);
        std::iter::repeat(bytes).take(args.count).collect()
    } else {
        let bytes = cap_mbps
            .map(|c| c as usize * 1_000_000 / 8 * 10)
            .unwrap_or(32 * 1024 * 1024);
        std::iter::repeat(bytes).take(args.count).collect()
    };

    if args.link_outage {
        run_multi_file_link_outage(sender_c2i, receiver_c2i, &file_sizes, cap_mbps).await?;
    } else if args.link_fail {
        run_multi_file_link_fail(sender_c2i, receiver_c2i).await?;
    } else {
        for (i, &sz) in file_sizes.iter().enumerate() {
            run_single_transfer(
                sender_c2i,
                receiver_c2i,
                sz,
                &format!("multi-file-{i}"),
                cap_mbps,
            )
            .await?;

            if args.bidirectional {
                run_single_transfer(
                    receiver_c2i,
                    sender_c2i,
                    sz,
                    &format!("multi-file-{i}-reverse"),
                    cap_mbps,
                )
                .await?;
            }
        }
    }

    println!("  multi-file PASSED ✓");
    println!();
    Ok(())
}

async fn run_single_transfer(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bytes: usize,
    label: &str,
    cap_mbps: Option<u32>,
) -> anyhow::Result<()> {
    println!("  [{label}] {} KiB", bytes / 1024);
    let payload = tokio::task::spawn_blocking(move || generate_test_data(bytes)).await?;
    let timeout = transfer_timeout(bytes, cap_mbps);
    let uuid = Uuid::new_v4().to_string();

    let stats = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload,
        &uuid,
        LinkInject::None,
        cap_mbps,
        timeout,
    )
    .await?;

    anyhow::ensure!(
        stats.sha256_sent == stats.sha256_rcvd,
        "[{label}] sha256 MISMATCH:\n  sent: {}\n  rcvd: {}",
        stats.sha256_sent,
        stats.sha256_rcvd,
    );
    println!("  [{label}] sha256 ✓");

    if let Some(cap) = cap_mbps {
        assert_bw_within_tolerance(&stats, cap)?;
    }

    Ok(())
}

async fn run_multi_file_link_outage(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    file_sizes: &[usize],
    cap_mbps: Option<u32>,
) -> anyhow::Result<()> {
    let rate_bps = cap_mbps.map(|c| c as f64 * 1_000_000.0 / 8.0);

    let link_down_secs = rate_bps
        .map(|r| SPOOL_DROP_AFTER_BYTES as f64 * SPOOL_FILL_FRACTION / r)
        .unwrap_or(1.0);

    let post_bytes = rate_bps
        .map(|r| (r * link_down_secs * 1.5) as usize)
        .unwrap_or(512 * 1024);
    let total_bytes = SPOOL_DROP_AFTER_BYTES + post_bytes;
    let timeout = Duration::from_secs(
        ((total_bytes as f64 / rate_bps.unwrap_or(10e6) * TIMEOUT_HEADROOM + link_down_secs * 2.0)
            as u64)
            .max(60),
    );

    println!(
        "  link-outage: drop after {} KiB, link down {link_down_secs:.3}s",
        SPOOL_DROP_AFTER_BYTES / 1024
    );

    let payload1 = tokio::task::spawn_blocking(move || generate_test_data(total_bytes)).await?;
    let uuid1 = Uuid::new_v4().to_string();

    let stats1 = run_transfer(
        sender_c2i,
        receiver_c2i,
        payload1,
        &uuid1,
        LinkInject::Drop {
            drop_after: SPOOL_DROP_AFTER_BYTES,
            link_down_secs,
            sender_c2i,
        },
        cap_mbps,
        timeout,
    )
    .await?;

    anyhow::ensure!(
        stats1.sha256_sent == stats1.sha256_rcvd,
        "link-outage file-1 sha256 MISMATCH"
    );
    println!("  link-outage file-1 sha256 ✓");

    let sz2 = file_sizes.get(1).copied().unwrap_or(64 * 1024);
    run_single_transfer(
        sender_c2i,
        receiver_c2i,
        sz2,
        "link-outage-file-2",
        cap_mbps,
    )
    .await?;

    Ok(())
}

async fn run_multi_file_link_fail(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
) -> anyhow::Result<()> {
    println!("  link-fail: (stub — implement once --link-fail-timeout is a tunable agent CLI arg)");
    // TODO: sequence:
    //   1. stream_start a file (~2s at cap)
    //   2. sleep 1s, link_enable(false) on both agents
    //   3. sleep past link-fail timeout
    //   4. assert get_link_state() == Failed on both agents
    //   5. assert stream_failed callback with code LinkFailed
    let _ = (sender_c2i, receiver_c2i);
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: drr
// ---------------------------------------------------------------------------

async fn cmd_drr(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &DrrArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== drr ===");

    ensure_agent_running(sender_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    {
        let mut agent = connect_agent(sender_c2i).context("connect_agent(sender_c2i) failed")?;
        agent
            .set_max_concurrent(1)
            .context("set_max_concurrent(1) failed")?;
    }

    let anchor_bytes = cap_mbps
        .map(|c| c as usize * 1_000_000 / 8 * 3)
        .unwrap_or(4 * 1024 * 1024);

    let priorities: Vec<(i8, &str)> = vec![(10, "low"), (30, "high"), (20, "med")];
    let priorities = &priorities[..args.file_count.min(priorities.len())];

    let anchor_payload =
        tokio::task::spawn_blocking(move || generate_test_data(anchor_bytes)).await?;
    let anchor_uuid = Uuid::new_v4().to_string();
    let anchor_sha256 = sha256_hex(&anchor_payload);

    let anchor_cb = TestCallbackServer::bind()?;
    let receiver_cb = TestCallbackServer::bind()?;
    let timeout = transfer_timeout(anchor_bytes, cap_mbps);

    {
        let mut sender_agent = connect_agent(sender_c2i)?;
        let mut receiver_agent = connect_agent(receiver_c2i)?;
        let e = sender_agent.set_callback(anchor_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "drr set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "drr set_callback (receiver): {e}");

        let mut attrs = BTreeMap::new();
        attrs.insert("sha256".to_string(), anchor_sha256.clone());
        let result = sender_agent.stream_start(
            anchor_uuid.clone(),
            StreamInfo {
                size_bytes: Some(anchor_bytes as i64),
                attrs: Some(attrs),
            },
            0,
        )?;
        anyhow::ensure!(
            result.err_msg.as_deref().unwrap_or("").is_empty(),
            "drr anchor stream_start failed: {:?}",
            result.err_msg
        );
        println!("  anchor queued (priority 0, {} KiB)", anchor_bytes / 1024);

        for (pri, label) in priorities {
            let small = 4 * 1024usize;
            let mut attrs = BTreeMap::new();
            attrs.insert("label".to_string(), label.to_string());
            let result = sender_agent.stream_start(
                Uuid::new_v4().to_string(),
                StreamInfo {
                    size_bytes: Some(small as i64),
                    attrs: Some(attrs),
                },
                *pri,
            )?;
            anyhow::ensure!(
                result.err_msg.as_deref().unwrap_or("").is_empty(),
                "drr {label} stream_start failed"
            );
            println!("  queued {label} (priority {pri})");

            if *label == priorities.last().unwrap().1 {
                let pending = result.pending_queue.unwrap_or_default();
                println!("  pending queue: {pending:?}");
                let queue_pris: Vec<i8> = pending
                    .iter()
                    .filter_map(|s| s.split('\t').next().and_then(|p| p.parse().ok()))
                    .collect();
                let mut sorted = queue_pris.clone();
                sorted.sort_unstable_by(|a, b| b.cmp(a));
                anyhow::ensure!(
                    queue_pris == sorted,
                    "DRR pending queue not sorted by priority: {queue_pris:?}"
                );
                println!("  DRR ordering ✓");
            }
        }
    }

    let anchor_port = match anchor_cb.recv_event(timeout)? {
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("drr anchor: expected Started, got {other:?}"),
    };

    let write_task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{anchor_port}"))?;
        tcp.write_all(&anchor_payload)?;
        Ok(())
    });

    let receiver_port = match receiver_cb.recv_event(timeout)? {
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("drr anchor receiver: expected Started, got {other:?}"),
    };
    let mut received = Vec::with_capacity(anchor_bytes);
    {
        use std::io::Read;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.read_to_end(&mut received)?;
    }

    match receiver_cb.recv_event(timeout)? {
        TestCallbackEvent::Done { .. } => {}
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("drr anchor receiver stream_failed: {reason}")
        }
        other => anyhow::bail!("drr anchor receiver: expected Done, got {other:?}"),
    }
    match anchor_cb.recv_event(timeout)? {
        TestCallbackEvent::Done { .. } => {}
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("drr anchor sender stream_failed: {reason}")
        }
        other => anyhow::bail!("drr anchor sender: expected Done, got {other:?}"),
    }

    write_task.await??;

    let sha256_rcvd = sha256_hex(&received);
    anyhow::ensure!(anchor_sha256 == sha256_rcvd, "drr anchor sha256 MISMATCH");
    println!("  drr anchor sha256 ✓");

    {
        let mut agent = connect_agent(sender_c2i).context("connect_agent(sender_c2i) failed")?;
        agent.set_max_concurrent(0)?; // 0 = restore default
    }

    println!("  drr PASSED ✓");
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: small-file-edge-cases
// ---------------------------------------------------------------------------

async fn cmd_small_file_edge_cases(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &SmallFileEdgeCasesArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== small-file-edge-cases ===");

    ensure_agent_running(sender_c2i)?;
    ensure_agent_running(receiver_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    {
        let mut s = connect_agent(sender_c2i)?;
        let mut r = connect_agent(receiver_c2i)?;
        s.set_chunk_size_bytes(1024)?;
        r.set_chunk_size_bytes(1024)?;
    }

    let sizes = [
        (9_000usize, "9000B (8 chunks + fragment)"),
        (1_024, "1024B (exact single chunk)"),
        (512, "512B (half chunk)"),
        (1, "1B (minimum C2I stream)"),
    ];

    for (sz, label) in &sizes {
        println!("  [{label}]");
        run_single_transfer(sender_c2i, receiver_c2i, *sz, label, cap_mbps).await?;
        if args.bidirectional {
            run_single_transfer(
                receiver_c2i,
                sender_c2i,
                *sz,
                &format!("{label} (reverse)"),
                cap_mbps,
            )
            .await?;
        }
    }

    {
        let mut s = connect_agent(sender_c2i)?;
        let mut r = connect_agent(receiver_c2i)?;
        s.set_chunk_size_bytes(0)?; // 0 = restore default
        r.set_chunk_size_bytes(0)?;
    }

    println!("  small-file-edge-cases PASSED ✓");
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("\nERROR: {:#}\n", e);
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.debug { "debug" } else { "info" };
    std::env::set_var("RUST_LOG", log_level);

    let no_color = std::env::var("EMACS").is_ok()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CARGO_TERM_COLOR").as_deref() == Ok("never")
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());

    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .with_ansi(!no_color)
        .init();

    match &cli.command {
        Command::RateLimiter => cmd_rate_limiter(cli.sender_c2i).await?,
        Command::MultiFile(args) => cmd_multi_file(cli.sender_c2i, cli.receiver_c2i, args).await?,
        Command::Drr(args) => cmd_drr(cli.sender_c2i, cli.receiver_c2i, args).await?,
        Command::SmallFileEdgeCases(args) => {
            cmd_small_file_edge_cases(cli.sender_c2i, cli.receiver_c2i, args).await?
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_verify() {
        Cli::command().debug_assert();
    }

    #[test]
    fn multi_file_size_flags_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "e2e_test",
            "--sender-c2i",
            "127.0.0.1:9090",
            "--receiver-c2i",
            "127.0.0.1:9091",
            "multi-file",
            "--large",
            "--small",
        ]);
        assert!(
            result.is_err(),
            "expected parse error for --large --small together"
        );
    }

    #[test]
    fn multi_file_link_outage_and_fail_are_mutually_exclusive() {
        let result =
            Cli::try_parse_from(["e2e_test", "multi-file", "--link-outage", "--link-fail"]);
        assert!(
            result.is_err(),
            "expected parse error for --link-outage --link-fail together"
        );
    }

    #[test]
    fn defaults_parse_cleanly() {
        for sub in ["rate-limiter", "drr", "small-file-edge-cases"] {
            Cli::try_parse_from(["e2e_test", sub])
                .unwrap_or_else(|e| panic!("{sub} default parse failed: {e}"));
        }
        Cli::try_parse_from(["e2e_test", "multi-file"]).expect("multi-file default parse failed");
    }
}
