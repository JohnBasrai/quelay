// ---------------------------------------------------------------------------
// Subcommand: max-concurrent
// ---------------------------------------------------------------------------

use clap::Args;

use crate::*;

#[derive(Debug, Args)]
pub struct MaxConcurrentArgs {
    // ---
    /// Maximum concurrent streams to configure on the sender agent (default: 2).
    ///
    /// The test queues `stream_count` streams, expects the first `max_concurrent`
    /// to start immediately (status RUNNING), and the rest to be queued
    /// (status PENDING) in descending priority order.
    #[arg(long, default_value_t = 2)]
    max_concurrent: usize,

    /// Total number of streams to submit (default: 5).
    ///
    /// Must be > max_concurrent so that at least one stream ends up queued.
    #[arg(long, default_value_t = 5)]
    stream_count: usize,
}

/// Test max-concurrent stream enforcement and pending queue priority ordering.
///
/// # Sequence
///
/// 1. Set `--max-concurrent N` on the sender agent via `set_max_concurrent`.
/// 2. Submit `stream_count` streams with distinct priorities in a single
///    burst.  Priority values are spread across the bulk range (0..63) so
///    none of them are strict-priority (≥ 64) and DRR ordering applies.
/// 3. Assert the first `max_concurrent` streams returned `RUNNING`.
/// 4. Assert the remaining streams returned `PENDING` with `queue_position`
///    values 1..=(stream_count - max_concurrent).
/// 5. Assert the pending queue snapshot returned by each `stream_start` is
///    sorted highest-priority-first (descending).
/// 6. Restore `max_concurrent` to its default (0 = unlimited) so subsequent
///    tests are not affected.
///
/// The test does **not** drive actual data; it only verifies the
/// `stream_start` return values.  Completing the queued streams and
/// observing `promote_pending` fire is left to a future extension.
pub async fn cmd_max_concurrent(
    sender_c2i: SocketAddr,
    _receiver_c2i: SocketAddr,
    args: &MaxConcurrentArgs,
) -> anyhow::Result<()> {
    // ---
    println!("=== max-concurrent ===");

    ensure_agent_running(sender_c2i)?;

    anyhow::ensure!(
        args.stream_count > args.max_concurrent,
        "--stream-count ({}) must be > --max-concurrent ({})",
        args.stream_count,
        args.max_concurrent,
    );

    // Configure the agent.
    {
        let mut agent = connect_agent(sender_c2i)?;
        agent
            .set_max_concurrent(args.max_concurrent as i32)
            .context("set_max_concurrent failed")?;
    }

    // Bind a callback server so we can receive QueueStatus updates.
    let cb = TestCallbackServer::bind()?;
    {
        let mut agent = connect_agent(sender_c2i)?;
        let e = agent.set_callback(cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "set_callback: {e}");
    }

    // Build a set of (priority, uuid) pairs.
    //
    // Priorities are evenly spread across 1..=62 (bulk range, below the
    // strict threshold of 64) so that:
    //   - All streams compete in DRR.
    //   - No two streams share a priority, making ordering deterministic.
    //
    // We use stream_count values spread over 1..=62. If stream_count > 62
    // we just wrap — for the default of 5 this gives [12, 24, 37, 49, 61].
    //
    // The values are then interleaved (low, high, mid, ...) so that the
    // pending queue's priority-sorted insertion is actually exercised.
    // Submitting in strictly ascending or descending order is trivial for
    // the insertion sort; interleaving forces it to compare and reorder.
    // Example for 5 streams: generated [12, 24, 37, 49, 61],
    // interleaved → [12, 61, 24, 49, 37].
    let step = 62usize / args.stream_count;
    let generated: Vec<i8> = (1..=args.stream_count)
        .map(|i| (i * step).min(62) as i8)
        .collect();

    // Interleave: take from front and back alternately so submissions
    // arrive out of priority order.
    let mut priorities: Vec<i8> = Vec::with_capacity(generated.len());
    let mut lo = 0usize;
    let mut hi = generated.len().saturating_sub(1);
    let mut take_lo = true;
    while lo <= hi {
        if lo == hi {
            priorities.push(generated[lo]);
            break;
        }
        if take_lo {
            priorities.push(generated[lo]);
            lo += 1;
        } else {
            priorities.push(generated[hi]);
            hi -= 1;
        }
        take_lo = !take_lo;
    }

    // Build expected pending priority order (highest-first) for QueueStatus
    // verification.  These are the priorities of streams that will be PENDING.
    let pending_priorities_desc = {
        let mut p: Vec<i8> = priorities[args.max_concurrent..].to_vec();
        p.sort_by(|a, b| b.cmp(a));
        p
    };

    let uuids: Vec<String> = (0..args.stream_count)
        .map(|_| Uuid::new_v4().to_string())
        .collect();

    println!(
        "  submitting {} streams with priorities {:?}",
        args.stream_count, priorities
    );

    let mut agent = connect_agent(sender_c2i)?;

    let mut results = Vec::new();
    for (i, (uuid, &pri)) in uuids.iter().zip(priorities.iter()).enumerate() {
        let result = agent
            .stream_start(
                uuid.clone(),
                StreamInfo {
                    size_bytes: Some(4096),
                    attrs: Some({
                        let mut m = std::collections::BTreeMap::new();
                        m.insert("label".to_string(), format!("mc-stream-{i}"));
                        m
                    }),
                },
                pri,
            )
            .context(format!("stream_start failed for stream {i}"))?;

        results.push((i, uuid.clone(), pri, result));
    }

    // -----------------------------------------------------------------------
    // Assertions
    // -----------------------------------------------------------------------

    let mut running_count = 0usize;
    let mut pending_positions: Vec<i32> = Vec::new();

    for (i, uuid, pri, result) in &results {
        let status = result
            .status
            .ok_or_else(|| anyhow::anyhow!("stream {i}: stream_start returned no status"))?;

        match status {
            Status::RUNNING => {
                running_count += 1;
                anyhow::ensure!(
                    result.queue_position.is_none() || result.queue_position == Some(0),
                    "stream {i} (priority {pri}): RUNNING but queue_position = {:?}",
                    result.queue_position
                );
                println!("  stream {i} (priority {pri:>3}, uuid {uuid}): RUNNING ✓");
            }
            Status::PENDING => {
                let pos = result
                    .queue_position
                    .ok_or_else(|| anyhow::anyhow!("stream {i}: PENDING but no queue_position"))?;
                anyhow::ensure!(
                    pos >= 1,
                    "stream {i} (priority {pri}): PENDING but queue_position = {pos}"
                );
                pending_positions.push(pos);
                println!(
                    "  stream {i} (priority {pri:>3}, uuid {uuid}): PENDING queue_pos={pos} ✓"
                );
            }
            other => {
                anyhow::bail!(
                    "stream {i} (priority {pri}): expected RUNNING or PENDING, got {other:?}"
                );
            }
        }
    }

    // Exactly max_concurrent streams must be RUNNING.
    anyhow::ensure!(
        running_count == args.max_concurrent,
        "expected {} RUNNING streams, got {}",
        args.max_concurrent,
        running_count,
    );
    println!(
        "  active count = {} / {} ✓",
        running_count, args.max_concurrent
    );

    // The pending queue positions must be 1..=pending_count with no gaps.
    let expected_pending = args.stream_count - args.max_concurrent;
    anyhow::ensure!(
        pending_positions.len() == expected_pending,
        "expected {} PENDING streams, got {}",
        expected_pending,
        pending_positions.len(),
    );

    // Positions must be 1-based and contiguous (1, 2, 3 ...).
    let mut sorted_pos = pending_positions.clone();
    sorted_pos.sort();
    let expected_pos: Vec<i32> = (1..=expected_pending as i32).collect();
    anyhow::ensure!(
        sorted_pos == expected_pos,
        "pending queue positions {sorted_pos:?} ≠ expected {expected_pos:?}"
    );
    println!("  pending positions {sorted_pos:?} ✓");

    // -----------------------------------------------------------------------
    // QueueStatus callback verification
    //
    // The agent fires a QueueStatus callback on every enqueue/dequeue event.
    // Drain all events received since submission and inspect the last one —
    // it should reflect the fully-settled state: max_concurrent active,
    // the pending UUIDs listed in priority-descending order.
    // -----------------------------------------------------------------------
    let qs = cb
        .last_queue_status(Duration::from_millis(200))
        .ok_or_else(|| anyhow::anyhow!("no QueueStatus callback received"))?;

    // active_count is Option<i32> in the wire type.
    anyhow::ensure!(
        qs.active_count == Some(args.max_concurrent as i32),
        "QueueStatus: active_count = {:?}, expected {}",
        qs.active_count,
        args.max_concurrent,
    );

    // pending is Option<Vec<String>> (UUID strings), in priority-desc order.
    let uuid_to_pri: std::collections::HashMap<String, i8> = uuids
        .iter()
        .zip(priorities.iter())
        .map(|(u, &p)| (u.clone(), p))
        .collect();

    let qs_pending_pris: Vec<i8> = qs
        .pending
        .unwrap_or_default()
        .iter()
        .map(|u| {
            *uuid_to_pri
                .get(u)
                .unwrap_or_else(|| panic!("QueueStatus: unknown UUID in pending: {u}"))
        })
        .collect();

    anyhow::ensure!(
        qs_pending_pris == pending_priorities_desc,
        "QueueStatus pending priorities (highest-first) mismatch:\n\
         got:      {qs_pending_pris:?}\n  expected: {pending_priorities_desc:?}",
    );

    println!(
        "  QueueStatus: active={:?}, pending={:?} ✓",
        qs.active_count, qs_pending_pris
    );

    // Restore agent to unlimited concurrency so subsequent tests are clean.
    {
        let mut agent = connect_agent(sender_c2i)?;
        agent.set_max_concurrent(0)?;
    }

    println!("  max-concurrent PASSED ✓");
    println!();
    Ok(())
}
