//! Rate limiter — metered write path for the uplink QUIC pump.
//!
//! [`RateLimiter`] owns the write half of a QUIC stream and meters bytes into
//! it at a configured bits-per-second ceiling.  A dedicated timer task wakes
//! on a computed interval, drains a bounded mpsc queue up to a byte budget,
//! and then discards any unused budget before sleeping again.  This mirrors
//! the C++ `sendAsync` / `dataRateTimerHandler` design.
//!
//! ## Architecture
//!
//! ```text
//! run_quic_pump
//!     │  enqueue(chunk)         mpsc             tokio::interval
//!     ├──────────────────► chunk_tx ──────► TimerTask ──► quic WriteHalf
//!     │                                         │
//!     └──── poll_error() ◄── err_rx ◄───────────┘
//! ```
//!
//! The pump enqueues pre-encoded chunks (header + payload) and calls
//! `poll_error` at the top of each loop iteration to detect write failures
//! reported asynchronously by the timer task.  On error the pump rewinds
//! `Q = A` and calls `stream_rx.recv()` for a fresh stream, then constructs
//! a new [`RateLimiter`] wrapping the new write half.
//!
//! ## Timer interval
//!
//! We target [`CHUNKS_PER_TICK`] chunks per wake-up so each tick does
//! meaningful work without waking too frequently:
//!
//! ```text
//! bytes_per_tick = CHUNKS_PER_TICK * CHUNK_SIZE
//! interval_ms    = bytes_per_tick * 1000 / rate_bytes_per_sec
//! ```
//!
//! Clamped to [`[MIN_INTERVAL_MS, MAX_INTERVAL_MS]`].  When clamped,
//! `bytes_per_tick` is recalculated from the clamped interval so long-term
//! throughput always equals the configured rate.
//!
//! ## Link-down injection
//!
//! The [`SessionManager`] sets a shared `link_enabled: Arc<AtomicBool>` to
//! `false` to simulate a satellite link outage.  The timer task checks this
//! flag at the top of every tick; when `false` it reports `ConnectionAborted`
//! via `err_tx`, drains and discards queued chunks, and continues ticking
//! (so it wakes cleanly when the link is re-enabled after reconnect).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---

use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, MissedTickBehavior};

// ---

use quelay_domain::QueLayStreamPtr;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Chunks to send per timer tick in the unclamped case.
const CHUNKS_PER_TICK: usize = 8;

/// Minimum timer interval — below this the wakeup overhead dominates.
const MIN_INTERVAL_MS: u64 = 5;

/// Maximum timer interval — above this queue latency becomes noticeable and
/// the pump's back-pressure channel fills up.
const MAX_INTERVAL_MS: u64 = 100;

// ---------------------------------------------------------------------------
// RateParams
// ---------------------------------------------------------------------------

/// Pre-computed parameters derived from the configured `rate_bps`.
/// Computed once at construction; logged for diagnostics.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateParams {
    /// Timer wake-up period.
    pub interval: Duration,
    /// Maximum bytes forwarded per tick.  Unused budget is discarded.
    pub budget_bytes: usize,
    /// mpsc channel capacity — a few ticks of headroom so the pump is rarely
    /// back-pressured waiting for the timer to drain.
    pub channel_capacity: usize,
}

impl RateParams {
    pub(crate) fn from_rate_bps(rate_bps: u64, chunk_size: usize) -> Self {
        // ---
        let rate_bps_bytes = rate_bps / 8; // bytes per second

        // Ideal: send CHUNKS_PER_TICK chunks per tick.
        let ideal_bytes_per_tick = (CHUNKS_PER_TICK * chunk_size) as u64;
        let ideal_ms = ideal_bytes_per_tick * 1000 / rate_bps_bytes;
        let interval_ms = ideal_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);

        // Recalculate budget from clamped interval to keep long-term rate correct.
        let budget_bytes = (rate_bps_bytes * interval_ms / 1000) as usize;

        // Channel: 4 ticks of chunks, minimum 16 slots.
        let chunks_per_tick = budget_bytes.div_ceil(chunk_size);
        let channel_capacity = (chunks_per_tick * 4).max(16);

        tracing::debug!(
            rate_bps,
            interval_ms,
            budget_bytes,
            channel_capacity,
            "RateLimiter: params",
        );

        Self {
            interval: Duration::from_millis(interval_ms),
            budget_bytes,
            channel_capacity,
        }
    }
}

// ---------------------------------------------------------------------------
// TimerTask
// ---------------------------------------------------------------------------

struct TimerTask {
    rx: mpsc::Receiver<Vec<u8>>,
    quic_tx: WriteHalf<QueLayStreamPtr>,
    budget_bytes: usize,
    interval: Duration,
    link_enabled: Arc<AtomicBool>,
    err_tx: mpsc::Sender<String>,
}

impl TimerTask {
    async fn run(mut self) {
        // ---
        let mut ticker = interval(self.interval);
        // Skip missed ticks — "discard unused budget" semantics.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            // Link-down check.
            if !self.link_enabled.load(Ordering::Relaxed) {
                // Drain and discard queued chunks so they don't pile up.
                while self.rx.try_recv().is_ok() {}
                // Report the error once — pump will replace us on reconnect.
                let _ = self.err_tx.try_send("link disabled".into());
                // Keep ticking so we're ready when link comes back; if the
                // channel is closed (pump dropped us) we exit below.
                continue;
            }

            // Drain up to budget_bytes from the queue.
            let mut remaining = self.budget_bytes;
            loop {
                if remaining == 0 {
                    break; // budget exhausted — unused budget discarded here
                }
                match self.rx.try_recv() {
                    Ok(chunk) => {
                        if let Err(e) = self.quic_tx.write_all(&chunk).await {
                            let _ = self.err_tx.try_send(format!("QUIC write: {e}"));
                            return; // fatal — pump will replace us
                        }
                        remaining = remaining.saturating_sub(chunk.len());
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            // Any remaining budget is discarded — intentional.
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Metered write path.  One `RateLimiter` per QUIC stream lifetime.
///
/// On reconnect the pump drops the old `RateLimiter` (which stops the timer
/// task) and constructs a new one wrapping the fresh write half.
pub struct RateLimiter {
    // ---
    /// `None` when uncapped — `enqueue` writes directly to `direct_tx`.
    chunk_tx: Option<mpsc::Sender<Vec<u8>>>,

    /// Direct write half for uncapped (no rate limit) mode.
    direct_tx: Option<WriteHalf<QueLayStreamPtr>>,

    /// Error reports from the timer task.
    err_rx: mpsc::Receiver<String>,
}

impl RateLimiter {
    // ---

    /// Construct a rate limiter wrapping `quic_tx`.
    ///
    /// - `rate_bps = None`: uncapped — writes go directly to `quic_tx`, no
    ///   timer task is spawned.
    /// - `rate_bps = Some(n)`: spawns a timer task that meters at `n` bps.
    pub fn new(
        quic_tx: WriteHalf<QueLayStreamPtr>,
        rate_bps: Option<u64>,
        link_enabled: Arc<AtomicBool>,
    ) -> Self {
        // ---
        use super::CHUNK_SIZE;

        // Dummy error channel for uncapped mode — never sent to.
        let (_, err_rx_dummy) = mpsc::channel(1);

        match rate_bps {
            None => Self {
                chunk_tx: None,
                direct_tx: Some(quic_tx),
                err_rx: err_rx_dummy,
            },

            Some(bps) => {
                let params = RateParams::from_rate_bps(bps, CHUNK_SIZE);
                let (chunk_tx, chunk_rx) = mpsc::channel(params.channel_capacity);
                let (err_tx, err_rx) = mpsc::channel(4);

                tokio::spawn(
                    TimerTask {
                        rx: chunk_rx,
                        quic_tx,
                        budget_bytes: params.budget_bytes,
                        interval: params.interval,
                        link_enabled,
                        err_tx,
                    }
                    .run(),
                );

                Self {
                    chunk_tx: Some(chunk_tx),
                    direct_tx: None,
                    err_rx,
                }
            }
        }
    }

    // ---

    /// Enqueue a pre-encoded chunk (chunk header + payload) for metered
    /// delivery to the QUIC stream.
    ///
    /// In uncapped mode, writes directly and awaits completion.
    /// In capped mode, sends to the mpsc channel; blocks if the channel is
    /// full (back-pressure when the pump is outrunning the rate).
    pub async fn enqueue(&mut self, chunk: Vec<u8>) -> io::Result<()> {
        // ---
        match (&self.chunk_tx, &mut self.direct_tx) {
            (Some(tx), None) => tx
                .send(chunk)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "timer task exited")),
            (None, Some(tx)) => tx.write_all(&chunk).await,
            _ => unreachable!(),
        }
    }

    // ---

    /// Check for an error reported by the timer task.
    ///
    /// Call at the top of each pump loop iteration — equivalent to checking
    /// the return value of the old `write_chunk` call.  Returns `Some(err)`
    /// if the timer task has reported a write failure or link-down event.
    pub fn poll_error(&mut self) -> Option<String> {
        self.err_rx.try_recv().ok()
    }

    // ---

    /// Shutdown the write half cleanly (called on EOF path).
    pub async fn shutdown(&mut self) -> io::Result<()> {
        match &mut self.direct_tx {
            Some(tx) => tx.shutdown().await,
            // Capped: the timer task owns quic_tx; signal it to shut down by
            // dropping chunk_tx (closing the mpsc channel), then wait briefly.
            None => {
                self.chunk_tx = None;
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{RateParams, MAX_INTERVAL_MS, MIN_INTERVAL_MS};
    use quelay_domain::CHUNK_SIZE;

    #[test]
    fn rate_params_high_rate_clamps_to_min() {
        let p = RateParams::from_rate_bps(10_000_000_000, CHUNK_SIZE);
        assert_eq!(p.interval.as_millis() as u64, MIN_INTERVAL_MS);
    }

    #[test]
    fn rate_params_low_rate_clamps_to_max() {
        let p = RateParams::from_rate_bps(100_000, CHUNK_SIZE);
        assert_eq!(p.interval.as_millis() as u64, MAX_INTERVAL_MS);
    }

    #[test]
    fn rate_params_100mbit() {
        let p = RateParams::from_rate_bps(100_000_000, CHUNK_SIZE);
        let ms = p.interval.as_millis() as u64;
        assert!(
            ms >= MIN_INTERVAL_MS && ms <= MAX_INTERVAL_MS,
            "interval {ms}ms out of [{MIN_INTERVAL_MS},{MAX_INTERVAL_MS}]"
        );
        let expected_budget = (100_000_000u64 / 8) * ms / 1000;
        let delta = (p.budget_bytes as i64 - expected_budget as i64).abs();
        assert!(delta < CHUNK_SIZE as i64, "budget delta {delta} too large");
    }

    #[test]
    fn rate_params_10mbit() {
        let p = RateParams::from_rate_bps(10_000_000, CHUNK_SIZE);
        let ms = p.interval.as_millis() as u64;
        assert!(
            ms >= MIN_INTERVAL_MS && ms <= MAX_INTERVAL_MS,
            "interval {ms}ms out of [{MIN_INTERVAL_MS},{MAX_INTERVAL_MS}]"
        );
    }

    #[test]
    fn rate_params_channel_capacity_reasonable() {
        // Channel should be at least 16 and at most a few hundred slots.
        for rate_bps in [1_000_000, 10_000_000, 100_000_000, 1_000_000_000] {
            let p = RateParams::from_rate_bps(rate_bps, CHUNK_SIZE);
            assert!(p.channel_capacity >= 16, "capacity too small at {rate_bps}");
            assert!(
                p.channel_capacity <= 512,
                "capacity {cap} suspiciously large at {rate_bps}",
                cap = p.channel_capacity
            );
        }
    }
}
