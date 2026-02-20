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
- `quelay-link-sim`: `MockTransport` and `MockSession` with `connected_pair()`
- `quelay-link-sim`: `LinkSimConfig` with presets (`perfect`, `degraded`, `outage_60s`)
- `quelay-link-sim`: `MockStream` implementing `AsyncRead + AsyncWrite`
- `quelay-quic`: stub crate (not yet implemented)
- `quelay-thrift`: C2I and callback service stubs (IDL revision pending)
