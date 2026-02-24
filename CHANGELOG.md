# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](https://semver.org/).

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
