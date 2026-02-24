//! Quelay example — demonstrations and C2I smoke client.
//!
//! Shows how to write a minimal quelay client. Three built-in demos run
//! automatically; with `--agent-endpoint` the example also performs a live
//! smoke check against a running agent.
//!
//! For integration testing (link outage, DRR priority, BW validation, etc.)
//! see the `e2e_test` binary in `quelay-agent/src/bin/`.
//!
//! Run with:
//!   cargo run -p quelay-example
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090 \
//!                                  --receiver-endpoint 127.0.0.1:9091

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
    /// TCP address of the agent's C2I interface.
    /// When supplied, a live smoke check runs against the agent.
    #[arg(long)]
    agent_endpoint: Option<SocketAddr>,

    /// TCP address of a second agent's C2I interface.
    /// When both --agent-endpoint and --receiver-endpoint are supplied,
    /// a single end-to-end transfer demo runs between the two agents.
    #[arg(long)]
    receiver_endpoint: Option<SocketAddr>,
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

    println!("=== 1. Thrift mapping demo ===");
    thrift_demo::run();

    println!();
    println!("=== 2. QUIC transport demo ===");
    quic_demo::run().await;

    if let (Some(sender), Some(receiver)) = (cfg.agent_endpoint, cfg.receiver_endpoint) {
        println!();
        println!("=== 3. End-to-end transfer demo ===");
        // Generate a small fixed payload — this is a demo, not a benchmark.
        let payload = {
            use rand::{RngCore, SeedableRng};
            let mut rng = rand::rngs::SmallRng::seed_from_u64(0xDEAD_BEEF);
            let mut buf = vec![0u8; 64 * 1024]; // 64 KiB
            rng.fill_bytes(&mut buf);
            buf
        };
        e2e_demo::run(sender, receiver, payload).await?;
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
