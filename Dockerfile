# Copyright (c) 2026 John Basrai
# SPDX-License-Identifier: MIT OR Apache-2.0

# ---------------------------------------------------------------------------
# Stage 1 — build
# ---------------------------------------------------------------------------
FROM rust:latest AS builder

WORKDIR /build

# Cache workspace dependencies before copying real source.
# Only manifests are copied here; a source-only change won't bust this layer.
COPY Cargo.toml Cargo.lock ./
COPY quelay-agent/Cargo.toml   quelay-agent/Cargo.toml
COPY quelay-domain/Cargo.toml  quelay-domain/Cargo.toml
COPY quelay-quic/Cargo.toml    quelay-quic/Cargo.toml
COPY quelay-thrift/Cargo.toml  quelay-thrift/Cargo.toml
COPY quelay-example/Cargo.toml quelay-example/Cargo.toml

# Stub build: give Cargo the minimal source it needs to compile the full
# dependency graph, then discard it.  Real source arrives in the next COPY.
#
# quelay-example is excluded from --bin here because its real main.rs declares
# submodules (mod e2e_demo, mod quic_demo, ...) that a bare "fn main(){}" stub
# can't satisfy.  Its transitive deps are compiled anyway via the other crates,
# and the real binary is built from real source in the second pass below.
RUN mkdir -p \
      quelay-agent/src/bin/e2e-test \
      quelay-agent/src/bin/bw_cap_test \
      quelay-domain/src \
      quelay-quic/src \
      quelay-thrift/src \
      quelay-example/src \
 && echo 'fn main(){}' > quelay-agent/src/main.rs \
 && echo 'fn main(){}' > quelay-agent/src/bin/e2e-test/main.rs \
 && echo 'fn main(){}' > quelay-agent/src/bin/bw_cap_test/mod.rs \
 && echo ''            > quelay-domain/src/lib.rs \
 && echo ''            > quelay-quic/src/lib.rs \
 && echo ''            > quelay-thrift/src/lib.rs \
 && echo 'fn main(){}' > quelay-example/src/main.rs \
 && cargo build --release --features quelay-agent/test-hooks \
      --bin quelay-agent --bin e2e-test --bin bw-cap-test \
 && rm -rf quelay-agent/src quelay-domain/src quelay-quic/src \
           quelay-thrift/src quelay-example/src

# Copy real source and rebuild only the changed crates.
COPY quelay-agent/   quelay-agent/
COPY quelay-domain/  quelay-domain/
COPY quelay-quic/    quelay-quic/
COPY quelay-thrift/  quelay-thrift/
COPY quelay-example/ quelay-example/

# Touch the entry points Cargo tracks so it sees them as dirty vs. the stubs.
RUN touch \
      quelay-agent/src/main.rs \
      quelay-agent/src/bin/e2e-test/main.rs \
      quelay-agent/src/bin/bw_cap_test/mod.rs \
      quelay-domain/src/lib.rs \
      quelay-quic/src/lib.rs \
      quelay-thrift/src/lib.rs \
 && cargo build --release --features quelay-agent/test-hooks

# ---------------------------------------------------------------------------
# Stage 2 — agent runtime image
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS agent

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/quelay-agent  /usr/local/bin/quelay-agent
COPY --from=builder /build/target/release/quelay-example /usr/local/bin/quelay-example
COPY scripts/agent-healthcheck.sh /usr/local/bin/agent-healthcheck.sh
RUN chmod +x /usr/local/bin/agent-healthcheck.sh

# Working directory is also the default cert output path for the 'server'
# subcommand (quelay-server.der is written here).
WORKDIR /app

ENTRYPOINT ["quelay-agent"]

# ---------------------------------------------------------------------------
# Stage 3 — e2e test runner image
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS e2e

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# e2e-test is built with --features quelay-agent/test-hooks so that
# link_enable / set_max_concurrent / set_chunk_size_bytes RPC calls are
# compiled in and wired up in the agents under test.
COPY --from=builder /build/target/release/e2e-test /usr/local/bin/e2e-test
COPY scripts/e2e-entrypoint.sh /usr/local/bin/e2e-entrypoint.sh
RUN chmod +x /usr/local/bin/e2e-entrypoint.sh

WORKDIR /app

ENTRYPOINT ["/usr/local/bin/e2e-entrypoint.sh"]
