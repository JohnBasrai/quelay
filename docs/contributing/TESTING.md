# Testing Strategy

## Layers

### Unit tests (`#[cfg(test)]` in source files)

Test individual types in isolation. Live in the same file as the code
they test. Use `MockTransport::connected_pair()` for anything that needs
a transport.

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

### System tests (external, containerised)

Two real Quelay instances communicating over a network interface impaired
with `tc netem`. No code changes needed — `quelay-quic` and `quelay-link-sim`
both implement `QueLayTransport`.

```bash
tc qdisc add dev eth0 root netem loss 5% delay 200ms 50ms
```

## Link Simulation Presets

| Preset | Conditions |
|---|---|
| `LinkSimConfig::perfect()` | No impairments — baseline |
| `LinkSimConfig::degraded()` | 5 % loss, 1 % dup, 512 kbit/s |
| `LinkSimConfig::outage_60s()` | Link goes dark for 60 s — tests spool / resume |

Use `seed` for reproducible drop / dup sequences in CI.

## What to Test

- New scheduler behaviour: add a `#[test]` in `quelay-domain/src/scheduler.rs`
- New transport behaviour: add an integration test using `MockTransport`
- Reconnection / spool logic: use `MockSession::simulate_outage()`
- New public API: add doc-test examples in `///` comments
