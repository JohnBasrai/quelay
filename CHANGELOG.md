# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added
- `quelay-domain`: transport traits (`QueLayStream`, `QueLaySession`, `QueLayTransport`)
- `quelay-domain`: `Priority` enum (`C2I`, `BulkTransfer`)
- `quelay-domain`: `LinkState` enum (`Connecting`, `Normal`, `Degraded`, `Failed`)
- `quelay-domain`: `DrrScheduler` — Deficit Round Robin with strict C2I priority
- `quelay-domain`: `QueLayHandler` callback trait with default no-op implementations
- `quelay-domain`: `StreamMeta`, `TransferProgress`, `Direction` types
- `quelay-link-sim`: `LinkSimTransport` and `LinkSimSession` with `connected_pair()`
- `quelay-link-sim`: `LinkSimConfig` with presets (`perfect`, `degraded`, `outage_60s`)
- `quelay-link-sim`: `LinkSimStream` implementing `AsyncRead + AsyncWrite`
- `quelay-link-sim`: `TokenBucket` — wall-clock token bucket for BW cap enforcement
- `quelay-quic`: QUIC transport via `quinn` — TLS, self-signed cert generation and pinning
- `quelay-thrift`: Thrift C2I and callback service — IDL, generated stubs, wire↔domain mapping
- `quelay-agent`: relay daemon with Thrift C2I, `SessionManager`, exponential backoff
  reconnection (1 s → 30 s cap), 6-byte wire framing + JSON stream metadata
- `quelay-agent`: `ci-smoke-test.sh` — two-agent QUIC handshake smoke test (~1 s)
- GitHub Actions CI: fmt check, clippy `-D warnings`, unit tests, smoke test on PR to main
