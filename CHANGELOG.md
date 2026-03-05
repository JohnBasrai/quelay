# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- E2E test reports packet loss percentage and congestion event count per transfer
- `ConnStats` struct in `quelay-domain` with `sent_packets`, `lost_packets`,
  `congestion_events` fields and `conn_stats()` method on `QueLaySession` trait
- Quinn QUIC path stats wired into `quelay-quic` session implementation
- Thrift IDL `ConnStats` struct and `get_conn_stats()` RPC method in `quelay-thrift`
- `AgentCmd::GetConnStats` / `SessionCommand::GetConnStats` dispatch chain

### Fixed
- `link-sim-test.sh`: `--rate` flag now errors if used with non-`rate` profiles
  (Pumba does not support chaining `rate` with other netem subcommands)

---

## [0.2.0] - 2026-03-04

### Breaking Changes
- CLI: `--bw-cap-mbps` replaced by `--bw-cap-bps` accepting human-readable
  units (e.g. `10Mbps`, `500Kbps`, `1.5Gbps`)
- IDL: `get_bandwidth_cap_mbps` (i32) renamed to `get_bandwidth_cap_bps` (i64)

### Added
- `test-hooks` Cargo feature gate: `link_enable`, `set_max_concurrent`, and
  `set_chunk_size_bytes` RPC handlers are now compiled out unless
  `--features test-hooks` is passed. Production builds are clean by default.
- `scripts/ci-integration-test.sh`: passes `--features test-hooks` when
  building `quelay-agent` and `e2e-test`.
- `quelay-example` gains `--healthcheck` mode for Docker health probes.

### Changed
- Agents now run in Docker containers; link simulation uses Pumba on the
  quic-net bridge, replacing the veth/tc netem approach.
- `AggregateRateLimiter` deducts actual wire bytes (including QUIC retransmits)
  from the rate budget each tick via `wire_bytes_sent()` on `QueLaySession`.
- `TestCallbackServer::bind` now takes an `advertise_ip` for cross-container
  callback reachability.
- Progress callbacks print live percent-done (or byte count) to stdout,
  gated to ~1 update/second.
- BW tolerance check now also requires a minimum elapsed time (500ms) in
  addition to minimum transfer size.
- Agent handles SIGTERM for clean container shutdown.
- Routed `LinkEnable` through `SessionCommand` channel, removing
  `SessionManagerHandle`.

---

## [0.1.3] - 2026-02-28

### Changed
- Refactored split up `e2e_test.rs` from a single 1400-line file

### Fixed
- `set_max_concurrent` C2I call was a no-op in `SessionManager`
- `AckTask` did not notify `SessionManager` on stream completion
- `enqueue()` returned the stream's sorted insertion rank

### Added
- Added full integration test for max-concurrent agent option

---

## [0.1.2] - 2026-02-26

### Changed
- Eliminated the QUIC pump task — `RateLimiter` timer task now reads directly
  from `SpoolBuffer`, removing one task and one mpsc queue per active uplink
  stream (issue #7)
- `encode_chunk` moved from `active_stream` into `rate_limiter`
- `run_ack_task` refactored into `AckTask` struct with per-responsibility methods
- Rate limiting now applies to aggregate BW across all streams instead of
  per-stream (issue #6)
- `ci-integration-test.sh` refactored to source `.common`, eliminating ~60
  lines of duplicated agent lifecycle code
- `ci-bw-cap-test.sh` added to CI workflow

### Fixed
- `wait_space_or_eof` busy-loop eliminated — replaced `tcp.peek()`-based
  backpressure spin with `space_ready.notified()` sleep; reduces agent CPU
  from ~300% to ~12% during `bw-cap-test` (issue #8 or whatever you assign)
- Hardened production code: removed `unwrap()` calls in non-test paths

---

## [0.1.1] - 2026-02-24

### Removed
- `e2e_test rate-limiter` subcommand — rate limiter accuracy is validated
  end-to-end by the ±10% BW utilization assertion in `multi-file --large`

---

## [0.1.0] - 2026-02-24

### Added
- `quelay-domain`: transport traits (`QueLayStream`, `QueLaySession`, `QueLayTransport`)
- `quelay-domain`: `Priority` enum (`C2I`, `BulkTransfer`)
- `quelay-domain`: `LinkState` enum (`Connecting`, `Normal`, `Degraded`, `Failed`)
- `quelay-domain`: `DrrScheduler` — Deficit Round Robin with strict C2I priority
- `quelay-domain`: `QueLayHandler` callback trait with default no-op implementations
- `quelay-domain`: `StreamMeta`, `TransferProgress`, `Direction` types
- `quelay-quic`: QUIC transport via `quinn` — TLS, self-signed cert generation and pinning
- `quelay-thrift`: Thrift C2I and callback service — IDL, generated stubs, wire↔domain mapping
- `quelay-agent`: relay daemon with Thrift C2I, `SessionManager`, exponential backoff
  reconnection (1 s → 30 s cap), 6-byte wire framing + JSON stream metadata
- `quelay-agent`: `ci-smoke-test.sh` — two-agent QUIC handshake smoke test (~1 s)
- GitHub Actions CI: fmt check, clippy `-D warnings`, unit tests, smoke test on PR to main

### Added (data pump + integration test suite)
- `quelay-agent`: full uplink/downlink data pump (`active_stream.rs`) with `SpoolBuffer`
  — three-pointer spool (`A`/`Q`/`T`) enabling lossless resume across link outages
- `quelay-agent`: `RateLimiter` — timer-task-based bandwidth cap; interval clamped to
  5–100 ms, targeting 8 chunks per tick; survives link outages via `link_down()` /
  `link_up(new_write_half)` without reconstruction
- `quelay-agent`: bidirectional reconnect — `SessionManager` now handles both uplink and
  downlink reconnect; `accept_loop` dispatches on 8-byte stream-open header
  (`OP_NEW_STREAM` / `OP_RECONNECT`); downlink pump uses `bytes_written` for duplicate
  detection and gap detection on replay
- `quelay-agent`: 8-byte stream-open header + 10-byte chunk header wire protocol; version
  field for forward-compatible rejection; JSON payload for `StreamHeader` /
  `ReconnectHeader`
- `quelay-agent`: dedicated ack-reader task (`WormholeMsg`) to prevent deadlock under QUIC
  flow-control backpressure; `mpsc` channel decouples ack processing from the write pump
- `quelay-agent`: `pending` queue in `SessionManager` — streams enqueued while the link is
  down are re-issued in arrival order after reconnect via `drain_pending`
- `quelay-agent`: `UplinkHandle` / `DownlinkHandle` — typed handles stored in
  `active_uplinks` / `active_downlinks`; `restore_active` prunes completed handles on
  reconnect
- `quelay-agent`: `link_enable(bool)` on `SessionManagerHandle` — used by integration tests
  and the `link_enable` Thrift C2I method to inject link-down/up events without restarting
  agents
- `quelay-agent/src/bin/e2e_test.rs`: integration test binary replacing legacy C++
  `FTAClientEndToEndTest` and shell-script orchestration; four subcommands: `rate-limiter`,
  `multi-file`, `drr`, `small-file-edge-cases`; SHA-256 per-transfer verification;
  throughput reporting; all timing derived from agent-reported BW cap
- `scripts/ci-integration-test.sh`: CI integration test script orchestrating `e2e_test`
  across two BW configurations (100 Mbit/s and 10 Mbit/s); replaces `ci-e2e-test.sh`
- `quelay-agent/src/bin/README.md`: full `e2e_test` design doc — CLI reference, subcommand
  rationale, legacy test mapping, timing derivation, CI integration guide

### Changed
- `quelay-agent`: `SessionManager::run` spawns a sibling `accept_loop` task; `session_restored`
  `Notify` re-arms the accept loop after each successful reconnect
- `quelay-agent`: wire framing updated from 6-byte to 8-byte stream-open header; chunk
  header extended from 6-byte to 10-byte (adds 8-byte `stream_offset` for spool replay)
- `quelay-agent`: `TransportConfig` enum replaces the earlier transport argument threading
  through `main.rs`, enabling reconnect without involving `main`

### Fixed
- `quelay-agent`: deadlock under QUIC flow-control backpressure eliminated by moving ack
  reads to a dedicated task rather than interleaving with the write pump in a `select!`
