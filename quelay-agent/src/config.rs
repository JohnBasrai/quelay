//! CLI configuration for `quelay-agent`.
//!
//! Run modes:
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] server [--bind 0.0.0.0:5000]
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] client --peer 192.168.1.2:5000 --cert /tmp/quelay-server.der

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(name = "quelay-agent", about = "Quelay relay daemon")]
pub struct Config {
    // ---
    #[command(subcommand)]
    pub mode: Mode,

    /// TCP address on which to expose the local Thrift C2I interface.
    /// Quelay example clients and other local C2I consumers connect here.
    #[arg(long, default_value = "127.0.0.1:9090")]
    pub agent_endpoint: SocketAddr,

    /// Directory used to spool stream data when the link is down.
    ///
    /// Created automatically if it does not exist.
    /// Future: each remote peer gets a subdirectory `<spool-dir>/<remote-id>/`.
    #[arg(long, default_value = "/tmp/quelay-spool")]
    pub spool_dir: PathBuf,

    /// Uplink bandwidth cap in Mbit/s.
    ///
    /// Applied by [`BandwidthGate`] on every QUIC write.  Set to 0 (default)
    /// to disable rate limiting entirely (loopback CI, uncapped production).
    ///
    /// Example: `--bw-cap-mbps 10` → cap at 10 Mbit/s (1.25 MB/s).
    #[arg(long, default_value_t = 0)]
    pub bw_cap_mbps: u64,
}

// ---

impl Config {
    // ---

    /// Convert `bw_cap_mbps` to bytes-per-second, or `None` if uncapped.
    pub fn bw_cap_bps(&self) -> Option<u64> {
        if self.bw_cap_mbps == 0 {
            None
        } else {
            Some(self.bw_cap_mbps * 1_000_000 / 8)
        }
    }
}

// ---

#[derive(Debug, Subcommand)]
pub enum Mode {
    // ---
    /// Listen for an incoming QUIC connection (satellite / ground station in
    /// server role for this session).
    Server {
        /// UDP address to bind the QUIC endpoint on.
        #[arg(long, default_value = "0.0.0.0:5000")]
        bind: SocketAddr,
    },

    /// Connect to a remote Quelay agent (example: 192.168.1.10:5000).
    Client {
        // ---
        /// UDP address of the remote agent's QUIC endpoint.
        #[arg(long)]
        peer: SocketAddr,

        /// TLS server name — must match the name used when the server
        /// generated its cert (default: "quelay").
        #[arg(long, default_value = "quelay")]
        server_name: String,

        /// Path to the server's self-signed cert DER file.
        /// The server writes this at startup; copy it to the client
        /// before launching.
        #[arg(long)]
        cert: PathBuf,
    },
}
