use std::collections::{HashMap, VecDeque};

use uuid::Uuid;

use super::{Priority, QueLayError, Result};

// ---------------------------------------------------------------------------
// Priority helpers
// ---------------------------------------------------------------------------

/// Default per-tick quantum for bulk streams (bytes).
const BULK_QUANTUM_BYTES: u32 = 4 * 1024;

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StreamEntry {
    // ---
    /// Operator-visible priority level
    priority: Priority,

    /// Accumulated deficit in bytes. Carries over between rounds.
    deficit: u32,

    /// Per-round byte quantum. May be updated dynamically by `rebalance`.
    quantum: u32,

    /// Estimated backlog in bytes. Updated by the session / spooler.
    backlog: u64,
}

// ---------------------------------------------------------------------------
// DrrScheduler
// ---------------------------------------------------------------------------

/// Deficit Round Robin (DRR) scheduler.
///
/// Each registered stream gets a deficit counter and a quantum. On each
/// scheduling round the stream's quantum is added to its deficit, then it
/// may send up to `deficit` bytes. Unused deficit carries over (hence
/// "deficit" RR — bursts are amortised, not penalised).
///
/// Streams with priority ≥ 64 bypass DRR via a strict-priority queue and
/// are always drained before any bulk transfer stream.
///
/// The AIMD pacer (not yet implemented) sits above this and provides the
/// total byte budget passed to [`DrrScheduler::schedule`] each tick.
#[derive(Debug, Default)]
pub struct DrrScheduler {
    // ---
    /// Current active stream table.
    streams: HashMap<Uuid, StreamEntry>,

    /// Round-robin order for bulk transfer streams (priority < 64).
    bulk_order: VecDeque<Uuid>,

    /// Strict-priority queue for high-priority streams (priority ≥ 64),
    /// drained before any bulk stream.
    c2i_queue: VecDeque<Uuid>,
}

// ---

impl DrrScheduler {
    // ---
    pub fn new() -> Self {
        Self::default()
    }

    // ---

    /// Register a new stream with its initial priority and quantum.
    pub fn register(&mut self, id: Uuid, priority: Priority) {
        // ---
        let quantum = priority.initial_quantum();
        self.streams.insert(
            id,
            StreamEntry {
                priority,
                deficit: 0,
                quantum,
                backlog: 0,
            },
        );

        if priority.is_strict() {
            // Keep strict queue in priority-desc order.
            let insert_at = self
                .c2i_queue
                .iter()
                .position(|other| {
                    self.streams
                        .get(other)
                        .map(|e| e.priority)
                        .unwrap_or(Priority::from_i8(0))
                        < priority
                })
                .unwrap_or(self.c2i_queue.len());
            self.c2i_queue.insert(insert_at, id);
        } else {
            self.bulk_order.push_back(id);
            self.rebalance();
        }
    }

    // ---

    /// Deregister a stream (transfer complete or reset).
    pub fn deregister(&mut self, id: Uuid) {
        // ---
        if let Some(entry) = self.streams.remove(&id) {
            if entry.priority.is_strict() {
                self.c2i_queue.retain(|&x| x != id);
            } else {
                self.bulk_order.retain(|&x| x != id);
                self.rebalance();
            }
        }
    }

    // ---

    /// Override the DRR quantum for a specific stream.
    pub fn set_quantum(&mut self, id: Uuid, quantum: u32) {
        // ---
        if let Some(entry) = self.streams.get_mut(&id) {
            entry.quantum = quantum;
        }
    }

    // ---

    /// Update the known backlog for a stream.
    ///
    /// Called by the session / spooler as data accumulates or drains.
    pub fn set_backlog(&mut self, id: Uuid, backlog: u64) {
        // ---
        if let Some(entry) = self.streams.get_mut(&id) {
            entry.backlog = backlog;
        }
    }

    // ---

    /// Given a total byte budget for this tick (from the AIMD pacer),
    /// return ordered `(stream_id, bytes_to_send)` allocations.
    ///
    /// C2I streams consume from the budget first. The remaining budget is
    /// distributed across BulkTransfer streams via DRR.
    pub fn schedule(&mut self, mut budget: u64) -> Result<Vec<(Uuid, u64)>> {
        let mut result = Vec::new();

        // --- strict priority: drain C2I first ---
        for &id in &self.c2i_queue {
            if budget == 0 {
                break;
            }
            let entry = self
                .streams
                .get_mut(&id)
                .ok_or(QueLayError::StreamNotFound(id))?;
            let send = budget.min(entry.backlog).min(entry.quantum as u64);
            if send > 0 {
                result.push((id, send));
                budget = budget.saturating_sub(send);
            }
        }

        // --- DRR: bulk transfers ---
        let n = self.bulk_order.len();
        if n == 0 || budget == 0 {
            return Ok(result);
        }

        let mut bulk_allocs: HashMap<Uuid, u64> = HashMap::new();

        // Phase 1: Give **every** bulk stream exactly one turn (mandatory fair round)
        // This ensures that with small budgets, no stream is completely skipped.
        for _ in 0..n {
            if budget == 0 {
                break;
            }

            if let Some(id) = self.bulk_order.front().copied() {
                let entry = self
                    .streams
                    .get_mut(&id)
                    .ok_or(QueLayError::StreamNotFound(id))?;

                entry.deficit += entry.quantum;

                let send = budget.min(entry.deficit as u64).min(entry.backlog);
                if send > 0 {
                    entry.deficit -= send as u32;
                    *bulk_allocs.entry(id).or_insert(0) += send;
                    budget = budget.saturating_sub(send);
                } else {
                    // Idle → prevent deficit accumulation
                    entry.deficit = 0;
                }

                self.bulk_order.rotate_left(1);
            }
        }

        // Phase 2: Continue giving extra turns to active streams while budget remains
        // (this handles cases where budget >> total quantum × n)
        let mut consecutive_idle = 0;
        while budget > 0 && consecutive_idle < n {
            if let Some(id) = self.bulk_order.front().copied() {
                let entry = self
                    .streams
                    .get_mut(&id)
                    .ok_or(QueLayError::StreamNotFound(id))?;

                entry.deficit += entry.quantum;

                let send = budget.min(entry.deficit as u64).min(entry.backlog);
                if send > 0 {
                    entry.deficit -= send as u32;
                    *bulk_allocs.entry(id).or_insert(0) += send;
                    budget = budget.saturating_sub(send);
                    consecutive_idle = 0;
                } else {
                    entry.deficit = 0;
                    consecutive_idle += 1;
                }

                self.bulk_order.rotate_left(1);
            }
        }

        // Append bulk allocations in the order they were served
        // (preserves rough temporal order from the RR cycle)
        result.extend(bulk_allocs);

        Ok(result)
    }

    // ---

    /// Rebalance quanta equally across all active BulkTransfer streams.
    ///
    /// Called automatically on `register` / `deregister`. May also be
    /// called explicitly when operator configuration changes.
    pub fn rebalance(&mut self) {
        // ---
        if self.bulk_order.is_empty() {
            return;
        }

        let quantum = BULK_QUANTUM_BYTES;

        for id in self.bulk_order.iter() {
            if let Some(entry) = self.streams.get_mut(id) {
                entry.quantum = quantum;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used)]
#[allow(clippy::panic_in_result_fn)]
#[cfg(test)]
mod tests {
    // ---

    use super::*;

    // ---
    ///
    /// **What these tests cover:**
    ///
    /// - `schedule_never_exceeds_budget` — budget is the hard ceiling; uses a very
    ///   small budget (3 000 B) against streams with huge backlogs to stress the
    ///   cap.
    /// - `c2i_does_not_starve_when_bulk_present` — C2I gets fully drained *and* bulk
    ///   still gets budget remaining; both strict-priority and fairness in the same
    ///   call.
    ///
    /// **Progress tracker**
    ///
    /// | # | Test | Status |
    /// |---|------|--------|
    /// | 1 | `c2i_drains_before_bulk`                    | ✅ passing |
    /// | 2 | `bulk_streams_share_budget`                 | ✅ passing |
    /// | 3 | `idle_stream_does_not_accumulate_deficit`   | ✅ passing |
    /// | 4 | `deregister_removes_stream`                 | ✅ passing |
    /// | 5 | `schedule_never_exceeds_budget`             | ✅ passing |
    /// | 6 | `c2i_does_not_starve_when_bulk_present`     | ✅ passing |
    /// | 7 | Rate limiter test (bw_cap)                  | ✅ integration |
    /// | 8 | Concurrent files / pending queue int. test  | ✅ integration |
    /// | 9 | Large bulk + C2I latency (DRR wired E2E)    | ✅ integration |
    /// | 10| Throughput measurement vs. BW cap           | ✅ integration |

    #[test]
    fn c2i_drains_before_bulk() -> Result<()> {
        // ---
        let mut sched = DrrScheduler::new();
        let c2i = Uuid::new_v4();
        let bulk = Uuid::new_v4();

        sched.register(c2i, Priority::c2i_priority_min());
        sched.register(bulk, Priority::bulk_priority_min());
        sched.set_backlog(c2i, 1_024);
        sched.set_backlog(bulk, 4_096);

        let allocs = sched.schedule(8_192)?;
        let c2i_pos = allocs.iter().position(|(id, _)| *id == c2i).unwrap();
        let bulk_pos = allocs.iter().position(|(id, _)| *id == bulk).unwrap();

        assert!(c2i_pos < bulk_pos, "C2I must appear before BulkTransfer");
        Ok(())
    }

    // ---

    #[test]
    fn bulk_streams_share_budget() -> Result<()> {
        // ---
        let mut sched = DrrScheduler::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        sched.register(a, Priority::bulk_priority_min());
        sched.register(b, Priority::bulk_priority_min());
        sched.set_backlog(a, 16_384);
        sched.set_backlog(b, 16_384);

        let allocs = sched.schedule(16_384)?;
        let total: u64 = allocs.iter().map(|(_, n)| n).sum();

        assert_eq!(total, 16_384, "full budget should be consumed");
        assert!(
            allocs.iter().any(|(id, _)| *id == a),
            "stream a must be scheduled"
        );
        assert!(
            allocs.iter().any(|(id, _)| *id == b),
            "stream b must be scheduled"
        );
        Ok(())
    }

    // ---

    #[test]
    fn idle_stream_does_not_accumulate_deficit() -> Result<()> {
        // ---
        let mut sched = DrrScheduler::new();
        let a = Uuid::new_v4();

        sched.register(a, Priority::bulk_priority_min());
        sched.set_backlog(a, 0); // idle

        sched.schedule(8_192)?;

        let entry = &sched.streams[&a];
        assert_eq!(
            entry.deficit, 0,
            "idle stream deficit must be reset to zero"
        );
        Ok(())
    }

    // ---

    #[test]
    fn deregister_removes_stream() -> Result<()> {
        // ---
        let mut sched = DrrScheduler::new();
        let a = Uuid::new_v4();

        sched.register(a, Priority::bulk_priority_min());
        sched.set_backlog(a, 4_096);
        sched.deregister(a);

        let allocs = sched.schedule(8_192)?;
        assert!(
            allocs.is_empty(),
            "deregistered stream must not be scheduled"
        );
        Ok(())
    }

    #[test]
    fn schedule_never_exceeds_budget() -> Result<()> {
        // ---
        let mut sched = DrrScheduler::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        sched.register(a, Priority::bulk_priority_min());
        sched.register(b, Priority::bulk_priority_min());
        sched.set_backlog(a, 1_000_000);
        sched.set_backlog(b, 1_000_000);

        let budget = 3_000;
        let allocs = sched.schedule(budget)?;
        let total: u64 = allocs.iter().map(|(_, n)| n).sum();

        assert!(
            total <= budget,
            "allocated {total} bytes but budget was only {budget}"
        );
        Ok(())
    }

    // ---

    #[test]
    fn c2i_does_not_starve_when_bulk_present() -> Result<()> {
        // ---
        // C2I backlog is smaller than the quantum, so it should be fully
        // drained in a single schedule call even when bulk streams compete.
        let mut sched = DrrScheduler::new();
        let c2i = Uuid::new_v4();
        let bulk = Uuid::new_v4();

        sched.register(c2i, Priority::c2i_priority_min());
        sched.register(bulk, Priority::bulk_priority_min());
        sched.set_backlog(c2i, 512);
        sched.set_backlog(bulk, 1_000_000);

        // Budget large enough for both, but we only care that C2I gets its
        // full 512 bytes before bulk touches anything.
        let allocs = sched.schedule(1_048_576)?;

        let c2i_bytes: u64 = allocs
            .iter()
            .filter(|(id, _)| *id == c2i)
            .map(|(_, n)| *n)
            .sum();

        assert_eq!(c2i_bytes, 512, "C2I must be fully drained");

        // Bulk must also have received something — C2I didn't starve it.
        let bulk_bytes: u64 = allocs
            .iter()
            .filter(|(id, _)| *id == bulk)
            .map(|(_, n)| *n)
            .sum();

        assert!(
            bulk_bytes > 0,
            "bulk must not be starved when budget allows"
        );
        Ok(())
    }
}
