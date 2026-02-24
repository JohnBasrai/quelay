# `quelay-agent/src/bin/e2e_test` — Integration Test Design

## Overview

`e2e_test` is the primary integration test binary for the Quelay relay daemon.
It replaces the legacy C++ `FTAClientEndToEndTest` binary and the shell scripts
that orchestrated it (`run-all-tests`, `run-end-to-end-test`, `run-link-down-test`,
`run-air-restart-test`), eliminating the fragile script-based orchestration in
favor of a single, self-contained Rust binary with a structured CLI.

The binary lives in `quelay-agent/src/bin/e2e_test.rs` rather than
`quelay-example/` so that it has direct access to agent-internal types, constants
(spool capacity, framing constants, priority bounds), and test-only utilities
without requiring those symbols to be exported through the public API.

A thin shell script `scripts/ci-integration-test.sh` starts/stops agents and
calls `e2e_test` subcommands in sequence. This script is wired into `ci.yml` and
`local-test.sh`.

---

## IDL additions required

Two new methods are needed on `QueLayAgent` before `e2e_test` can be
self-configuring. Add to `quelay-thrift/quelay.thrift`:

```thrift
/// Get the agent's currently configured uplink BW cap in Mbit/s.
/// Returns 0 if uncapped.
u32 get_bandwidth_cap_mbps(),

/// Set the agent's uplink BW cap in Mbit/s.
/// Set to 0 to remove the cap entirely.
/// Used by integration tests to configure agents without restart.
void set_bandwidth_cap_mbps(1: u32 bw_cap_mbps),
```

By querying the agents for their current cap, `e2e_test` derives all timing
parameters internally. The `--bw-cap-mbps` duplication between agent startup and
test invocation is eliminated. If both agents report different caps (asymmetric
uplink/downlink — a realistic satellite scenario), the test uses the sender's cap
for timing and reports both in the transfer summary.

---

## CLI design

```
e2e_test [GLOBAL OPTIONS] <SUBCOMMAND>

GLOBAL OPTIONS:
    --sender-c2i    ADDR    C2I address of the sending agent   [default: 127.0.0.1:9090]
    --receiver-c2i  ADDR    C2I address of the receiving agent [default: 127.0.0.1:9091]
    -N, --concurrent N      Set max concurrent streams on both agents before running the
                            test, then restore the original value after. Leave unset to
                            keep the agents' current configuration.
    --debug                 Run with RUST_LOG=debug

SUBCOMMANDS:
    rate-limiter            Token bucket accuracy test (no agents required)
    multi-file              Multi-file transfer test — the primary workhorse
    drr                     DRR scheduler priority ordering test
    small-file-edge-cases   Boundary-condition file size tests
```

---

## Global option: `-N` / `--concurrent`

Controls the maximum number of concurrent active streams the scheduler allows on
both agents. The test saves the current value via `get_queue_status().max_concurrent`,
sets the requested value, runs the test, then restores the original.

This is expressed as a live C2I command (add `set_max_concurrent(i32)` to the IDL)
rather than a startup flag so agents do not need to be restarted between subcommands
within a single CI run.

The `drr` subcommand always sets `-N 1` internally. Other subcommands leave the
value unchanged unless `--concurrent N` is explicitly passed.

---

## Subcommand: `rate-limiter`

Tests the token bucket rate limiter (`quelay-agent/src/rate_limiter.rs`) in
isolation. No agents or network required — purely in-process.

The test sends a burst of bytes through the rate limiter at the configured cap,
measures the realized throughput, and asserts it falls within ±10% of the
configured rate.

```
rate-limiter
    (no subcommand-specific options — derives cap from --sender-c2i agent query,
     or runs uncapped if agents are not reachable)
```

---

## Subcommand: `multi-file`

The primary workhorse. Covers large files, small files, bidirectional transfers,
link outage recovery, and link failure. All timing is derived from the agent's
configured BW cap (queried via `get_bandwidth_cap_mbps()`).

```
multi-file [OPTIONS]

File size selection (mutually exclusive; if none given, defaults to --count 2 at
a size derived from the BW cap × 10 seconds):
    --large             3 large files with the size progression that exercises
                        the full BW cap over a measurable duration.
                        Default sizes: 30 MiB, 2 MiB, 500 KiB.
    --small             4 boundary-condition file sizes:
                          9 000 B  (8 blocks + fragment)
                          1 024 B  (exact single block, no fragment)
                            512 B  (half block)
                              1 B  (single byte — minimum C2I stream over link)
                        See "Small file rationale" below.
    --size-mb N         Single custom file size in MiB.
    --duration-secs N   Derive file size from BW cap × N seconds.
    --count N           Number of files (default: 2). Ignored when --large or
                        --small is given.

Transfer behavior:
    --bidirectional     Send files in both directions (sender→receiver and
                        receiver→sender). Default: one direction only.

Link outage injection (mutually exclusive):
    --link-outage       Simulate a recoverable link outage mid-transfer.
                        Sequence:
                          1. Start first file transfer.
                          2. After 1 s, disable link on both agents
                             (LinkState → Degraded).
                          3. While link is down, enqueue a second file.
                             Verifies the pending queue is maintained across
                             the reconnect.
                          4. After the configured link-down window, re-enable
                             the link (background thread, not the write path).
                          5. Assert all files complete with SHA-256 match.
                        All durations are derived from the BW cap — no
                        hard-coded sleeps.

    --link-fail         Simulate a permanent link failure.
                        Sequence:
                          1. Start a file transfer.
                          2. After 1 s, disable link on both agents.
                          3. Wait past the link-fail timeout.
                          4. Assert LinkState == Failed on both agents.
                          5. Assert the in-flight file receives a
                             stream_failed callback with code LinkFailed.
                          6. Send a second file; assert it is also rejected.
                        Note: the link-fail test was present but skipped
                        in the legacy C++ scripts (see run-link-down-test,
                        "exit 0" before the fail test). Implement and gate
                        it behind a feature flag or --enable-fail-test until
                        confirmed stable.

Validation (all default on; use negating flag to disable):
    --no-verify-sha256  Skip SHA-256 integrity check on received data.
    --no-report-bw      Skip BW utilization report.
```

### Small file rationale

The four sizes in `--small` are not arbitrary. They were introduced in the
legacy C++ suite specifically because each one exposed a distinct class of
field bug:

- **9 000 B** — exercises 8 full blocks plus a trailing fragment; caught
  off-by-one errors in fragment handling.
- **1 024 B** — exactly one block with no fragment; caught corner cases in
  the "no fragment" code path that was rarely exercised by larger files.
- **512 B** — half a block; caught issues with minimum-chunk logic.
- **1 B** — single byte; the minimum possible C2I stream. Exercises the
  entire framing path at its boundary and is effectively a "can a 1-byte
  payload traverse the QUIC link" regression test.

---

## Subcommand: `drr`

Tests the DRR (Deficit Round Robin) scheduler's priority ordering. This test
requires the agent to have `max_concurrent = 1` so that queued streams are
reordered by priority before they become active. The test sets this automatically
via `-N 1` before running and restores the original value after.

```
drr [OPTIONS]
    --file-count N      Number of priority-varied files to queue behind the
                        anchor file (default: 3 — low/med/high).

    --no-verify-sha256  Skip SHA-256 integrity check.
    --no-report-bw      Skip BW utilization report.
```

Test sequence:

1. Enqueue a large "anchor" file at priority 0 to occupy the single active slot.
2. Immediately enqueue `N` files at varying priorities in non-sorted order
   (e.g. low=10, high=30, med=20).
3. Call `stream_start`; capture the returned `queue_position` and
   `QueueStatus.pending` list from the `queue_status_update` callback.
4. Assert that `pending` is sorted highest→lowest priority.
5. Re-enable normal concurrency; wait for all files to complete.
6. Assert SHA-256 on all received files.

Naming note: `drr` rather than `multi-file --priority` because the test requires
specific agent state (`max_concurrent = 1`) that would be surprising to have
injected silently by a flag on `multi-file`.

---

## Subcommand: `small-file-edge-cases`

Runs the four boundary-condition file sizes from `multi-file --small` as an
independently callable subcommand. This allows the CI script to run them
separately from the bulk multi-file tests and produce a distinct pass/fail signal.

```
small-file-edge-cases [OPTIONS]
    --bidirectional     Test both transfer directions for each size.
    --no-verify-sha256  Skip SHA-256 integrity check.
    --no-report-bw      Skip BW utilization report.
```

This subcommand is effectively an alias for `multi-file --small`, but callable
independently so a regression in framing boundary handling produces a targeted
failure rather than a failure buried inside a larger multi-file run.

---

## Internal structure

### `TestCallbackHandler` vs. `e2e_demo.rs` `CallbackHandler`

`e2e_demo.rs` (in `quelay-example/`) retains its simple `CallbackHandler` —
it is intentionally minimal for onboarding readability.

`e2e_test.rs` uses a richer `TestCallbackHandler` that accumulates:

- Per-transfer SHA-256 digest (streaming, so the full payload is not held in
  memory twice).
- Actual bytes received (for BW calculation).
- Wall-clock timestamps for start and done events.
- `LinkState` change history (for link-outage and link-fail assertions).
- Progress message count (for the BW utilization report).

If the two handlers share more than ~80 lines of infrastructure, extract a
`test_support` module within `quelay-agent/src/` gated behind a `test-support`
Cargo feature. Do not make it part of the default build.

### Timing derivation

All durations are derived from the agents' configured BW cap rather than
hard-coded. The pattern established in `e2e_demo.rs` is the baseline:

```
payload_bytes  = cap_bytes_per_sec × target_duration_secs
drop_after     = 1 MiB                           (spool outage trigger)
link_down_secs = spool_capacity × fill_fraction / cap_bytes_per_sec
post_bytes     = cap_bytes_per_sec × link_down_secs × 1.5
transfer_timeout = (total_bytes / cap_bytes_per_sec × 3.0 + link_down_secs × 2.0).max(60s)
```

The `SPOOL_CAPACITY`, `SPOOL_FILL_FRACTION`, and block-size constants used for
timing are imported directly from the agent crate — this is one of the primary
reasons `e2e_test` lives in `quelay-agent/src/bin/` rather than `quelay-example/`.

---

## CI integration

`scripts/ci-integration-test.sh` replaces `scripts/ci-e2e-test.sh`. Structure:

```bash
# Phase A — high cap (100 Mbit/s): large file throughput + rate limiter
start_agents 100
e2e_test multi-file --large --bidirectional
e2e_test rate-limiter
stop_agents

# Phase B — low cap (10 Mbit/s): reconnect, priority, edge cases
# (lower cap gives the link-outage window enough time to be deterministic)
start_agents 10
e2e_test multi-file --small
e2e_test multi-file --count 2 --link-outage
e2e_test drr
e2e_test small-file-edge-cases
stop_agents
```

`ci.yml` step `End to end test` changes from:
```yaml
run: ./scripts/ci-e2e-test.sh
```
to:
```yaml
run: ./scripts/ci-integration-test.sh
```

`local-test.sh` adds a call to `ci-integration-test.sh` after the existing
smoke test.

---

## Legacy test mapping

The table below maps the legacy C++ test suite to the new subcommands. This is
documentation only — no legacy references appear in `--help` output.

| Legacy test / script            | New subcommand                                    | Notes |
|---------------------------------|---------------------------------------------------|-------|
| `run-end-to-end-test large`     | `multi-file --large --bidirectional`              | C++ also ran ground→air and air→ground |
| `run-end-to-end-test small`     | `multi-file --small` / `small-file-edge-cases`    | See small file rationale above |
| `run-end-to-end-test bad-file`  | Unit tests in `quelay-agent` (`#[test]`)          | Not an integration test |
| `run-end-to-end-test priority`  | `drr`                                             | Requires `-N 1` |
| `run-link-down-test` (long)     | `multi-file --count 2 --link-outage`              | File 1 longer than link-down window |
| `run-link-down-test` (short)    | `multi-file --small --link-outage`                | File 1 shorter than link-down window |
| `run-link-down-test` (fail)     | `multi-file --link-fail`                          | Was skipped in C++ (`exit 0` before it) |
| `run-air-restart-test`          | Not ported (QUIC reconnect covered by link-outage)| C++ tested FTA *process* restart |
| `run-no-callback-test`          | Not ported (disabled in C++ since SZIP2-1352)     | — |
| Phase 1 BW validation           | `multi-file --large`                              | ±10% BW tolerance assertion |
| Phase 2 spool reconnect         | `multi-file --count 2 --link-outage`              | SHA-256 across reconnect |
| Token bucket accuracy           | `rate-limiter`                                    | New; C++ relied on link hardware |
