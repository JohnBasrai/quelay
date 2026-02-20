use std::time::Duration;

// ---------------------------------------------------------------------------
// LinkSimConfig
// ---------------------------------------------------------------------------

/// Configuration for the in-process link simulator.
///
/// All fields default to a perfect link — no drops, no cap, no outage.
#[derive(Debug, Clone)]
pub struct LinkSimConfig {
    // ---
    /// Probability `[0.0, 1.0]` that any given write is silently dropped.
    pub drop_percent: f64,

    /// Probability `[0.0, 1.0]` that any given write is duplicated.
    pub dup_percent: f64,

    /// Caps total throughput using a token bucket. `None` = unlimited.
    pub bw_cap_bps: Option<u64>,

    /// If `Some`, the link goes dark for this duration once, starting
    /// after `outage_delay`. Tests spool-and-resume behaviour.
    pub outage: Option<Duration>,
    /// Delay before the outage begins after first send.
    pub outage_delay: Duration,
    /// RNG seed for reproducible drop / dup sequences. `None` = random.
    pub seed: Option<u64>,
}

// ---

impl Default for LinkSimConfig {
    fn default() -> Self {
        // ---
        Self {
            drop_percent: 0.0,
            dup_percent: 0.0,
            bw_cap_bps: None,
            outage: None,
            outage_delay: Duration::from_millis(100),
            seed: None,
        }
    }
}

// ---

impl LinkSimConfig {
    // ---
    /// Perfect link — no impairments. Useful as a baseline.
    pub fn perfect() -> Self {
        Self::default()
    }

    // ---

    /// Typical degraded satellite link: 5 % loss, 1 % dup, 512 kbit/s.
    pub fn degraded() -> Self {
        // ---
        Self {
            drop_percent: 0.05,
            dup_percent: 0.01,
            bw_cap_bps: Some(512_000),
            ..Default::default()
        }
    }

    // ---

    /// Link that goes completely dark for 60 seconds.
    pub fn outage_60s() -> Self {
        // ---
        Self {
            outage: Some(Duration::from_secs(60)),
            outage_delay: Duration::from_millis(200),
            ..Default::default()
        }
    }
}
