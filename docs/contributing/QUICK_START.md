# Quick Start

## Prerequisites

- Rust stable (edition 2021 or later)
- `cargo` in your `PATH`
- `thrift-compiler` for regenerating C2I stubs (`apt-get install thrift-compiler`)

## Build

```bash
git clone https://github.com/JohnBasrai/quelay
cd quelay
./scripts/thrift-compile.sh   # generate Thrift C2I sources
cargo build --workspace
```

## Run Tests

```bash
cargo test --workspace
```

## Run the Smoke Test

```bash
./scripts/ci-smoke-test.sh
```

Starts two `quelay-agent` instances on loopback, validates QUIC handshake
and C2I reachability end-to-end in ~1 second.

## Run a Specific Test

```bash
cargo test -p quelay-domain c2i_drains_before_bulk
```

## Generate Docs

```bash
cargo doc --workspace --no-deps --open
```

## Project Layout

```
quelay/
├── quelay-domain/    # Domain model and core traits (start here)
├── quelay-link-sim/  # In-process link simulator
├── quelay-quic/      # QUIC transport (quinn)
├── quelay-thrift/    # Thrift C2I — IDL, generated stubs, mapping
├── quelay-agent/     # Deployable relay daemon
├── quelay-example/   # Demos and live C2I smoke check
├── scripts/          # CI and build helpers
└── docs/             # Contributing guides
```

Start in `quelay-domain/src/` to understand the vocabulary of the system
before reading any implementation crate.
