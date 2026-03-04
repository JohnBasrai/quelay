# Testing Strategy

## Layers

### Unit tests (`#[cfg(test)]` in source files)

Test individual types in isolation. Live in the same file as the code
they test.

```bash
cargo test -p quelay-domain
cargo test -p quelay-agent
```

### Workspace unit tests

```bash
cargo test --workspace
```

### Integration tests (`e2e_test` binary)

End-to-end tests using two live `quelay-agent` processes communicating over
loopback QUIC. Covers rate-limiter accuracy, large/small file transfers,
link outage + reconnect, DRR priority ordering, and framing edge cases.
SHA-256 verification on all received files; throughput reported against
configured BW cap.

```bash
./scripts/ci-integration-test.sh   # full suite (two BW configurations)
```

Or run individual subcommands against already-running agents:

```bash
e2e_test multi-file --large --bidirectional
e2e_test multi-file --count 2 --link-outage
e2e_test drr
e2e_test small-file-edge-cases
```

### Smoke test

Two real `quelay-agent` instances communicating over loopback QUIC.
Validates end-to-end handshake and C2I reachability in ~1 second.

```bash
./scripts/ci-smoke-test.sh
```

### Network impairment tests (manual)

`scripts/link-sim-test.sh` runs the link simulation suite using Docker Compose.
Two `quelay-agent` containers communicate over an isolated `quic-net` bridge;
[Pumba](https://github.com/alexei-led/pumba) applies network impairment on that
bridge. No host kernel namespaces, `veth` pairs, or `sudo` required.

```bash
# 5% packet loss, 10 MiB payload
./scripts/link-sim-test.sh loss --size-mb 10

# 600ms delay ±100ms jitter
./scripts/link-sim-test.sh delay --size-mb 10

# Loss + delay combined
./scripts/link-sim-test.sh both --size-mb 10

# 2mbit link cap (agent cap must be ≤ link cap)
./scripts/link-sim-test.sh rate --rate 2mbit --bw-cap 1Mbps --size-mb 2

# Baseline — no impairment
./scripts/link-sim-test.sh clean --size-mb 10
```

Requires Docker with Compose v2. Run `./scripts/link-sim-test.sh --help` for
the full option reference.

## Before Submitting a PR

Run the full local CI check to match what `ci.yml` will execute on the PR:

```bash
./scripts/local-test.sh
```

This runs `cargo fmt --check`, `cargo clippy`, `cargo test --workspace`,
the smoke test (`ci-smoke-test.sh`), and the full integration test suite
(`ci-integration-test.sh`) in order, stopping on the first failure. A PR
should only be pushed once this passes cleanly.

You can also run the integration suite independently (e.g. while iterating
on a specific area):

```bash
./scripts/ci-integration-test.sh
```

## What to Test

- New scheduler behaviour: add a `#[test]` in `quelay-domain/src/scheduler.rs`
- New rate-limiter behaviour: add a `#[test]` in `quelay-agent/src/rate_limiter.rs`
- Reconnection / spool logic: add an `e2e_test` subcommand or extend `multi-file`
- New public API: add doc-test examples in `///` comments

## Current Test Inventory

| #  | Test | Crate | Status |
|----|---|---|---|
| 1  | `c2i_drains_before_bulk`                  | `quelay-domain` | ✅ passing |
| 2  | `bulk_streams_share_budget`               | `quelay-domain` | ✅ passing |
| 3  | `idle_stream_does_not_accumulate_deficit` | `quelay-domain` | ✅ passing |
| 4  | `deregister_removes_stream`               | `quelay-domain` | ✅ passing |
| 5  | `schedule_never_exceeds_budget`           | `quelay-domain` | ✅ passing |
| 6  | `c2i_does_not_starve_when_bulk_present`   | `quelay-domain` | ✅ passing |
| 7  | `rate_params_*` (5 unit tests)            | `quelay-agent`  | ✅ passing |
| 8  | `e2e_test multi-file --large`             | `quelay-agent/bin` | ✅ passing |
| 9  | `e2e_test multi-file --count 2 --link-outage` | `quelay-agent/bin` | ✅ passing |
| 10 | `e2e_test drr`                            | `quelay-agent/bin` | ✅ passing |
| 11 | `e2e_test small-file-edge-cases`          | `quelay-agent/bin` | ✅ passing |
| 12 | `e2e_test max-concurrent`                 | `quelay-agent/bin` | ✅ passing |

`e2e_test` lives in `quelay-agent/src/bin/e2e_test/`. Run it via the CI
script (which handles agent lifecycle) or point it at already-running agents.
See `quelay-agent/src/bin/README.md` for the full subcommand reference and
legacy test mapping.
