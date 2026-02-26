//! `bw-cap-test` — bandwidth cap integration test.
//!
//! Starts N concurrent stream transfers through two live quelay-agents.
//! Each sender tuner runs for `--duration-secs` seconds then shuts down
//! cleanly.  After all tuners exit the aggregate throughput is asserted
//! against the configured BW cap within ±10%.
//!
//! # Usage
//!
//! ```text
//! bw-cap-test \
//!   --sender-c2i    127.0.0.1:9090 \
//!   --receiver-c2i  127.0.0.1:9091 \
//!   --count         3              \
//!   --duration-secs 5
//! ```
//!
//! # Agent setup (ci-integration-test.sh)
//!
//! ```bash
//! start_agents 10 4   # --bw-cap-mbps 10  --max-concurrent 4
//!
//! "$BW_CAP_TEST_BIN"          \
//!   --sender-c2i   "$AGENT1_C2I" \
//!   --receiver-c2i "$AGENT2_C2I" \
//!   --count        3             \
//!   --duration-secs 5
//!
//! stop_agents
//! ```

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

// ---------------------------------------------------------------------------
// Module declarations  (EMBP: private modules, gateway controls exports)
// ---------------------------------------------------------------------------

mod callback;
mod cic;
mod tuner;

// ---------------------------------------------------------------------------
// Gateway re-exports
// ---------------------------------------------------------------------------

pub use callback::{CallbackActor, Role};
pub use cic::{assert_aggregate_bw, Cic, CicConfig, CicHandle, CicMsg, TunerPair};
pub use tuner::{spawn_receiver, spawn_sender, TunerCmd, TunerOutcome, TunerResult};

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

// ---

use anyhow::Context as _;
use clap::Parser;
use rand::{RngCore, SeedableRng};
use tokio::sync::mpsc;
use uuid::Uuid;

// ---

#[rustfmt::skip]
use quelay_thrift::{
    // ---
    QueLayAgentSyncClient,
    QueLayCallbackSyncProcessor,
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

/// Each stream payload is sized as if it were the sole consumer of the link,
/// multiplied by this factor so the writer is still mid-stream when the
/// duration timer fires.
const PAYLOAD_HEADROOM: f64 = 4.0;

/// Fallback payload size when no BW cap is configured on the agent.
const FALLBACK_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

/// How long after `--duration-secs` before the failsafe Kill fires.
const KILL_HEADROOM_SECS: u64 = 30;

/// Aggregate BW must be within this fraction of the configured cap.
const BW_TOLERANCE: f64 = 0.10;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Quelay bandwidth-cap integration test.
///
/// Verifies that the agent's rate limiter correctly shapes N concurrent
/// streams to the configured aggregate cap.
#[derive(Debug, Parser)]
#[command(name = "bw-cap-test")]
struct Cli {
    // ---
    /// C2I address of the sending agent (air side).
    #[arg(long, default_value = "127.0.0.1:9090")]
    sender_c2i: SocketAddr,

    /// C2I address of the receiving agent (ground side).
    #[arg(long, default_value = "127.0.0.1:9091")]
    receiver_c2i: SocketAddr,

    /// Number of concurrent streams to run.
    #[arg(long, default_value_t = 3)]
    count: usize,

    /// How long (seconds) each stream transmits before shutdown.
    #[arg(long, default_value_t = 5)]
    duration_secs: u64,

    /// Enable debug logging.
    #[arg(long, default_value_t = false)]
    debug: bool,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // ---
    if let Err(e) = real_main().await {
        eprintln!("\nERROR: {e:#}\n");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    // ---
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

    println!("=== bw-cap-test ===");
    println!(
        "  count={} duration={}s  sender={}  receiver={}",
        cli.count, cli.duration_secs, cli.sender_c2i, cli.receiver_c2i,
    );

    ensure_agent_running(cli.sender_c2i)?;
    ensure_agent_running(cli.receiver_c2i)?;

    // --- query BW cap from sender agent ---
    let cap_mbps = query_cap(cli.sender_c2i).context("query_cap failed")?;
    println!(
        "  BW cap: {}",
        cap_mbps
            .map(|c| format!("{c} Mbit/s"))
            .unwrap_or_else(|| "uncapped".into()),
    );

    // --- size each payload as if it is the sole stream on the link ---
    let payload_bytes = cap_mbps
        .map(|c| {
            let bps = c as f64 * 1_000_000.0 / 8.0;
            (bps * cli.duration_secs as f64 * PAYLOAD_HEADROOM) as usize
        })
        .unwrap_or(FALLBACK_PAYLOAD_BYTES);

    println!(
        "  payload per stream: {} MiB  ({payload_bytes} B)",
        payload_bytes / (1024 * 1024),
    );

    // --- generate all payloads sequentially before any transfer starts ---
    println!("  generating {} payloads...", cli.count);
    let mut payloads = Vec::with_capacity(cli.count);
    for i in 0..cli.count {
        let seed = 0xDEAD_BEEF_0000_0000u64 | i as u64;
        let p = tokio::task::spawn_blocking(move || generate_payload(payload_bytes, seed)).await?;
        payloads.push(p);
    }

    // --- build CIC ---
    let (mut cic, cic_tx) = Cic::new(CicConfig {
        sender_c2i: cli.sender_c2i,
        receiver_c2i: cli.receiver_c2i,
        duration: Duration::from_secs(cli.duration_secs),
        kill_headroom: Duration::from_secs(KILL_HEADROOM_SECS),
    });

    // --- bind callback servers ---
    let sender_cb_addr = bind_callback_server(Role::Sender, cic_tx.clone())?;
    let receiver_cb_addr = bind_callback_server(Role::Receiver, cic_tx.clone())?;

    {
        let mut s = connect_agent(cli.sender_c2i)?;
        let mut r = connect_agent(cli.receiver_c2i)?;
        let e = s.set_callback(sender_cb_addr.to_string())?;
        anyhow::ensure!(e.is_empty(), "set_callback (sender): {e}");
        let e = r.set_callback(receiver_cb_addr.to_string())?;
        anyhow::ensure!(e.is_empty(), "set_callback (receiver): {e}");
    }

    // --- single agent connections for all stream_start calls ---
    let mut sender_agent = connect_agent(cli.sender_c2i)?;
    let mut receiver_agent = connect_agent(cli.receiver_c2i)?;

    println!("  starting {} stream pairs...", cli.count);

    let t_wall = Instant::now();

    for (i, payload) in payloads.into_iter().enumerate() {
        let uuid = Uuid::new_v4().to_string();

        // --- sender tuner ---
        let (s_tx, s_rx) = mpsc::channel::<TunerCmd>(32);
        let s_handle = spawn_sender(uuid.clone(), payload, s_rx, cic.handle());

        // --- receiver tuner ---
        let (r_tx, r_rx) = mpsc::channel::<TunerCmd>(32);
        let r_handle = spawn_receiver(uuid.clone(), r_rx, cic.handle());

        // Register before stream_start to avoid lost callbacks.
        cic.register(uuid.clone(), Role::Sender, s_tx, s_handle);
        cic.register(uuid.clone(), Role::Receiver, r_tx, r_handle);
        cic.register_pair(TunerPair {
            sender_uuid: uuid.clone(),
            receiver_uuid: uuid.clone(),
        });

        // --- stream_start on sender agent ---
        let mut attrs = BTreeMap::new();
        attrs.insert("filename".to_string(), format!("bw-cap-stream-{i}.bin"));

        let res = sender_agent.stream_start(
            uuid.clone(),
            StreamInfo {
                size_bytes: Some(payload_bytes as i64),
                attrs: Some(attrs.clone()),
            },
            0,
        )?;
        anyhow::ensure!(
            res.err_msg.as_deref().unwrap_or("").is_empty(),
            "sender stream_start[{i}] failed: {:?}",
            res.err_msg,
        );

        // --- stream_start on receiver agent ---
        let res = receiver_agent.stream_start(
            uuid.clone(),
            StreamInfo {
                size_bytes: Some(payload_bytes as i64),
                attrs: Some(attrs),
            },
            0,
        )?;
        anyhow::ensure!(
            res.err_msg.as_deref().unwrap_or("").is_empty(),
            "receiver stream_start[{i}] failed: {:?}",
            res.err_msg,
        );

        println!("  stream {i} started  uuid={uuid}");
    }

    // --- run CIC dispatch loop ---
    println!("  CIC running ({} s)...", cli.duration_secs);
    let results = cic.run().await?;
    let wall_elapsed = t_wall.elapsed();

    // --- per-tuner summary ---
    println!("\n  ┌── per-stream results ──────────────────────────────────┐");
    for r in &results {
        let status = match &r.outcome {
            TunerOutcome::Pass => "✓".to_string(),
            TunerOutcome::Fail { reason } => format!("✗ {reason}"),
        };
        println!(
            "  │  {:8?}  {:>12} B  {:>6.2}s  {status}",
            r.role,
            r.bytes,
            r.elapsed.as_secs_f64(),
        );
    }
    println!("  └────────────────────────────────────────────────────────┘\n");

    // --- fail fast if any tuner reported an error ---
    let failures: Vec<_> = results
        .iter()
        .filter(|r| matches!(r.outcome, TunerOutcome::Fail { .. }))
        .collect();

    if !failures.is_empty() {
        for f in &failures {
            if let TunerOutcome::Fail { reason } = &f.outcome {
                eprintln!("  FAIL  {:?}  {}  — {reason}", f.role, f.uuid);
            }
        }
        anyhow::bail!("{} tuner(s) failed", failures.len());
    }

    // --- aggregate BW assertion ---
    match cap_mbps {
        Some(cap) => assert_aggregate_bw(&results, cap, wall_elapsed, BW_TOLERANCE)?,
        None => println!("  BW assertion skipped (no cap configured)"),
    }

    println!("\n  bw-cap-test PASSED ✓\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// Callback server
// ---------------------------------------------------------------------------

fn bind_callback_server(role: Role, cic_tx: mpsc::Sender<CicMsg>) -> anyhow::Result<SocketAddr> {
    // ---
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let addr_str = addr.to_string();

    let handler = CallbackActor::new(role, cic_tx);
    let processor = QueLayCallbackSyncProcessor::new(handler);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

    std::thread::Builder::new()
        .name(format!("cb-{role:?}"))
        .spawn(move || {
            drop(listener);
            let mut server = TServer::new(
                TBufferedReadTransportFactory::new(),
                TBinaryInputProtocolFactory::new(),
                TBufferedWriteTransportFactory::new(),
                TBinaryOutputProtocolFactory::new(),
                processor,
                8,
            );
            let _ = ready_tx.send(());
            if let Err(e) = server.listen(&addr_str) {
                tracing::warn!("callback server {role:?} exiting: {e}");
            }
        })?;

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| anyhow::anyhow!("callback server {role:?} did not start within 2s"))?;

    std::thread::sleep(Duration::from_millis(10));
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Agent C2I helpers
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

fn ensure_agent_running(addr: SocketAddr) -> anyhow::Result<()> {
    // ---
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        Ok(_) => Ok(()),
        Err(_) => anyhow::bail!("agent not reachable at {addr} (is it running?)"),
    }
}

fn query_cap(addr: SocketAddr) -> anyhow::Result<Option<u32>> {
    // ---
    let mut agent = connect_agent(addr)?;
    let v = agent.get_bandwidth_cap_mbps()?;
    Ok(if v <= 0 { None } else { Some(v as u32) })
}

// ---------------------------------------------------------------------------
// Test data generation
// ---------------------------------------------------------------------------

fn generate_payload(n: usize, seed: u64) -> Vec<u8> {
    // ---
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
    let mut buf = vec![0u8; n];
    rng.fill_bytes(&mut buf);
    buf
}

// ---------------------------------------------------------------------------
// CLI tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    // ---

    #[test]
    fn cli_verify() {
        // ---
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_parse_cleanly() {
        // ---
        Cli::try_parse_from(["bw-cap-test"]).expect("default parse failed");
    }

    #[test]
    fn custom_args_parse() {
        // ---
        Cli::try_parse_from([
            "bw-cap-test",
            "--sender-c2i",
            "127.0.0.1:9090",
            "--receiver-c2i",
            "127.0.0.1:9091",
            "--count",
            "3",
            "--duration-secs",
            "5",
        ])
        .expect("custom args parse failed");
    }
}
