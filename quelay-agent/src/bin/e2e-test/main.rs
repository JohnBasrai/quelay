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
//!   multi-file            Multi-file transfer — large, small, link-outage, link-fail
//!   drr                   DRR scheduler priority ordering
//!   small-file-edge-cases Framing boundary file sizes
//!   max-concurrent        Tests max-concurrent in agent.
//! ```
//!
//! See `quelay-agent/src/bin/README.md` for the full design rationale and
//! mapping to legacy tests.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// ---

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use rand::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---

#[allow(unused)]
use quelay_thrift::{
    // ---
    FailReason,
    LinkState,
    QueLayAgentSyncClient,
    QueLayCallbackSyncHandler,
    QueLayCallbackSyncProcessor,
    QueueStatus,
    StreamInfo,
    StreamStartStatus as Status,
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
// Sub command modules
// ---------------------------------------------------------------------------
mod callback;
mod drr;
mod max_concurrent;
mod multi_file;
mod small_file_edge_cases;

// ---------------------------------------------------------------------------
// Hoist up public symbols.
// ---------------------------------------------------------------------------
use callback::*;
use drr::*;
use max_concurrent::*;
use multi_file::*;
use small_file_edge_cases::*;

fn ensure_agent_running(addr: SocketAddr) -> Result<()> {
    // ---

    let timeout = Duration::from_millis(300);
    match std::net::TcpStream::connect_timeout(&addr, timeout) {
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

/// Minimum transfer size for a meaningful BW check.  Transfers smaller than
/// this complete too quickly for the rate limiter to shape them, so we skip
/// the ±10% assertion rather than raise a spurious error.
const MIN_BW_TEST_BYTES: usize = 1024 * 1024;

/// Bytes written before disabling the link in the spool reconnect path.
/// Matches the agent's default spool capacity (1 MiB).
const SPOOL_DROP_AFTER_BYTES: usize = 1024 * 1024;

/// Fraction of spool capacity to fill during the link-down window.
const SPOOL_FILL_FRACTION: f64 = 0.50;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Quelay integration test suite.
///
/// Requires two running quelay-agents accessible at --sender-c2i and
/// --receiver-c2i
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
    /// Multi-file transfer: large files, small files, link outage, link failure.
    MultiFile(MultiFileArgs),

    /// DRR scheduler priority ordering (sets agent to 1 concurrent stream).
    Drr(DrrArgs),

    /// Framing boundary file size regression tests.
    SmallFileEdgeCases(SmallFileEdgeCasesArgs),

    /// Max-concurrent stream enforcement and pending queue ordering.
    MaxConcurrent(MaxConcurrentArgs),
}

// ---

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
    // ---
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_1234);
    let mut buf = vec![0u8; n];
    rng.fill_bytes(&mut buf);
    buf
}

fn sha256_hex(data: &[u8]) -> String {
    // ---
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
    // ---

    let elapsed_s = elapsed.as_secs_f64();
    let kbps = (bytes as f64 / 1_000.0) / elapsed_s;
    let kbits_s = kbps * 8.0;
    let (snd, rcv) = progress_msgs;
    let total_prog = snd + rcv;
    let prog_rate = total_prog as f64 / elapsed_s;

    println!("\n    ============================================================");
    println!("    ---\t{label}");
    println!("    ---\t   Payload Size  : {:.3} MB", bytes as f32 * 1e-6);
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
    // ---
    sha256_sent: String,
    sha256_rcvd: String,
    rate_bytes_per_sec: f64,
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
        result.status == Some(Status::RUNNING),
        "stream_start failed: expected {:?}, got {:?}",
        Status::RUNNING,
        result.status
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
    loop {
        match sender_cb.recv_event(timeout)? {
            TestCallbackEvent::Done { bytes, .. } => {
                tracing::info!(bytes, "sender stream_done");
                break;
            }
            TestCallbackEvent::Failed { reason, .. } => {
                anyhow::bail!("sender stream_failed: {reason}")
            }
            TestCallbackEvent::QueueStatus(_) => {
                // Ignore queue updates while waiting for completion.
                continue;
            }
            TestCallbackEvent::Started { .. } => {
                // Should not happen here, but harmless.
                continue;
            }
            TestCallbackEvent::LinkState(_) => {
                continue;
            }
        }
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
    // ---

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
        "  BW utilization {:.1}%  ✓  (realized {:.1} KB/s, cap {:.1} KB/s elapsed:{:.3?})",
        stats.rate_bytes_per_sec / cap_bps * 100.0,
        stats.rate_bytes_per_sec / 1_000.0,
        cap_bps / 1_000.0,
        stats.elapsed
    );
    Ok(())
}

async fn run_single_transfer(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    bytes: usize,
    label: &str,
    cap_mbps: Option<u32>,
) -> anyhow::Result<()> {
    println!();
    println!("  ┌── [{label}]  {} B  ({} KiB) ───┐", bytes, bytes / 1024);
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
        if bytes >= MIN_BW_TEST_BYTES {
            assert_bw_within_tolerance(&stats, cap)?;
        } else {
            println!(
                "  [{label}] BW check skipped (transfer too small for rate shaping: {} B < {} KiB)",
                bytes,
                MIN_BW_TEST_BYTES / 1024,
            );
        }
    }

    Ok(())
}

async fn run_multi_file_link_outage(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    file_sizes: &[usize],
    cap_mbps: Option<u32>,
) -> anyhow::Result<()> {
    // ---

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
    // ---

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
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("\nERROR: {e:#}\n");
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
        Command::MultiFile(args) => cmd_multi_file(cli.sender_c2i, cli.receiver_c2i, args).await?,
        Command::Drr(args) => cmd_drr(cli.sender_c2i, cli.receiver_c2i, args).await?,
        Command::SmallFileEdgeCases(args) => {
            cmd_small_file_edge_cases(cli.sender_c2i, cli.receiver_c2i, args).await?
        }
        Command::MaxConcurrent(args) => {
            cmd_max_concurrent(cli.sender_c2i, cli.receiver_c2i, args).await?
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
        for sub in ["drr", "small-file-edge-cases", "max-concurrent"] {
            Cli::try_parse_from(["e2e_test", sub])
                .unwrap_or_else(|e| panic!("{sub} default parse failed: {e}"));
        }
        Cli::try_parse_from(["e2e_test", "multi-file"]).expect("multi-file default parse failed");
    }
}
