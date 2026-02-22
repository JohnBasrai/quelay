//! Quelay example — demonstrations and C2I smoke client.
//!
//! * When run without `--agent-endpoint` the three built-in demos
//!   execute (Thrift mapping, QUIC loopback).
//!
//! * When `--agent-endpoint` is supplied the example connects to a live
//!   `quelay-agent`, asserts that the IDL wire version matches the locally
//!   compiled version, and reports the link state.
//!
//! * When both `--agent-endpoint` and `--receiver-endpoint` are supplied
//!   the full end-to-end transfer demo runs against two live agents.
//!
//! * Add `--spool-test` to also run the spool reconnect test (requires
//!   `--agent-endpoint` and `--receiver-endpoint`).
//!
//! Run with:
//!   cargo run -p quelay-example
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090 \
//!                                  --receiver-endpoint 127.0.0.1:9091
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090 \
//!                                  --receiver-endpoint 127.0.0.1:9091 \
//!                                  --spool-test \
//!                                  --bw-cap-mbps 50

use std::net::SocketAddr;

use clap::Parser;

use quelay_thrift::{
    // ---
    QueLayAgentSyncClient,
    TBinaryInputProtocol,
    TBinaryOutputProtocol,
    TBufferedReadTransport,
    TBufferedWriteTransport,
    TIoChannel,
    TQueLayAgentSyncClient,
    TTcpChannel,
    IDL_VERSION,
};

mod e2e_demo;
mod quic_demo;
mod thrift_demo;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    name = "quelay-example",
    about = "Quelay example client and demo runner"
)]
struct Config {
    // ---
    /// TCP address of the sending agent's C2I interface (agent-1).
    #[arg(long)]
    agent_endpoint: Option<SocketAddr>,

    /// TCP address of the receiving agent's C2I interface (agent-2).
    /// Required for e2e transfer and spool tests.
    #[arg(long)]
    receiver_endpoint: Option<SocketAddr>,

    /// Also run the spool reconnect test after the straight e2e transfer.
    ///
    /// Drops the link mid-transfer, verifies the spool replays correctly,
    /// and asserts the received sha256 matches the sent sha256.
    /// Requires both `--agent-endpoint` and `--receiver-endpoint`.
    /// Best paired with `--bw-cap-mbps` for deterministic timing.
    #[arg(long, default_value_t = false)]
    spool_test: bool,

    /// Expected uplink bandwidth cap in Mbit/s (must match `--bw-cap-mbps`
    /// passed to the agents).  Used for BW utilization reporting only.
    /// 0 = uncapped (no utilization report printed).
    #[arg(long, default_value_t = 0)]
    bw_cap_mbps: u64,
}

impl Config {
    fn bw_cap(&self) -> Option<u64> {
        if self.bw_cap_mbps == 0 {
            None
        } else {
            Some(self.bw_cap_mbps)
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ---
    let cfg = Config::parse();

    let no_color = std::env::var("EMACS").is_ok()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CARGO_TERM_COLOR").as_deref() == Ok("never")
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());

    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .with_ansi(!no_color)
        .init();

    if let Some(addr) = cfg.agent_endpoint {
        println!("=== Live agent smoke check: {addr} ===");
        smoke_check(addr)?;
        println!();
    }

    println!("=== 1. LinkSim transport demo === (REMOVED/SKIPPED)");

    println!();
    println!("=== 2. Thrift mapping demo ===");
    thrift_demo::run();

    println!();
    println!("=== 3. QUIC transport demo ===");
    quic_demo::run().await;

    if let (Some(sender), Some(receiver)) = (cfg.agent_endpoint, cfg.receiver_endpoint) {
        println!();
        e2e_demo::run(sender, receiver, cfg.bw_cap()).await?;

        if cfg.spool_test {
            e2e_demo::run_spool_test(sender, receiver, cfg.bw_cap()).await?;
        }
    } else if cfg.spool_test {
        anyhow::bail!("--spool-test requires both --agent-endpoint and --receiver-endpoint");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// smoke_check
// ---------------------------------------------------------------------------

fn smoke_check(addr: SocketAddr) -> anyhow::Result<()> {
    // ---
    let mut channel = TTcpChannel::new();
    channel.open(addr.to_string())?;

    let (rx, tx) = channel.split()?;
    let mut client = QueLayAgentSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    );

    let remote_version = client.get_version()?;
    if remote_version != IDL_VERSION {
        anyhow::bail!("IDL version mismatch: local={IDL_VERSION:?} remote={remote_version:?}");
    }
    println!("  IDL version: {remote_version} ✓");

    let state = client.get_link_state()?;
    println!("  Link state:  {state}");

    Ok(())
}
