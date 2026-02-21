# Testing Strategy

## Layers

### Unit tests (`#[cfg(test)]` in source files)

Test individual types in isolation. Live in the same file as the code
they test.

```bash
cargo test -p quelay-domain
cargo test -p quelay-link-sim
```

### Integration tests (`tests/` directories)

Test interactions between multiple crates. Use `quelay-link-sim` with various
`LinkSimConfig` presets to simulate degraded link conditions.

```bash
cargo test --workspace
```

### Smoke test

Two real `quelay-agent` instances communicating over loopback QUIC.
Validates end-to-end handshake and C2I reachability in ~1 second.

```bash
./scripts/ci-smoke-test.sh
```

### System tests (external, containerised)

Two real Quelay instances communicating over a network interface impaired
with `tc netem`. No code changes needed — `quelay-quic` and `quelay-link-sim`
both implement `QueLayTransport`.

```bash
tc qdisc add dev eth0 root netem loss 5% delay 200ms 50ms
```

## Link Simulation Presets

| Preset | Conditions |
|--------|------------|
| `LinkSimConfig::perfect()`    | No impairments — baseline |
| `LinkSimConfig::degraded()`   | 5 % loss, 1 % dup, 512 kbit/s |
| `LinkSimConfig::outage_60s()` | Link goes dark for 60 s — tests spool / resume |

Use `seed` for reproducible drop / dup sequences in CI.

## What to Test

- New scheduler behaviour: add a `#[test]` in `quelay-domain/src/scheduler.rs`
- New transport behaviour: add a unit test using `LinkSimTransport::connected_pair()`
- Reconnection / spool logic: use `LinkSimSession::simulate_outage()`
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
| 7a | `token_bucket_caps_throughput`            | `quelay-link-sim` | ✅ passing |
| 7b | `mock_stream_bw_cap_enforced`             | `quelay-link-sim` | ✅ passing |
| 8  | Concurrent files/ pending queue int. test | — | 🔒 blocked — data pump stub |
| 9  | Large bulk + C2I latency (DRR wired E2E)  | — | 🔒 blocked — data pump stub |
| 10 | Throughput measurement vs. BW cap         | — | 🔒 blocked — data pump stub |
