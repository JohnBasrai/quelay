//! Quelay example — demonstrations and C2I smoke client.
//!
//! * When run without `--agent-endpoint` the three built-in demos
//!   execute (LinkSimTransport, Thrift mapping, QUIC loopback).
//!
//! * When `--agent-endpoint` is supplied the binary connects to a
//!   live `quelay-agent`, asserts that the IDL wire version matches the
//!   locally compiled version, and reports the link state — then runs
//!   the demos as before.
//!
//! Run with:
//!   cargo run -p quelay-example
//!   cargo run -p quelay-example -- --agent-endpoint 127.0.0.1:9090

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

mod link_sim_demo;
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
    /// TCP address of a running quelay-agent C2I interface.
    /// When supplied, the example connects and runs a live smoke check
    /// (version assertion + link state query) before the built-in demos.
    #[arg(long)]
    agent_endpoint: Option<SocketAddr>,
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

    println!("=== 1. LinkSim transport demo ===");
    link_sim_demo::run().await;

    println!();
    println!("=== 2. Thrift mapping demo ===");
    thrift_demo::run();

    println!();
    println!("=== 3. QUIC transport demo ===");
    quic_demo::run().await;

    Ok(())
}

// ---------------------------------------------------------------------------
// smoke_check
// ---------------------------------------------------------------------------

/// Connects to a live `quelay-agent` C2I endpoint, asserts the remote IDL
/// version matches the locally compiled `IDL_VERSION`, and logs the current
/// link state.
///
/// The ephemeral local port assigned by the OS is visible in the tracing
/// output from the agent side; no fixed port is required here.
fn smoke_check(addr: SocketAddr) -> anyhow::Result<()> {
    // ---
    let mut channel = TTcpChannel::new();
    channel.open(addr.to_string())?;

    let (rx, tx) = channel.split()?;
    let mut client = QueLayAgentSyncClient::new(
        TBinaryInputProtocol::new(TBufferedReadTransport::new(rx), true),
        TBinaryOutputProtocol::new(TBufferedWriteTransport::new(tx), true),
    );

    // --- version assertion
    let remote_version = client.get_version()?;
    if remote_version != IDL_VERSION {
        anyhow::bail!("IDL version mismatch: local={IDL_VERSION:?} remote={remote_version:?}");
    }
    println!("  IDL version: {remote_version} ✓");

    // --- link state
    let state = client.get_link_state()?;
    println!("  Link state:  {state}");

    Ok(())
}
