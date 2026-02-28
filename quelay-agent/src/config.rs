//! CLI configuration for `quelay-agent`.
//!
//! Run modes:
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] server [--bind 0.0.0.0:5000]
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] client --peer 192.168.1.10:5000 --cert /tmp/quelay-server.der

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// Defaults — kept here so integration tests can import them directly.
// ---------------------------------------------------------------------------

/// Default chunk payload size in bytes.
///
/// Drives spool granularity and ack frequency.  Smaller values give finer
/// acks at higher per-chunk framing overhead; larger values amortize overhead
/// but coarsen reconnect replay granularity.
///
/// Override at runtime with `--chunk-size-bytes` or via the
/// `set_chunk_size_bytes` C2I call (used by `e2e_test small-file-edge-cases`
/// to reproduce the 1 KiB block size used by the legacy FTA system).
pub const DEFAULT_CHUNK_SIZE_BYTES: usize = 16 * 1024; // 16 KiB

/// Default in-memory spool capacity per uplink stream.
///
/// The spool absorbs bursts during link outages.  When full, the TCP reader
/// pauses (back-pressure).  Override with `--spool-capacity-bytes`.
pub const DEFAULT_SPOOL_CAPACITY_BYTES: usize = 1024 * 1024; // 1 MiB

/// Default maximum concurrent active streams (0 = unlimited).
pub const DEFAULT_MAX_CONCURRENT: usize = 0;

/// Default maximum depth of the pending queue.
pub const DEFAULT_MAX_PENDING: usize = 100;

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

    /// Uplink bandwidth cap in Mbit/s.
    ///
    /// Applied by the rate limiter on every QUIC write.
    /// Set to 0 (default) to disable rate limiting entirely.
    ///
    /// Example: `--bw-cap-mbps 10` caps at 10 Mbit/s (1.25 MB/s).
    #[arg(long, default_value_t = 0)]
    pub bw_cap_mbps: u64,

    /// Chunk payload size in bytes written to the QUIC stream.
    ///
    /// Controls spool granularity and ack frequency.  Must be ≤ 65535
    /// (u16 max — the wire field width).  Defaults to 16 KiB.
    ///
    /// The integration test binary sets this to 1024 for `small-file-edge-cases`
    /// to reproduce the legacy FTA block size and exercise multi-block
    /// framing boundaries.
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE_BYTES)]
    pub chunk_size_bytes: usize,

    /// In-memory spool capacity per uplink stream in bytes.
    ///
    /// The spool absorbs bursts during link outages.  When full the TCP
    /// reader pauses (back-pressure to the client).  The link-outage test
    /// in `e2e_test` derives its link-down window from this value.
    /// Defaults to 1 MiB.
    #[arg(long, default_value_t = DEFAULT_SPOOL_CAPACITY_BYTES)]
    pub spool_capacity_bytes: usize,

    /// Maximum concurrent active streams (0 = unlimited).
    ///
    /// The DRR test sets this to 1 via the `set_max_concurrent` C2I call
    /// so that queued streams are reordered by priority before activation.
    /// This flag sets the startup default; the value can be changed live
    /// via `set_max_concurrent`.
    #[arg(short = 'N', long, default_value_t = DEFAULT_MAX_CONCURRENT)]
    pub max_concurrent: usize,

    /// Maximum number of streams allowed in the pending queue (default: 100).
    ///
    /// When the pending queue is full, `stream_start` returns
    /// `queue_position == -1` and an error message.  Capped at 100 to bound
    /// the size of the `pending_queue` snapshot returned by `stream_start`.
    #[arg(long, default_value_t = DEFAULT_MAX_PENDING)]
    pub max_pending: usize,
}

// ---

impl Config {
    // ---

    /// Convert `bw_cap_mbps` to **bits-per-second**, or `None` if uncapped.
    ///
    /// The returned value is in bits/s and is passed directly to the rate
    /// limiter, which divides by 8 internally to derive its byte budget.
    pub fn bw_cap_bps(&self) -> Option<u64> {
        if self.bw_cap_mbps == 0 {
            None
        } else {
            Some(self.bw_cap_mbps * 1_000_000)
        }
    }

    /// Validate config fields that clap cannot express as type constraints.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.chunk_size_bytes == 0 || self.chunk_size_bytes > 65_535 {
            anyhow::bail!(
                "--chunk-size-bytes must be 1..=65535, got {}",
                self.chunk_size_bytes
            );
        }
        if self.spool_capacity_bytes == 0 {
            anyhow::bail!("--spool-capacity-bytes must be > 0");
        }
        Ok(())
    }

    /// Maximum concurrent active streams, or `None` if unlimited.
    pub fn max_concurrent(&self) -> Option<usize> {
        // ---
        if self.max_concurrent == 0 {
            None
        } else {
            Some(self.max_concurrent)
        }
    }

    /// Maximum depth of the pending queue.
    pub fn max_pending(&self) -> usize {
        // ---
        self.max_pending
    }

    /// Chunk payload size in bytes.
    #[allow(dead_code)]
    pub fn chunk_size_bytes(&self) -> usize {
        // ---
        self.chunk_size_bytes
    }

    /// In-memory spool capacity per uplink stream in bytes.
    #[allow(dead_code)]
    pub fn spool_capacity_bytes(&self) -> usize {
        // ---
        self.spool_capacity_bytes
    }
}

// ---

#[derive(Debug, Subcommand)]
pub enum Mode {
    // ---
    /// Listen for an incoming QUIC connection (satellite ground station or
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
        /// generated its cert.
        #[arg(long, default_value = "quelay")]
        server_name: String,

        /// Path to the server's self-signed cert DER file.
        /// The server writes this at startup; copy it to the client
        /// before launching.
        #[arg(long)]
        cert: PathBuf,
    },
}
