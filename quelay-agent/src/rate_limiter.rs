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
//!     │  RateCmd (LinkDown/LinkUp/Finish)       │
//!     ├──────────────────► cmd_tx ──────────────┤
//!     │                                         │
//!     │                                         ├─ drain pending writes
//!     │                                         ├─ shutdown() (FIN) after drain
//!     │                                         │
//!     └──── poll_error() ◄── err_rx ◄───────────┘
//! ```
//!
//! The pump enqueues pre-encoded chunks (header + payload) and calls
//! `poll_error` at the top of each loop iteration to detect write failures
//! reported asynchronously by the timer task.  On error the pump rewinds
//! `Q = A`, splits the new stream, sends the new read half to the ack task
//! via `stream_ack_tx`, then calls `rate_limiter.link_up(new_write_half)` —
//! no `RateLimiter` reconstruction needed.
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
//! The pump calls `link_down()` to notify the timer task that the link is
//! going down.  The timer task receives [`RateCmd::LinkDown`], drains and
//! discards queued chunks, reports `ConnectionAborted` via `err_tx`, then
//! **blocks** on `cmd_rx` waiting for [`RateCmd::LinkUp`].
//!
//! The pump calls `link_up(new_tx)` after splitting the fresh reconnect stream,
//! which unblocks the timer task with the correct write half already in hand.
//! This eliminates the race where the timer task could start writing with
//! link_enabled=true but using a stale write half.

use std::io;

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
// RateCmd
// ---------------------------------------------------------------------------

/// Control commands sent from the pump to the timer task.
pub(crate) enum RateCmd {
    // ---
    /// Link went down.  Timer task should drain+discard queued chunks,
    /// report an error, then block waiting for [`RateCmd::LinkUp`].
    LinkDown,

    /// Link is back up with a fresh QUIC write half.
    LinkUp(WriteHalf<QueLayStreamPtr>),

    /// Pump reached EOF — timer task should shut down the current QUIC write half
    /// (send FIN) but remain alive to handle a reconnect replay if needed.
    Finish,

    /// Pump is done — timer task should exit cleanly.
    #[cfg(false)]
    Shutdown,
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

    /// mpsc channel capacity — a few ticks of headroom so the pump is rarely
    /// back-pressured waiting for the timer to drain.
    pub channel_capacity: usize,
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

        // Channel: 4 ticks of chunks, minimum 16 slots.
        let chunks_per_tick = budget_bytes.div_ceil(chunk_size);
        let channel_capacity = (chunks_per_tick * 4).max(16);

        tracing::debug!(
            rate_bps,
            rate_bytes_per_sec,
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
    cmd_rx: mpsc::Receiver<RateCmd>,
    err_tx: mpsc::Sender<String>,
    done: bool,
    finishing: bool,
}

impl TimerTask {
    // ---
    async fn run(mut self) {
        // ---
        let mut ticker = interval(self.interval);
        // Skip missed ticks — "discard unused budget" semantics.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        while !self.done {
            // ---
            // Wait for the next tick OR a cmd that should fire first.

            tokio::select! {
                _ = ticker.tick() => {
                    // Normal tick: drain up to budget_bytes.
                    let mut remaining = self.budget_bytes;
                    loop {
                        if remaining == 0 {
                            break; // budget exhausted — discard remainder
                        }
                        match self.rx.try_recv() {
                            Ok(chunk) => {
                                tracing::debug!("timer task: sending chunk !!");
                                if let Err(e) = self.quic_tx.write_all(&chunk).await {
                                    let _ = self.err_tx.try_send(format!("QUIC write: {e}"));
                                    // Fatal write error — wait for LinkUp with fresh tx.
                                    if !self.wait_for_link_up().await {
                                        return; // Shutdown received
                                    }
                                    ticker.reset();
                                    break;
                                }
                                remaining = remaining.saturating_sub(chunk.len());
                            }
                            Err(mpsc::error::TryRecvError::Empty) => {
                                tracing::info!("timer task: empty finishing:{}", self.finishing);
                                if self.finishing {
                                    tracing::info!("timer task: empty AND finishing");
                                    let _ = self.quic_tx.shutdown().await;
                                    tracing::info!("timer task: set done true");
                                    self.done = true;
                                }
                                break
                            },
                            Err(mpsc::error::TryRecvError::Disconnected) => return,
                        }
                    }
                    // Any remaining budget is discarded — intentional.
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(RateCmd::LinkDown) => {
                            // Drain and discard queued chunks.
                            while self.rx.try_recv().is_ok() {}
                            // Report the link-down error once so the pump rewinds.
                            let _ = self.err_tx.try_send("link disabled".into());
                            // Block until the pump delivers a fresh write half.
                            if !self.wait_for_link_up().await {
                                return; // Shutdown received
                            }
                            ticker.reset();
                        }
                        Some(RateCmd::LinkUp(_)) => {
                            // Unexpected LinkUp outside of wait_for_link_up.
                            tracing::debug!("timer task: unexpected LinkUp in running state — ignoring");
                        }
                        Some(RateCmd::Finish) => {
                            tracing::debug!("timer task: got RateCmd::Finish !!");
                            self.finishing = true;
                            continue;
                        }
                        None => return,
                    }
                }
            }
        }
        tracing::info!("timer task: exiting.........");
    }

    // ---

    /// Block until `RateCmd::LinkUp(new_tx)` arrives; swap `quic_tx`.
    /// Returns `true` if link came back up, `false` if `Shutdown` arrived.
    async fn wait_for_link_up(&mut self) -> bool {
        // ---
        // Drain any chunks that arrived while we were processing the error.
        while self.rx.try_recv().is_ok() {}

        loop {
            match self.cmd_rx.recv().await {
                Some(RateCmd::LinkUp(new_tx)) => {
                    // Discard any chunks that arrived during the outage.
                    while self.rx.try_recv().is_ok() {}
                    self.quic_tx = new_tx;
                    tracing::debug!("timer task: link up — new write half installed");
                    return true;
                }
                Some(RateCmd::LinkDown) => {
                    // Duplicate LinkDown — drain again (defensive).
                    while self.rx.try_recv().is_ok() {}
                    tracing::warn!("timer task: duplicate LinkDown while waiting for LinkUp");
                }
                Some(RateCmd::Finish) => {
                    tracing::warn!("timer task: got RateCmd::Finish !!");
                    self.finishing = true;
                    continue;
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
    /// `None` when uncapped — `enqueue` writes directly to `direct_tx`.
    chunk_tx: Option<mpsc::Sender<Vec<u8>>>,

    /// Command channel to the timer task (capped mode only).
    cmd_tx: Option<mpsc::Sender<RateCmd>>,

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
    /// - `rate_bps = Some(n)`: `n` is in **bits/s**.  Spawns a timer task
    ///   that meters at `n` bps.
    pub fn new(quic_tx: WriteHalf<QueLayStreamPtr>, rate_bps: Option<u64>) -> Self {
        // ---
        use super::CHUNK_SIZE;

        // Dummy error channel for uncapped mode — never sent to.
        let (_, err_rx_dummy) = mpsc::channel(1);

        match rate_bps {
            None => Self {
                chunk_tx: None,
                cmd_tx: None,
                direct_tx: Some(quic_tx),
                err_rx: err_rx_dummy,
            },

            Some(bps) => {
                let params = RateParams::from_rate_bps(bps, CHUNK_SIZE);
                let (chunk_tx, chunk_rx) = mpsc::channel(params.channel_capacity);
                let (cmd_tx, cmd_rx) = mpsc::channel(8);
                let (err_tx, err_rx) = mpsc::channel(4);

                tokio::spawn(
                    TimerTask {
                        rx: chunk_rx,
                        quic_tx,
                        budget_bytes: params.budget_bytes,
                        interval: params.interval,
                        cmd_rx,
                        err_tx,
                        done: false,
                        finishing: false,
                    }
                    .run(),
                );

                Self {
                    chunk_tx: Some(chunk_tx),
                    cmd_tx: Some(cmd_tx),
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
    /// Call at the top of each pump loop iteration.  Returns `Some(err)`
    /// if the timer task has reported a write failure or link-down event.
    pub fn poll_error(&mut self) -> Option<String> {
        self.err_rx.try_recv().ok()
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
    /// The timer task will drain+discard its queue, report an error via
    /// `err_tx`, then block waiting for `link_up`.  Call this before
    /// `stream_rx.recv()` so the timer task has already discarded stale
    /// chunks by the time the pump rewinds `Q = A`.
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
    // Tell timer task that it should drain (finish) then exit.
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

    // ---

    /// Shutdown the write half cleanly (called on EOF path).
    #[cfg(false)]
    pub async fn shutdown(&mut self) -> io::Result<()> {
        match &mut self.direct_tx {
            Some(tx) => tx.shutdown().await,
            // Capped: signal the timer task to exit cleanly.
            None => {
                if let Some(cmd_tx) = &self.cmd_tx {
                    let _ = cmd_tx.send(RateCmd::Shutdown).await;
                }
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
