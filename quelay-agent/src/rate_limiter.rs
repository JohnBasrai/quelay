//! Rate limiter — metered write path for uplink QUIC streams.
//!
//! ## Architecture
//!
//! A single [`AggregateRateLimiter`] (ARL) is shared across all active uplink
//! streams.  One aggregate timer task wakes at a computed interval, reads each
//! stream's backlog from its [`Arc<AtomicU64>`], calls
//! [`DrrScheduler::schedule`] with the total tick budget, and sends an
//! [`AllocTicket`] to each stream's [`StreamPump`] task.
//!
//! Each [`StreamPump`] does a `select!` on two channels:
//!
//! ```text
//! alloc_rx  ← AllocTicket(n)  sent by ARL aggregate timer
//! cmd_rx    ← RateCmd         sent by AckTask (LinkDown / LinkUp / Finish)
//! ```
//!
//! On receiving an `AllocTicket` the pump drains up to `n` bytes from its
//! [`SpoolBuffer`] into the QUIC write half, then updates its head/q atomics
//! so the ARL scheduler sees current data for the next tick.
//!
//! ## Backlog tracking (Option A)
//!
//! Each registered stream's backlog is tracked via an [`Arc<AtomicU64>`]
//! written by the [`StreamPump`] after each drain and read by the ARL timer
//! before calling `schedule()`.  No extra channel needed.
//!
//! ## Uncapped mode
//!
//! When `rate_bps = None` the ARL is constructed in uncapped mode.
//! [`AggregateRateLimiter::register`] returns `None` for `alloc_rx`.
//! The [`StreamPump`] is not spawned; `RateLimiter` holds a direct write half
//! and the existing uncapped write path is preserved unchanged.
//!
//! ## Timer interval
//!
//! ```text
//! bytes_per_tick = CHUNKS_PER_TICK * CHUNK_SIZE
//! interval_ms    = bytes_per_tick * 1000 / rate_bytes_per_sec
//! ```
//!
//! Clamped to `[MIN_INTERVAL_MS, MAX_INTERVAL_MS]`.  When clamped,
//! `budget_bytes` is recalculated so long-term throughput equals the
//! configured rate.
//!
//! ## Link-down / link-up sequencing
//!
//! On [`RateCmd::LinkDown`] the [`StreamPump`] rewinds `q = spool.bytes_acked`,
//! then **blocks** on `cmd_rx` waiting for [`RateCmd::LinkUp`].  Incoming
//! [`AllocTicket`]s are silently discarded while the pump is paused.
//!
//! On [`RateCmd::LinkUp(new_tx)`] the pump installs the fresh write half and
//! resumes draining from the rewound `q`.

use std::collections::HashMap;
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

// ---

use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, Duration, MissedTickBehavior};
use uuid::Uuid;

// ---

use quelay_domain::{DrrScheduler, Priority, QueLayStreamPtr};

// ---

use super::SpoolBuffer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Chunks to send per timer tick in the unclamped case.
const CHUNKS_PER_TICK: usize = 8;

/// Minimum timer interval — below this wakeup overhead dominates.
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
// AllocTicket
// ---------------------------------------------------------------------------

/// Byte budget for one pump for one tick, sent by the ARL aggregate timer.
///
/// The [`StreamPump`] drains up to `bytes` from its spool into the QUIC
/// write half, then discards any unused budget before the next ticket arrives.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AllocTicket {
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// RateCmd
// ---------------------------------------------------------------------------

/// Control commands sent to the [`StreamPump`] via [`RateLimiter::cmd_tx`].
pub(crate) enum RateCmd {
    // ---
    /// Link went down.  Pump rewinds `q = spool.bytes_acked`, then blocks
    /// waiting for [`RateCmd::LinkUp`].
    LinkDown,

    /// Link is back up with a fresh QUIC write half.
    LinkUp(WriteHalf<QueLayStreamPtr>),

    /// Uplink EOF — pump should drain remaining spool data, send FIN,
    /// then exit.
    Finish,
}

// ---------------------------------------------------------------------------
// RateParams
// ---------------------------------------------------------------------------

/// Pre-computed timer parameters derived from `rate_bps`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateParams {
    // ---
    /// Aggregate timer wake-up period.
    pub interval: Duration,

    /// Total bytes the ARL may distribute across all streams per tick.
    pub budget_bytes: usize,
}

impl RateParams {
    /// `rate_bps` is in **bits per second** (e.g. 100_000_000 for 100 Mbit/s).
    pub(crate) fn from_rate_bps(rate_bps: u64, chunk_size: usize) -> Self {
        // ---
        let rate_bytes_per_sec = rate_bps / 8;

        let ideal_bytes_per_tick = (CHUNKS_PER_TICK * chunk_size) as u64;
        let ideal_ms = ideal_bytes_per_tick * 1000 / rate_bytes_per_sec;
        let interval_ms = ideal_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);

        let budget_bytes = (rate_bytes_per_sec * interval_ms / 1000) as usize;

        tracing::debug!(
            rate_bps,
            rate_bytes_per_sec,
            interval_ms,
            budget_bytes,
            "AggregateRateLimiter: params",
        );

        Self {
            interval: Duration::from_millis(interval_ms),
            budget_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// StreamEntry  (ARL-internal per-stream state)
// ---------------------------------------------------------------------------

struct StreamEntry {
    // ---
    /// Allocation ticket channel to the pump task.
    alloc_tx: mpsc::Sender<AllocTicket>,

    /// Absolute offset of the byte one past the last byte currently in the spool (`T`).
    ///
    /// Written by the TCP reader task after each `push()` and on EOF sentinel.
    head_offset: Arc<AtomicU64>,

    /// Pump drain cursor (`Q`) — absolute offset of the next byte to transmit.
    ///
    /// Written by the StreamPump after each successful QUIC write, and on rewind
    /// during LinkDown/LinkUp.
    q_atomic: Arc<AtomicU64>,
}

// ---------------------------------------------------------------------------
// AggregateTimerTask
// ---------------------------------------------------------------------------

struct AggregateTimerTask {
    scheduler: Arc<Mutex<DrrScheduler>>,
    streams: Arc<Mutex<HashMap<Uuid, StreamEntry>>>,
    budget_bytes: usize,
    interval: Duration,
}

impl AggregateTimerTask {
    // ---

    async fn run(self) {
        // ---
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let per_tick = self.budget_bytes as u64;
        let max_carry = per_tick * 2; // max carry over to next tick
        let mut available_budget: u64 = 0;

        loop {
            ticker.tick().await;

            // Accumulate budget with clamp (2 intervals)
            available_budget = available_budget.saturating_add(per_tick);
            if available_budget > max_carry {
                available_budget = max_carry;
            }

            // 1) Snapshot backlog + alloc channels without holding scheduler lock
            //
            // We only need:
            // - uuid
            // - backlog (atomic)
            // - alloc_tx clone (to send tickets later without holding streams lock)
            let snapshot: Vec<(Uuid, u64, tokio::sync::mpsc::Sender<AllocTicket>)> = {
                let streams = self.streams.lock().await;
                let snap: Vec<_> = streams
                    .iter()
                    .map(|(uuid, entry)| {
                        let head = entry.head_offset.load(Ordering::Acquire);
                        let q = entry.q_atomic.load(Ordering::Acquire);
                        let backlog = head.saturating_sub(q);
                        (*uuid, backlog, entry.alloc_tx.clone())
                    })
                    .collect();
                tracing::debug!(
                    n_streams = snap.len(),
                    total_backlog = snap.iter().map(|(_, b, _)| b).sum::<u64>(),
                    "ARL tick: snapshot"
                );
                snap
            };

            // 2) Schedule with scheduler lock only
            let allocs: Vec<(Uuid, u64)> = {
                let mut sched = self.scheduler.lock().await;

                // Update backlogs from snapshot
                for (uuid, backlog, _) in &snapshot {
                    sched.set_backlog(*uuid, *backlog);
                }

                match sched.schedule(available_budget) {
                    Ok(allocs) => allocs,
                    Err(err) => {
                        tracing::warn!("schedule failed:{err}");
                        continue;
                    }
                }
            };

            // 3) Deliver tickets without holding any locks, while tracking
            // how much budget was *actually* delivered to pumps.
            //
            // We only subtract delivered bytes from carryover, so that:
            // - if a pump's alloc channel is full, we don't "spend" budget
            //   the pump never receives
            // - if a pump task is gone (channel closed), we can deregister it
            let mut delivered: u64 = 0;
            let mut closed_streams: Vec<Uuid> = Vec::new();

            tracing::debug!(
                n_allocs = snapshot.len(),
                available_budget,
                "ARL tick: delivering tickets"
            );

            // Snapshot lookup: linear scan is fine at small N; if you want,
            // replace with HashMap<Uuid, AllocTx>.
            for (uuid, bytes) in allocs {
                if bytes == 0 {
                    continue;
                }
                if let Some((_, _, tx)) = snapshot.iter().find(|(id, _, _)| *id == uuid) {
                    // Non-blocking: if the pump's alloc channel is full it
                    // already has a ticket queued; discard this one so we
                    // don't pile up stale budget grants.
                    match tx.try_send(AllocTicket { bytes }) {
                        Ok(()) => {
                            tracing::debug!(%uuid, bytes, "ARL tick: ticket delivered");
                            delivered = delivered.saturating_add(bytes);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // Keep carryover; we'll attempt to deliver again
                            // on a later tick.
                            tracing::debug!(%uuid, bytes, "ARL tick: ticket FULL — pump not draining");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!(%uuid, "ARL tick: pump channel closed");
                            closed_streams.push(uuid);
                        }
                    }
                }
            }

            // 4) Reduce carryover only by delivered bytes.
            available_budget = available_budget.saturating_sub(delivered);

            // 5) Clean up streams whose pumps are gone so the scheduler doesn't
            // keep allocating to closed channels.
            if !closed_streams.is_empty() {
                {
                    let mut streams = self.streams.lock().await;
                    for uuid in &closed_streams {
                        streams.remove(uuid);
                    }
                }
                {
                    let mut sched = self.scheduler.lock().await;
                    for uuid in &closed_streams {
                        sched.deregister(*uuid);
                    }
                }
            }
        }
    } // run
} // impl AggregateTimerTask {

// ---------------------------------------------------------------------------
// AggregateRateLimiter
// ---------------------------------------------------------------------------

/// Shared rate limiter — one instance per [`SessionManager`], covering all
/// active uplink streams.
///
/// The ARL owns:
/// - A single [`DrrScheduler`] that distributes the aggregate tick budget.
/// - One [`AggregateTimerTask`] running as a background tokio task.
/// - A map of per-stream [`StreamEntry`]s holding the alloc channel and
///   head/q atomics for each active pump.
///
/// Streams register on open (`register`) and deregister on close
/// (`deregister`).  The timer task reads the map each tick; entries added
/// or removed between ticks are picked up on the next wake.
pub struct AggregateRateLimiter {
    // ---
    scheduler: Arc<Mutex<DrrScheduler>>,
    streams: Arc<Mutex<HashMap<Uuid, StreamEntry>>>,
    /// Stored for uncapped detection by callers; `None` = uncapped.
    params: Option<RateParams>,
}

impl AggregateRateLimiter {
    // ---

    /// Construct and start the aggregate rate limiter.
    ///
    /// - `rate_bps = None`: uncapped.  No timer task is spawned.
    ///   [`register`] returns `(None, backlog)` so callers can detect
    ///   uncapped mode and skip pump construction.
    /// - `rate_bps = Some(n)`: spawns the aggregate timer task.
    pub fn new(rate_bps: Option<u64>) -> Self {
        // ---
        use super::CHUNK_SIZE;

        let scheduler = Arc::new(Mutex::new(DrrScheduler::new()));
        let streams: Arc<Mutex<HashMap<Uuid, StreamEntry>>> = Arc::new(Mutex::new(HashMap::new()));

        let params = rate_bps.map(|bps| RateParams::from_rate_bps(bps, CHUNK_SIZE));

        if let Some(p) = params {
            tokio::spawn(
                AggregateTimerTask {
                    scheduler: Arc::clone(&scheduler),
                    streams: Arc::clone(&streams),
                    budget_bytes: p.budget_bytes,
                    interval: p.interval,
                }
                .run(),
            );
        }

        Self {
            scheduler,
            streams,
            params,
        }
    }

    // ---

    /// Register a new stream.
    ///
    /// Returns:
    /// - `alloc_rx`: `Some` in capped mode — the [`StreamPump`] selects on
    ///   this for budget grants from the ARL timer.  `None` in uncapped mode.
    /// - `backlog`: shared atomic the pump writes after each drain.
    pub(crate) async fn register(
        &self,
        uuid: Uuid,
        priority: Priority,
    ) -> (
        Option<mpsc::Receiver<AllocTicket>>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        // ---
        let head_offset = Arc::new(AtomicU64::new(0));
        let q_atomic = Arc::new(AtomicU64::new(0));

        self.scheduler.lock().await.register(uuid, priority);

        if self.params.is_some() {
            // Channel depth 1: the pump always drains before the next tick
            // in the steady state.  Depth 1 lets one ticket queue up if the
            // pump is slow, preventing timer-task stalls on a full channel.
            let (alloc_tx, alloc_rx) = mpsc::channel::<AllocTicket>(1);
            tracing::debug!("ATT:register, locking streams...");
            self.streams.lock().await.insert(
                uuid,
                StreamEntry {
                    alloc_tx,
                    head_offset: Arc::clone(&head_offset),
                    q_atomic: Arc::clone(&q_atomic),
                },
            );
            (Some(alloc_rx), head_offset, q_atomic)
        } else {
            (None, head_offset, q_atomic)
        }
    }

    // ---

    /// Deregister a stream — called when the pump exits (done or failed).
    pub async fn deregister(&self, uuid: Uuid) {
        // ---
        tracing::debug!(%uuid, "ATT:deregister, ...");
        self.scheduler.lock().await.deregister(uuid);
        self.streams.lock().await.remove(&uuid);
    }
}

// ---------------------------------------------------------------------------
// StreamPump
// ---------------------------------------------------------------------------

/// Per-stream write task in capped mode.
///
/// Replaces the old `TimerTask`.  Instead of owning its own timer, the pump
/// waits for [`AllocTicket`]s from the [`AggregateRateLimiter`] and drains
/// up to `ticket.bytes` from the spool per grant.
///
/// Concurrently selects on `cmd_rx` for link lifecycle events.
struct StreamPump {
    // ---
    spool: Arc<Mutex<SpoolBuffer>>,

    /// Q pointer — absolute byte offset of the next byte to send.
    q: u64,
    quic_tx: WriteHalf<QueLayStreamPtr>,
    alloc_rx: mpsc::Receiver<AllocTicket>,
    cmd_rx: mpsc::Receiver<RateCmd>,

    q_atomic: Arc<AtomicU64>,
    done: bool,
    finishing: bool,
}

impl StreamPump {
    // ---
    async fn run(mut self) {
        // ---
        use super::CHUNK_SIZE;

        tracing::debug!("stream pump: task started, entering select loop");

        while !self.done {
            tokio::select! {
                ticket = self.alloc_rx.recv() => {
                    match ticket {
                        Some(t) => {
                            tracing::debug!(bytes = t.bytes, q = self.q, "stream pump: AllocTicket received");
                            self.drain_alloc(t.bytes, CHUNK_SIZE).await;
                        }
                        None => {
                            // ARL dropped the sender (shutdown) — exit cleanly.
                            tracing::debug!("stream pump: alloc channel closed, exiting");
                            return;
                        }
                    }
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(RateCmd::LinkDown) => {

                            self.q = self.spool.lock().await.bytes_acked;
                            self.q_atomic.store(self.q, Ordering::Release);

                            tracing::info!("stream pump: link down — rewound Q, waiting for LinkUp");

                            if !self.wait_for_link_up().await {
                                return;
                            }
                        }
                        Some(RateCmd::LinkUp(_)) => {
                            tracing::debug!(
                                "stream pump: unexpected LinkUp in running state — ignoring"
                            );
                        }
                        Some(RateCmd::Finish) => {
                            tracing::debug!("stream pump: got RateCmd::Finish");
                            self.finishing = true;
                            let remaining = {
                                let s = self.spool.lock().await;
                                s.head_offset().saturating_sub(self.q)
                            };
                            if remaining == 0 {
                                // Pump is fully caught up: no more data to
                                // rate-limit, just need one drain_alloc call
                                // to hit the EOF sentinel and send FIN.
                                // The ARL won't send a ticket (backlog == 0)
                                // so we trigger it directly here.
                                self.drain_alloc(1, CHUNK_SIZE).await;
                            } else {
                                // There is still backlog — the ARL will keep
                                // sending tickets as drain_alloc updates the
                                // atomic.  Refresh it now using self.q so a
                                // stale bytes_acked value from run_tcp_reader
                                // doesn't cause the ARL to under-schedule.
                            }
                        }
                        None => return,
                    }
                }
            }
        }
        tracing::info!("stream pump: exiting");
    }

    // ---

    /// Drain up to `budget` bytes from the spool, sending chunks to QUIC.
    ///
    /// Updates the head/q atomics after draining so the ARL scheduler has
    /// current data for the next tick.
    async fn drain_alloc(&mut self, budget: u64, chunk_size: usize) {
        // ---
        let mut remaining = budget;

        tracing::debug!(budget, q = self.q, "drain_alloc: enter");

        loop {
            if remaining == 0 {
                break;
            }

            let chunk = {
                let s = self.spool.lock().await;
                let head = s.head;

                // EOF sentinel: spool fully drained.
                if head == u64::MAX && s.head_offset() <= self.q {
                    drop(s);
                    if self.finishing {
                        tracing::info!("stream pump: spool drained, sending FIN");
                        let _ = self.quic_tx.shutdown().await;
                        self.done = true;
                    }
                    break;
                }
                if s.head_offset() <= self.q {
                    tracing::trace!(
                        head_offset = s.head_offset(),
                        q = self.q,
                        "drain_alloc: no new data"
                    );
                    break; // nothing new yet
                }

                let slice = s.slice_from(self.q);
                let n = slice.len().min(chunk_size).min(remaining as usize);
                slice[..n].to_vec()
            };

            if chunk.is_empty() {
                break;
            }

            let encoded = encode_chunk(self.q, &chunk);
            tracing::debug!(
                q = self.q,
                n = chunk.len(),
                "drain_alloc: writing chunk to QUIC"
            );
            if let Err(e) = self.quic_tx.write_all(&encoded).await {
                tracing::warn!("stream pump: QUIC write error: {e} — waiting for LinkUp");
                if !self.wait_for_link_up().await {
                    self.done = true;
                }
                return;
            }
            self.q += chunk.len() as u64;
            self.q_atomic.store(self.q, Ordering::Release);
            remaining = remaining.saturating_sub(chunk.len() as u64);
        }

        // Snapshot remaining bytes and EOF sentinel after draining.
        let (remaining_bytes, head_at_end) = {
            let s = self.spool.lock().await;
            (s.head_offset().saturating_sub(self.q), s.head)
        };

        // Post-drain sentinel: the budget may have been exhausted on the last
        // chunk (remaining == 0 at top of loop), causing the loop to exit
        // before the in-loop sentinel could fire.  Check here so we don't
        // leave the pump stuck in select! with backlog == 0 and Finish already
        // processed — the 30-second idle-timeout stall in issue #6.
        if self.finishing && head_at_end == u64::MAX && remaining_bytes == 0 {
            tracing::info!("stream pump: spool drained, sending FIN");
            let _ = self.quic_tx.shutdown().await;
            self.done = true;
        }
    }

    // ---

    /// Block on `cmd_rx` until `LinkUp(new_tx)` arrives.
    ///
    /// Discards any `AllocTicket`s that arrive on `alloc_rx` while paused
    /// (link is down; no data should be sent).
    ///
    /// Returns `true` if link came back up, `false` if cmd channel closed.
    async fn wait_for_link_up(&mut self) -> bool {
        // ---
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(RateCmd::LinkUp(new_tx)) => {
                            self.q = self.spool.lock().await.bytes_acked;
                            self.q_atomic.store(self.q, Ordering::Release);
                            self.quic_tx = new_tx;

                            tracing::debug!(q = self.q,
                                            "stream pump: link up — new write half installed");

                            return true;
                        }
                        Some(RateCmd::LinkDown) => {
                            // Duplicate LinkDown — rewind again (defensive).
                            self.q = self.spool.lock().await.bytes_acked;
                            self.q_atomic.store(self.q, Ordering::Release);

                            tracing::warn!(q = self.q,
                                           "stream pump: duplicate LinkDown while waiting for LinkUp");
                        }
                        Some(RateCmd::Finish) => {
                            tracing::warn!(q = self.q,
                                           "stream pump: got RateCmd::Finish while waiting for LinkUp");
                            self.finishing = true;
                        }
                        None => return false,
                    }
                }

                // Drain stale tickets while link is down.
                _ = self.alloc_rx.recv() => {
                    tracing::trace!("stream pump: discarding AllocTicket while link is down");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Per-stream handle to the rate-limiting infrastructure.
///
/// In **capped** mode a [`StreamPump`] task was spawned at construction;
/// `cmd_tx` drives its lifecycle (LinkDown / LinkUp / Finish).
///
/// In **uncapped** mode no pump is spawned; `direct_tx` is a plain write half
/// used by the caller directly (unchanged from before).
pub struct RateLimiter {
    // ---
    /// Command channel to the [`StreamPump`] (capped mode only).
    cmd_tx: Option<mpsc::Sender<RateCmd>>,

    /// Direct write half for uncapped mode.
    direct_tx: Option<WriteHalf<QueLayStreamPtr>>,
}

impl RateLimiter {
    // ---

    /// Construct a `RateLimiter` for one uplink stream.
    ///
    /// - `alloc_rx = Some(rx)`: capped mode.  Spawns a [`StreamPump`] that
    ///   selects on `rx` (budget grants) and `cmd_rx` (lifecycle events).
    /// - `alloc_rx = None`: uncapped mode.  No pump spawned; caller writes
    ///   directly via the returned `direct_tx` path.
    ///
    /// `backlog` must be the [`Arc<AtomicU64>`] returned by
    /// [`AggregateRateLimiter::register`] for this stream.
    pub fn new(
        quic_tx: WriteHalf<QueLayStreamPtr>,
        alloc_rx: Option<mpsc::Receiver<AllocTicket>>,
        spool: Arc<Mutex<SpoolBuffer>>,
        q_atomic: Arc<AtomicU64>,
    ) -> Self {
        // ---
        match alloc_rx {
            None => Self {
                cmd_tx: None,
                direct_tx: Some(quic_tx),
            },

            Some(alloc_rx) => {
                let (cmd_tx, cmd_rx) = mpsc::channel(8);

                tokio::spawn(
                    StreamPump {
                        spool,
                        q: 0,
                        quic_tx,
                        alloc_rx,
                        cmd_rx,
                        q_atomic,
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

    /// Return a clone of `cmd_tx` so `UplinkHandle` can send `LinkDown`.
    /// Returns `None` in uncapped mode.
    pub fn link_down_tx_clone(&self) -> Option<mpsc::Sender<RateCmd>> {
        self.cmd_tx.clone()
    }

    // ---

    /// Signal the pump that the link is down (non-blocking).
    /// No-op in uncapped mode.
    pub fn link_down(&self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(RateCmd::LinkDown);
        }
    }

    // ---

    /// Hand the pump a fresh QUIC write half after reconnect.
    /// In uncapped mode, replaces `direct_tx` in-place.
    pub async fn link_up(&mut self, new_tx: WriteHalf<QueLayStreamPtr>) -> io::Result<()> {
        // ---
        match (&self.cmd_tx, &mut self.direct_tx) {
            (Some(cmd_tx), None) => cmd_tx
                .send(RateCmd::LinkUp(new_tx))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream pump exited")),
            (None, Some(direct)) => {
                *direct = new_tx;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    // ---

    /// Tell the pump to drain remaining spool data, send FIN, then exit.
    pub async fn finish(&mut self) -> io::Result<()> {
        // ---
        if let Some(cmd_tx) = &self.cmd_tx {
            cmd_tx
                .send(RateCmd::Finish)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream pump exited"))?;
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
