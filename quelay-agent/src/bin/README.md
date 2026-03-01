# `quelay-agent/src/bin` — Integration Test Binaries

Two integration test binaries live under `quelay-agent/src/bin/`.  Both require
two live `quelay-agent` instances and are driven by `scripts/ci-integration-test.sh`.

---

## Binaries at a glance

| Binary         | Entry point                         | Purpose |
|:---------------|:------------------------------------|:--------|
| `e2e-test`     | `src/bin/e2e-test/main.rs`          | Full data-pump path — file transfer, DRR ordering, link outage, framing edge cases, max-concurrent enforcement |
| `bw-cap-test`  | `src/bin/bw_cap_test/mod.rs`        | Rate-limiter accuracy — N concurrent streams, aggregate BW asserted within ±10 % of configured cap |

---

## `e2e-test`

### Overview

`e2e-test` is the primary integration test binary.  It replaces the legacy C++
`FTAClientEndToEndTest` binary and the shell scripts that orchestrated it
(`run-all-tests`, `run-end-to-end-test`, `run-link-down-test`,
`run-air-restart-test`).

The binary lives in `quelay-agent/src/bin/` so that it has direct access to
agent-internal constants (spool capacity, framing constants, priority bounds)
without requiring those to be re-exported through the public API.

### Source layout

```
quelay-agent/src/bin/e2e-test/
├── main.rs                  CLI, Cli/Command, run_transfer, shared helpers
├── callback.rs              TestCallbackEvent, TestCallbackHandler,
│                            TestCallbackServer (incl. last_queue_status)
├── drr.rs                   cmd_drr, DrrArgs
├── max_concurrent.rs        cmd_max_concurrent, MaxConcurrentArgs
├── multi_file.rs            cmd_multi_file, MultiFileArgs
└── small_file_edge_cases.rs cmd_small_file_edge_cases, SmallFileEdgeCasesArgs
```

### CLI

```
e2e-test [OPTIONS] <SUBCOMMAND>

OPTIONS:
    --sender-c2i    ADDR    C2I address of the sending agent   [default: 127.0.0.1:9090]
    --receiver-c2i  ADDR    C2I address of the receiving agent [default: 127.0.0.1:9091]
    --debug                 Run with RUST_LOG=debug

SUBCOMMANDS:
    multi-file              Multi-file transfer — large, small, link-outage
    drr                     DRR scheduler — anchor transfer + pending queue formation
    small-file-edge-cases   Framing boundary file sizes
    max-concurrent          Max-concurrent stream slot enforcement and queue ordering
```

---

### Subcommand: `multi-file`

Covers large file throughput, small boundary-condition files, bidirectional
transfers, and link outage recovery.  All timing is derived from the agent's
configured BW cap (queried via `get_bandwidth_cap_mbps()`).

```
multi-file [OPTIONS]
    --large             3 large files (30 MiB / 2 MiB / 500 KiB)
    --small             4 boundary-condition sizes (see small-file-edge-cases)
    --count N           Number of files [default: 2]
    --bidirectional     Test both transfer directions
    --link-outage       Inject a recoverable mid-transfer link outage
```

---

### Subcommand: `drr`

Tests that the DRR scheduler correctly holds streams in the pending queue while
the single active slot is occupied, and that the anchor transfer completes with
SHA-256 integrity.

Sets `max_concurrent = 1` on the sender agent for the duration of the test and
restores unlimited concurrency afterwards.

```
drr [OPTIONS]
    --file-count N      Files to queue behind the anchor [default: 3]
```

Sequence:

1. Set `max_concurrent = 1` on the sender agent.
2. Enqueue a large anchor file at priority 0 to occupy the single active slot.
3. Enqueue `N` files at priorities `[low=10, high=30, med=20]` — deliberately
   out of priority order.
4. Assert each `stream_start` returns `PENDING` with `queue_position` equal to
   the queue depth at submission time (1, 2, 3 …).
5. Wait for the anchor to transfer completely; verify SHA-256.
6. Restore `max_concurrent = 0` (unlimited).

**What `drr` does not test** — it verifies that streams enter the pending queue
and that the anchor completes, but it does not assert the order in which pending
streams are subsequently promoted.  Priority-sorted promotion order is verified
by the `max-concurrent` subcommand via `QueueStatus` callback inspection.

**Historical note** — before `max_concurrent` enforcement was implemented,
`drr` would open all streams immediately (no pending queue formed) and the
`PENDING` / `queue_position` assertions would have failed.  The test was written
with those assertions present, implicitly depending on `set_max_concurrent`
working correctly.  Both now work.

---

### Subcommand: `small-file-edge-cases`

Runs four boundary-condition file sizes as a standalone subcommand so that a
framing regression produces a targeted CI failure rather than one buried inside
a larger `multi-file` run.

```
small-file-edge-cases [OPTIONS]
    --bidirectional     Test both directions for each size
```

The test sets chunk size to **1024 B** on both agents before running and
restores the default afterwards (`set_chunk_size_bytes(0)`).  All four sizes
are interpreted relative to that 1024 B chunk size.

| Size | Chunks | Fragment | Exercises |
|-----:|-------:|:--------:|:----------|
| 9000 B | 8 full | 776 B | Off-by-one in multi-chunk + fragment handling |
| 1024 B | 1 full | none | Exact single-chunk, no-fragment corner case |
|  512 B | none | 512 B | Sub-chunk (single fragment) payload |
|    1 B | none | 1 B | Pathological minimum — one byte through the full framing path |

At the agent default chunk size (16 KiB), both 9000 B and 1024 B would be
single-fragment transfers and the multi-chunk path would go untested.  The
1024 B override is what makes 9000 B a genuine 8-chunk + fragment case.

---

### Subcommand: `max-concurrent`

Verifies that the agent enforces the configured max-concurrent stream limit and
that the pending queue is maintained with correct priority ordering.  This is
the definitive test for pending queue sort order — it inspects the `QueueStatus`
callback to assert that pending UUIDs are listed highest-priority-first.

```
max-concurrent [OPTIONS]
    --max-concurrent N  Maximum active streams [default: 2]
    --stream-count N    Total streams to submit (must be > --max-concurrent) [default: 5]
```

Sequence:

1. Set `max_concurrent = N` on the sender agent.
2. Submit `stream_count` streams with priorities in interleaved order
   (low / high / mid / …) so the priority-sorted insertion in `enqueue()` is
   exercised with out-of-order arrivals.
3. Assert the first `N` streams returned `RUNNING` and the remaining returned
   `PENDING` with `queue_position` values `1..=(stream_count − N)`.
4. Drain `QueueStatus` callbacks via `last_queue_status(200 ms)`; assert
   `active_count == N` and the pending UUID list is in priority-descending order.
5. Restore `max_concurrent = 0` (unlimited).

---

### `TestCallbackServer`

`callback.rs` provides a Thrift callback server used by all subcommands.

| Method   | Description |
|:---------|:------------|
| `bind()` | Binds on an ephemeral port; spawns the Thrift server thread |
| `recv_event(timeout)` | Block until the next event |
| `recv_event_for(uuid, timeout)` | Block until an event matching `uuid` arrives; discards others |
| `last_queue_status(timeout)` | Drain events for `timeout`; return the last `QueueStatus` seen |
| `progress_count()` | Number of progress callbacks received |

`QueueStatus` events are forwarded into the channel and used exclusively by
`cmd_max_concurrent` to verify pending queue sort order.

---

## `bw-cap-test`

### Overview

`bw-cap-test` verifies that the agent's `RateLimiter` correctly shapes N
concurrent streams to the configured aggregate bandwidth cap.  Each stream
transmits continuously for `--duration-secs` seconds; aggregate throughput is
then asserted within ±10 % of the cap.

In practice, observed utilization for transfers larger than ~4 KiB is
consistently within ±2 % of the cap.  The ±10 % tolerance exists to
accommodate the start-up and tear-down transients that dominate measurement
error for very small or very short transfers.

### Source layout

```
quelay-agent/src/bin/bw_cap_test/
├── mod.rs      CLI, main, stream orchestration
├── callback.rs CallbackActor — Thrift callback handler for sender/receiver roles
├── cic.rs      Combat Information Center — event dispatch, BW assertion
└── tuner.rs    TunerCmd/TunerOutcome — per-stream sender and receiver tasks
```

### CLI

```
bw-cap-test [OPTIONS]

OPTIONS:
    --sender-c2i    ADDR    Sending agent C2I address   [default: 127.0.0.1:9090]
    --receiver-c2i  ADDR    Receiving agent C2I address [default: 127.0.0.1:9091]
    --count N               Number of concurrent streams [default: 3]
    --duration-secs N       Transmission duration per stream [default: 5]
    --debug                 Run with RUST_LOG=debug
```

### Design

Each stream payload is sized so the writer remains mid-stream when the duration
timer fires (`cap_bytes_per_sec × duration × PAYLOAD_HEADROOM`), ensuring the
rate limiter is continuously under load for the full measurement window.  After
all tuners exit, `assert_aggregate_bw` computes total bytes / wall-clock time
and checks it against the cap within the tolerance.

The BW cap is queried from the sender agent at startup (`get_bandwidth_cap_mbps`).

### Observed performance

Results from a representative `bw-cap-test` run: 3 concurrent streams at a
10 Mbit/s cap, 5 s duration.

```
┌── per-stream results ───────────────┐
│  Receiver    5 537 024 B   13.16s ✓ │
│  Receiver    5 537 024 B   13.16s ✓ │
│  Receiver    5 537 024 B   13.16s ✓ │
│  Sender      5 492 736 B   13.16s ✓ │
│  Sender      5 488 640 B   13.16s ✓ │
│  Sender      5 491 928 B   13.16s ✓ │
└─────────────────────────────────────┘

  Aggregate BW: 1251.4 KB/s  cap: 1250.0 KB/s  (100.1%)
  Aggregate BW within ±10% ✓
```

In practice, observed utilization for transfers larger than ~4 KiB is
consistently within ±2 % of the cap.  The ±10 % tolerance exists to
accommodate the start-up and tear-down transients that dominate measurement
error for very small or very short transfers.

Each sender writes for 5 s at full speed into the spool, then stops. The three
streams share the 10 Mbit/s cap, so each stream's ~5.5 MiB drains at roughly 1/3 of
1250 KB/s ≈ 417 KB/s. Three streams × 5.5 MiB ÷ 1250 KB/s ≈ 13.2 s total drain time,
which matches the observed 13.16 s. The aggregate check — 16.6 MiB ÷ 13.16 s = 1251
KB/s — is what the ±10 % assertion measures against the 1250 KB/s cap.

---

## Scheduler unit tests (`quelay-domain`)

The DRR scheduler unit tests live in `quelay-domain/src/scheduler.rs`.  Six
unit tests cover the core scheduling invariants:

- `c2i_drains_before_bulk`
- `bulk_streams_share_budget`
- `idle_stream_does_not_accumulate_deficit`
- `deregister_removes_stream`
- `schedule_never_exceeds_budget`
- `c2i_does_not_starve_when_bulk_present`

End-to-end scheduler coverage is provided by `bw-cap-test` (aggregate
throughput under concurrent load), `e2e-test drr` (pending queue formation),
and `e2e-test max-concurrent` (priority-sorted promotion order).

---

## CI integration

`scripts/ci-integration-test.sh` orchestrates both binaries:

```bash
# Rate limiter accuracy
start_agents 10
bw-cap-test --count 3 --duration-secs 5
stop_agents

# Full data-pump path
start_agents 10
e2e-test multi-file --large --bidirectional
e2e-test multi-file --small
e2e-test multi-file --count 2 --link-outage
e2e-test drr
e2e-test small-file-edge-cases
e2e-test max-concurrent
stop_agents
```

---

## Legacy test mapping

| Legacy test / script            | New subcommand / binary                        |
|---------------------------------|------------------------------------------------|
| `run-end-to-end-test large`     | `e2e-test multi-file --large --bidirectional`  |
| `run-end-to-end-test small`     | `e2e-test small-file-edge-cases`               |
| `run-end-to-end-test priority`  | `e2e-test drr`                                 |
| `run-link-down-test`            | `e2e-test multi-file --link-outage`            |
| Rate limiter accuracy           | `bw-cap-test`                                  |
| `run-air-restart-test`          | Not applicable — legacy test relies on the file itself as backing store for full replay from byte 0. Quelay operates on streams; replay is bounded by the spool buffer. |
