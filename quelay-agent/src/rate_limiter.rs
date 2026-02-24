//! Rate limiter — metered write path for the uplink QUIC stream.
//!
//! [`RateLimiter`] owns the write half of a QUIC stream and meters bytes into
//! it at a configured bits-per-second ceiling.  A dedicated timer task wakes
//! on a computed interval, reads directly from the [`SpoolBuffer`] up to a
//! byte budget per tick, encodes chunk headers inline, and writes to the QUIC
//! stream.  Unused budget is discarded before the next tick.
//!
//! ## Architecture
//!
//! ```text
//! TCP reader task
//!     │  push(data)
//!     ▼
//! SpoolBuffer
//!     │
//!     │  timer task reads slice_from(q) up to budget_bytes per tick
//!     ▼
//! QUIC WriteHalf
//!     ▲
//! cmd_tx mpsc
//! (LinkDown / LinkUp / Finish)
//! ```
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
//! ## Link-down / link-up sequencing
//!
//! On [`RateCmd::LinkDown`] the timer task rewinds `q = spool.bytes_acked`
//! (no data is discarded — the spool retains it for replay), reports an error
//! via `err_tx`, then **blocks** on `cmd_rx` waiting for [`RateCmd::LinkUp`].
//!
//! On [`RateCmd::LinkUp(new_tx)`] the timer task installs the fresh write half
//! and resumes from the rewound `q`.  This eliminates the race where the timer
//! task could start writing with a stale write half.

use std::io;
use std::sync::Arc;

// ---

use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, Duration, MissedTickBehavior};

// ---

use quelay_domain::QueLayStreamPtr;

// ---

use super::SpoolBuffer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Chunks to send per timer tick in the unclamped case.
const CHUNKS_PER_TICK: usize = 8;

/// Minimum timer interval — below this the wakeup overhead dominates.
const MIN_INTERVAL_MS: u64 = 5;

/// Maximum timer interval — above this latency becomes noticeable.
const MAX_INTERVAL_MS: u64 = 100;

// ---------------------------------------------------------------------------
// encode_chunk
// ---------------------------------------------------------------------------

/// Encode a chunk into a flat `Vec<u8>` (10-byte header + payload).
///
/// Header layout (big-endian):
/// - bytes 0..8 : `stream_offset` as `u64`
/// - bytes 8..10: `payload.len()` as `u16`
pub(crate) fn encode_chunk(stream_offset: u64, payload: &[u8]) -> Vec<u8> {
    // ---
    use crate::CHUNK_HEADER_LEN;
    let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN + payload.len());
    buf.extend_from_slice(&stream_offset.to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

// ---------------------------------------------------------------------------
// RateCmd
// ---------------------------------------------------------------------------

/// Control commands sent to the timer task.
pub(crate) enum RateCmd {
    // ---
    /// Link went down.  Timer task rewinds `q = spool.bytes_acked`, reports
    /// an error, then blocks waiting for [`RateCmd::LinkUp`].
    LinkDown,

    /// Link is back up with a fresh QUIC write half.
    LinkUp(WriteHalf<QueLayStreamPtr>),

    /// Uplink EOF — timer task should drain remaining spool data, send FIN,
    /// then exit.
    Finish,
}

// ---------------------------------------------------------------------------
// RateParams
// ---------------------------------------------------------------------------

/// Pre-computed parameters derived from the configured `rate_bps`.
/// Computed once at construction; logged for diagnostics.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateParams {
    // ---
    /// Timer wake-up period.
    pub interval: Duration,

    /// Maximum bytes forwarded per tick.  Unused budget is discarded.
    pub budget_bytes: usize,
}

impl RateParams {
    /// `rate_bps` is in **bits per second** (e.g. 100_000_000 for 100 Mbit/s).
    pub(crate) fn from_rate_bps(rate_bps: u64, chunk_size: usize) -> Self {
        // ---
        let rate_bytes_per_sec = rate_bps / 8; // bits/s → bytes/s

        // Ideal: send CHUNKS_PER_TICK chunks per tick.
        let ideal_bytes_per_tick = (CHUNKS_PER_TICK * chunk_size) as u64;
        let ideal_ms = ideal_bytes_per_tick * 1000 / rate_bytes_per_sec;
        let interval_ms = ideal_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);

        // Recalculate budget from clamped interval to keep long-term rate correct.
        let budget_bytes = (rate_bytes_per_sec * interval_ms / 1000) as usize;

        tracing::debug!(
            rate_bps,
            rate_bytes_per_sec,
            interval_ms,
            budget_bytes,
            "RateLimiter: params",
        );

        Self {
            interval: Duration::from_millis(interval_ms),
            budget_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// TimerTask
// ---------------------------------------------------------------------------

struct TimerTask {
    spool: Arc<Mutex<SpoolBuffer>>,
    /// Q pointer — absolute byte offset of the next byte to send.
    q: u64,
    quic_tx: WriteHalf<QueLayStreamPtr>,
    budget_bytes: usize,
    interval: Duration,
    cmd_rx: mpsc::Receiver<RateCmd>,
    done: bool,
    finishing: bool,
}

impl TimerTask {
    // ---
    async fn run(mut self) {
        // ---
        use super::CHUNK_SIZE;

        let mut ticker = interval(self.interval);
        // Skip missed ticks — "discard unused budget" semantics.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        while !self.done {
            // ---
            tokio::select! {
                _ = ticker.tick() => {
                    self.drain_tick(CHUNK_SIZE).await;
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(RateCmd::LinkDown) => {
                            // Rewind Q; spool retains data for replay on reconnect.
                            self.q = self.spool.lock().await.bytes_acked;
                            tracing::info!("timer task: link down — rewound Q, waiting for LinkUp");
                            if !self.wait_for_link_up().await {
                                return;
                            }
                            ticker.reset();
                        }
                        Some(RateCmd::LinkUp(_)) => {
                            tracing::debug!(
                                "timer task: unexpected LinkUp in running state — ignoring"
                            );
                        }
                        Some(RateCmd::Finish) => {
                            tracing::debug!("timer task: got RateCmd::Finish");
                            self.finishing = true;
                        }
                        None => return,
                    }
                }
            }
        }
        tracing::info!("timer task: exiting");
    }

    // ---

    /// Drain up to `budget_bytes` from the spool into the QUIC stream.
    ///
    /// Encodes each chunk inline (header + payload) and writes to `quic_tx`.
    /// On write error, logs a warning and waits for a fresh write half via `wait_for_link_up`.
    /// Sets `self.done = true` when the spool EOF sentinel is drained and
    /// `self.finishing` is set.
    async fn drain_tick(&mut self, chunk_size: usize) {
        // ---
        let mut remaining = self.budget_bytes;

        loop {
            if remaining == 0 {
                break; // budget exhausted
            }

            let chunk = {
                let s = self.spool.lock().await;
                let head = s.head;
                // EOF sentinel: spool fully drained.
                if head == u64::MAX && s.head_offset() <= self.q {
                    drop(s);
                    if self.finishing {
                        tracing::info!("timer task: spool drained, sending FIN");
                        let _ = self.quic_tx.shutdown().await;
                        self.done = true;
                    }
                    break;
                }
                if s.head_offset() <= self.q {
                    break; // nothing new yet
                }
                let slice = s.slice_from(self.q);
                let n = slice.len().min(chunk_size).min(remaining);
                slice[..n].to_vec()
            };

            if chunk.is_empty() {
                break;
            }

            let encoded = encode_chunk(self.q, &chunk);
            if let Err(e) = self.quic_tx.write_all(&encoded).await {
                tracing::warn!("timer task: QUIC write error: {e} — waiting for LinkUp");
                if !self.wait_for_link_up().await {
                    self.done = true;
                }
                return;
            }
            self.q += chunk.len() as u64;
            remaining = remaining.saturating_sub(chunk.len());
        }
    }

    // ---

    /// Block until `RateCmd::LinkUp(new_tx)` arrives; swap `quic_tx` and
    /// rewind `q = spool.bytes_acked`.
    ///
    /// Returns `true` if link came back up, `false` if the cmd channel closed.
    async fn wait_for_link_up(&mut self) -> bool {
        // ---
        loop {
            match self.cmd_rx.recv().await {
                Some(RateCmd::LinkUp(new_tx)) => {
                    self.q = self.spool.lock().await.bytes_acked;
                    self.quic_tx = new_tx;
                    tracing::debug!(q = self.q, "timer task: link up — new write half installed");
                    return true;
                }
                Some(RateCmd::LinkDown) => {
                    // Duplicate LinkDown — rewind again (defensive).
                    self.q = self.spool.lock().await.bytes_acked;
                    tracing::warn!("timer task: duplicate LinkDown while waiting for LinkUp");
                }
                Some(RateCmd::Finish) => {
                    tracing::warn!("timer task: got RateCmd::Finish while waiting for LinkUp");
                    self.finishing = true;
                }
                None => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Metered write path.  One `RateLimiter` per uplink stream lifetime;
/// survives link outages via [`link_down`] / [`link_up`].
pub struct RateLimiter {
    // ---
    /// Command channel to the timer task (capped mode only).
    cmd_tx: Option<mpsc::Sender<RateCmd>>,

    /// Direct write half for uncapped (no rate limit) mode.
    direct_tx: Option<WriteHalf<QueLayStreamPtr>>,
}

impl RateLimiter {
    // ---

    /// Construct a rate limiter wrapping `quic_tx`.
    ///
    /// - `rate_bps = None`: uncapped — writes go directly to `quic_tx`, no
    ///   timer task is spawned.
    /// - `rate_bps = Some(n)`: `n` is in **bits/s**.  Spawns a timer task
    ///   that reads directly from `spool` at `n` bps on each tick.
    pub fn new(
        quic_tx: WriteHalf<QueLayStreamPtr>,
        rate_bps: Option<u64>,
        spool: Arc<Mutex<SpoolBuffer>>,
    ) -> Self {
        // ---
        use super::CHUNK_SIZE;

        match rate_bps {
            None => Self {
                cmd_tx: None,
                direct_tx: Some(quic_tx),
            },

            Some(bps) => {
                let params = RateParams::from_rate_bps(bps, CHUNK_SIZE);
                let (cmd_tx, cmd_rx) = mpsc::channel(8);

                tokio::spawn(
                    TimerTask {
                        spool,
                        q: 0,
                        quic_tx,
                        budget_bytes: params.budget_bytes,
                        interval: params.interval,
                        cmd_rx,
                        done: false,
                        finishing: false,
                    }
                    .run(),
                );

                Self {
                    cmd_tx: Some(cmd_tx),
                    direct_tx: None,
                }
            }
        }
    }

    // ---

    /// Return a clone of the cmd_tx for storage in [`UplinkHandle`].
    ///
    /// The session manager uses this to call `link_down()` on all active
    /// uplinks when `link_enable(false)` fires.  Returns `None` when uncapped.
    pub fn link_down_tx_clone(&self) -> Option<mpsc::Sender<RateCmd>> {
        self.cmd_tx.clone()
    }

    // ---

    /// Signal the timer task that the link is down.
    ///
    /// The timer task rewinds `q = spool.bytes_acked`, logs the event,
    /// then blocks waiting for `link_up`.
    ///
    /// No-op in uncapped mode (direct write path handles errors inline).
    pub fn link_down(&self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(RateCmd::LinkDown);
        }
    }

    // ---

    /// Hand the timer task a fresh QUIC write half after reconnect.
    ///
    /// The timer task was blocked in `wait_for_link_up`; this unblocks it
    /// so it can resume metered writes on the new stream immediately.
    ///
    /// In uncapped mode, replaces `direct_tx` in-place.
    pub async fn link_up(&mut self, new_tx: WriteHalf<QueLayStreamPtr>) -> io::Result<()> {
        // ---
        match (&self.cmd_tx, &mut self.direct_tx) {
            (Some(cmd_tx), None) => cmd_tx
                .send(RateCmd::LinkUp(new_tx))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "timer task exited")),
            (None, Some(direct)) => {
                *direct = new_tx;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    // ---

    /// Tell the timer task to drain remaining spool data, send FIN, then exit.
    pub async fn finish(&mut self) -> io::Result<()> {
        // ---
        if let Some(cmd_tx) = &self.cmd_tx {
            cmd_tx
                .send(RateCmd::Finish)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "timer task exited"))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{RateParams, MAX_INTERVAL_MS, MIN_INTERVAL_MS};

    // Mirror CHUNK_SIZE from framing.rs — avoid a cross-crate dep in tests.
    const CHUNK_SIZE: usize = 16 * 1024;

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
            (MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&ms),
            "interval {ms}ms out of [{MIN_INTERVAL_MS},{MAX_INTERVAL_MS}]"
        );
        let rate_bytes = 100_000_000u64 / 8;
        let expected_budget = rate_bytes * ms / 1000;
        let delta = (p.budget_bytes as i64 - expected_budget as i64).abs();
        assert!(delta < CHUNK_SIZE as i64, "budget delta {delta} too large");
    }

    #[test]
    fn rate_params_10mbit() {
        let p = RateParams::from_rate_bps(10_000_000, CHUNK_SIZE);
        let ms = p.interval.as_millis() as u64;
        assert!(
            (MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&ms),
            "interval {ms}ms out of [{MIN_INTERVAL_MS},{MAX_INTERVAL_MS}]"
        );
    }
}
