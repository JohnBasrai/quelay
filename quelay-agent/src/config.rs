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

    /// Uplink bandwidth cap in bits/sec.
    ///
    /// `None` means uncapped (no rate limiting).
    ///
    /// Set via `--bw-cap-bps` on the command line using a value and unit,
    /// e.g. `--bw-cap-bps 10Mbps`, `--bw-cap-bps 500Kbps`, `--bw-cap-bps
    /// 1.5Gbps`.
    #[arg(long, value_parser = parse_bandwidth)]
    pub bw_cap_bps: Option<u64>,

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

/// Parse a bandwidth string of the form `<value><unit>` into bits/sec.
///
/// - Accepted units (case-insensitive): `Kbps`, `Mbps`, `Gbps`.
/// - The value may be fractional, e.g. `1.5Mbps` or `500Kbps`.
/// - Whitespace between value and unit is permitted: `10 Mbps`.
///
/// Returns an error string if the value or unit is invalid.
fn parse_bandwidth(s: &str) -> Result<u64, String> {
    // ---

    let s = s.trim();
    let (num, unit) = if let Some(pos) = s.find(|c: char| c.is_alphabetic()) {
        (&s[..pos].trim(), &s[pos..].trim())
    } else {
        return Err(format!("missing unit in '{s}' — use Kbps, Mbps, Gbps"));
    };

    let value: f64 = num.parse().map_err(|_| format!("invalid number '{num}'"))?;

    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "kbps" => 1_000_u64,
        "mbps" => 1_000_000_u64,
        "gbps" => 1_000_000_000_u64,
        _ => return Err(format!("unknown unit '{unit}' — use Kbps, Mbps, Gbps")),
    };
    if value == 0.0 {
        Err(format!("bandwidth must be greater than zero, got '{s}'"))
    } else {
        Ok((value * multiplier as f64) as u64)
    }
}

// ---

impl Config {
    // ---

    /// Validate config fields that clap cannot express as type constraints.
    pub fn validate(&self) -> anyhow::Result<()> {
        // ---
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

    /// Format the bandwidth cap as a human-readable string for logging.
    pub fn bw_cap_display(&self) -> String {
        // ---
        match self.bw_cap_bps {
            None => "uncapped".to_string(),
            Some(bps) if bps >= 1_000_000_000 => {
                format!("{:.1} Gbps", bps as f64 / 1_000_000_000.0)
            }
            Some(bps) if bps >= 1_000_000 => format!("{:.1} Mbps", bps as f64 / 1_000_000.0),
            Some(bps) => format!("{:.1} Kbps", bps as f64 / 1_000.0),
        }
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
