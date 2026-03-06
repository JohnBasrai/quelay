//! CLI configuration for `quelay-agent`.
//!
//! Run modes:
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] server [--bind 0.0.0.0:5000]
//!   quelay-agent [--agent-endpoint 127.0.0.1:9090] client --peer 192.168.1.10:5000 --cert /tmp/quelay-server.der

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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
// CongestionAlgo
// ---------------------------------------------------------------------------

/// QUIC congestion control algorithm selection.
///
/// Passed to `quinn::TransportConfig::congestion_controller_factory`.
/// Both sides of a connection can use different algorithms independently —
/// congestion control is a per-sender property in QUIC.
#[derive(Debug, Clone, Default, ValueEnum)]
pub enum CongestionAlgo {
    /// RFC 6582 loss-based controller (quinn default).
    #[default]
    NewReno,

    /// Bottleneck Bandwidth and RTT — measures bandwidth directly; does not
    /// treat loss as a congestion signal.  Preferred for high-latency,
    /// lossy SATCOM links where loss-based controllers enter a death spiral.
    Bbr,

    /// RFC 8312 CUBIC — improved loss recovery over NewReno; widely deployed
    /// in terrestrial TCP stacks.
    Cubic,
}

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

    /// QUIC congestion control algorithm.
    ///
    /// `new-reno` (default) is the RFC 6582 loss-based controller built into
    /// quinn.  `bbr` measures bandwidth and RTT directly without using loss as
    /// a congestion signal — strongly preferred for SATCOM links with high RTT
    /// and moderate loss where NewReno enters a window-halving death spiral.
    /// `cubic` is RFC 8312, a middle ground widely used for terrestrial TCP.
    ///
    /// Both peers may run different algorithms; congestion control is a
    /// per-sender property in QUIC.
    #[arg(long, default_value = "new-reno", value_enum)]
    pub congestion: CongestionAlgo,
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
        /// UDP address or hostname of the remote agent's QUIC endpoint.
        ///
        /// Accepts both numeric IPs (`192.168.1.10:5000`) and hostnames
        /// (`agent-server:4433`).  DNS resolution is deferred to connect
        /// time so the agent handles container startup ordering gracefully
        /// without shell-level `getent` workarounds.
        #[arg(long)]
        peer: String,

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::expect_used)]
#[allow(clippy::needless_borrow)]
mod tests {
    // ---
    use super::*;

    // ------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------

    fn parse_ok(args: &[&str]) -> Config {
        Config::try_parse_from(args).expect("expected parse success")
    }

    fn parse_err(args: &[&str]) {
        assert!(Config::try_parse_from(args).is_err());
    }

    // ------------------------------------------------------------
    // Bandwidth parsing (positive)
    // ------------------------------------------------------------

    #[test]
    fn bw_cap_parses_basic_units() {
        // ---
        let cfg = parse_ok(&["quelay-agent", "--bw-cap-bps", "10Mbps", "server"]);
        assert_eq!(cfg.bw_cap_bps, Some(10_000_000));

        let cfg = parse_ok(&["quelay-agent", "--bw-cap-bps", "500Kbps", "server"]);
        assert_eq!(cfg.bw_cap_bps, Some(500_000));

        let cfg = parse_ok(&["quelay-agent", "--bw-cap-bps", "1.5Gbps", "server"]);
        assert_eq!(cfg.bw_cap_bps, Some(1_500_000_000));
    }

    #[test]
    fn bw_cap_parses_case_and_whitespace() {
        // ---
        let cfg = parse_ok(&["quelay-agent", "--bw-cap-bps", "10 mbps", "server"]);
        assert_eq!(cfg.bw_cap_bps, Some(10_000_000));

        let cfg = parse_ok(&["quelay-agent", "--bw-cap-bps", "10MBPS", "server"]);
        assert_eq!(cfg.bw_cap_bps, Some(10_000_000));
    }

    // ------------------------------------------------------------
    // Bandwidth parsing (negative)
    // ------------------------------------------------------------

    #[test]
    fn bw_cap_rejects_missing_unit() {
        // ---
        parse_err(&["quelay-agent", "--bw-cap-bps", "10", "server"]);
    }

    #[test]
    fn bw_cap_rejects_invalid_number() {
        // ---

        parse_err(&["quelay-agent", "--bw-cap-bps", "abcMbps", "server"]);
    }

    #[test]
    fn bw_cap_rejects_unknown_unit() {
        // ---

        parse_err(&["quelay-agent", "--bw-cap-bps", "10Foo", "server"]);
    }

    #[test]
    fn bw_cap_rejects_zero() {
        // ---

        parse_err(&["quelay-agent", "--bw-cap-bps", "0Mbps", "server"]);
    }

    // ------------------------------------------------------------
    // Validate()
    // ------------------------------------------------------------

    #[test]
    fn validate_rejects_invalid_chunk_size() {
        // ---

        let mut cfg = parse_ok(&["quelay-agent", "server"]);
        cfg.chunk_size_bytes = 0;
        assert!(cfg.validate().is_err());

        cfg.chunk_size_bytes = 70_000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_spool_capacity() {
        // ---

        let mut cfg = parse_ok(&["quelay-agent", "server"]);
        cfg.spool_capacity_bytes = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        // ---

        let cfg = parse_ok(&["quelay-agent", "server"]);
        assert!(cfg.validate().is_ok());
    }

    // ------------------------------------------------------------
    // bw_cap_display()
    // ------------------------------------------------------------

    #[test]
    fn bw_cap_display_uncapped() {
        // ---

        let cfg = parse_ok(&["quelay-agent", "server"]);
        assert_eq!(cfg.bw_cap_display(), "uncapped");
    }

    #[test]
    fn bw_cap_display_thresholds() {
        // ---

        let mut cfg = parse_ok(&["quelay-agent", "server"]);

        cfg.bw_cap_bps = Some(500_000);
        assert_eq!(cfg.bw_cap_display(), "500.0 Kbps");

        cfg.bw_cap_bps = Some(1_000_000);
        assert_eq!(cfg.bw_cap_display(), "1.0 Mbps");

        cfg.bw_cap_bps = Some(1_500_000_000);
        assert_eq!(cfg.bw_cap_display(), "1.5 Gbps");
    }

    // ------------------------------------------------------------
    // Subcommand parsing
    // ------------------------------------------------------------

    #[test]
    fn server_mode_parses_defaults() {
        // ---

        let cfg = parse_ok(&["quelay-agent", "server"]);
        assert!(matches!(cfg.mode, crate::Mode::Server { .. }));
    }

    #[test]
    fn client_mode_requires_peer_and_cert() {
        // ---

        parse_err(&["quelay-agent", "client", "--peer", "127.0.0.1:5000"]);

        parse_err(&["quelay-agent", "client", "--cert", "server.der"]);
    }

    #[test]
    fn client_mode_parses_valid() {
        // ---

        // numeric IP
        let cfg = parse_ok(&[
            "quelay-agent",
            "client",
            "--peer",
            "127.0.0.1:5000",
            "--cert",
            "server.der",
        ]);
        assert!(matches!(cfg.mode, crate::Mode::Client { .. }));

        // hostname — DNS resolved at connect time, not parse time
        let cfg = parse_ok(&[
            "quelay-agent",
            "client",
            "--peer",
            "agent-server:4433",
            "--cert",
            "server.der",
        ]);
        assert!(matches!(cfg.mode, crate::Mode::Client { .. }));
    }
}
