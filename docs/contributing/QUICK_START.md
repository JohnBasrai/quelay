# Quick Start

## Prerequisites

- Rust stable (edition 2021 or later)
- `cargo` in your `PATH`

## Build

```bash
git clone https://github.com/jbasrai/quelay
cd quelay
cargo build
```

## Run Tests

```bash
cargo test
```

## Run a Specific Test

```bash
cargo test -p quelay-domain c2i_drains_before_bulk
```

## Generate Docs

```bash
cargo doc --open
```

## Project Layout

```
quelay/
├── quelay-domain/   # Domain model and core traits (start here)
├── quelay-link-sim/     # In-process link simulator (used by tests)
├── quelay-quic/     # QUIC transport (stub)
├── quelay-thrift/   # Thrift C2I stubs
├── scripts/      # CI helpers
└── docs/         # Contributing guides
```

Start in `quelay-domain/src/` to understand the vocabulary of the system
before reading any implementation crate.
