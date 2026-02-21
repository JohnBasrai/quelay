#!/usr/bin/env bash
# ci-smoke-test.sh
#
# Validates the two-agent QUIC handshake and C2I baseline:
#
#   1. Builds workspace.
#   2. Starts agent-1 (server) and agent-2 (client) on loopback.
#   3. Waits for the QUIC handshake to complete.
#   4. Runs quelay-example against agent-1's C2I to assert:
#        - IDL version match
#        - get_link_state responds
#        - LinkSim transport loopback (in-process, no agents)
#        - Thrift wire-type mapping round-trips (in-process, no agents)
#        - QUIC transport loopback (direct peer, no C2I)
#
# What this does NOT test (blocked on data pump):
#   - set_callback / QueLayCallback notifications
#   - stream_start end-to-end file transfer
#   - ephemeral TCP port handoff to client
#
# Usage:
#   ./scripts/ci-smoke-test.sh
#
# Exit codes:
#   0  all assertions passed
#   1  build failed, timeout, or assertion failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"

AGENT1_C2I="127.0.0.1:9090"
AGENT2_C2I="127.0.0.1:9091"
AGENT1_QUIC="127.0.0.1:5000"

# Agent writes cert to its working directory ($WORKSPACE since we cd there).
CERT_SRC="$WORKSPACE/quelay-server.der"
CERT_FILE="/tmp/quelay-smoke-server.der"

AGENT1_PID=""
AGENT2_PID=""

# ---------------------------------------------------------------------------
# Cleanup — kill both agents on exit (normal or error).
# ---------------------------------------------------------------------------
cleanup() {
    local exit_code=$?
    [[ -n "$AGENT1_PID" ]] && kill "$AGENT1_PID" 2>/dev/null || true
    [[ -n "$AGENT2_PID" ]] && kill "$AGENT2_PID" 2>/dev/null || true
    rm -f "$CERT_FILE" "$CERT_SRC"
    exit $exit_code
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "==> Building workspace..."
cd "$WORKSPACE"

# Remove stale cert from any previous run so the cert-wait loop below
# cannot be fooled by a leftover file.
rm -f "$CERT_SRC" "$CERT_FILE"

cargo build --bin quelay-agent --bin quelay-example

# Resolve the actual target directory — CARGO_TARGET_DIR may redirect it
# away from $WORKSPACE/target (e.g. to ~/.cargo/target).
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')"

AGENT_BIN="$TARGET_DIR/debug/quelay-agent"
EXAMPLE_BIN="$TARGET_DIR/debug/quelay-example"

if [[ ! -x "$AGENT_BIN" ]]; then
    echo "ERROR: $AGENT_BIN not found after build." >&2
    ls -la "$TARGET_DIR/debug/" >&2
    exit 1
fi
if [[ ! -x "$EXAMPLE_BIN" ]]; then
    echo "ERROR: $EXAMPLE_BIN not found after build." >&2
    ls -la "$TARGET_DIR/debug/" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Start agent-1 (server role, writes cert)
# ---------------------------------------------------------------------------
echo "==> Starting agent-1 (server, QUIC $AGENT1_QUIC, C2I $AGENT1_C2I)..."
"$AGENT_BIN" \
    --agent-endpoint "$AGENT1_C2I" \
    server \
    --bind "$AGENT1_QUIC" \
    &
AGENT1_PID=$!

# Wait for the cert file — agent-1 writes it to $WORKSPACE before accepting.
echo "==> Waiting for server cert ($CERT_SRC)..."
for i in $(seq 1 20); do
    if [[ -f "$CERT_SRC" ]]; then
        cp "$CERT_SRC" "$CERT_FILE"
        break
    fi
    sleep 0.25
    if [[ $i -eq 20 ]]; then
        echo "ERROR: server cert not written after 5s — agent-1 may have crashed." >&2
        exit 1
    fi
done
echo "==> Cert found, copied to $CERT_FILE"

# ---------------------------------------------------------------------------
# Start agent-2 (client role)
# ---------------------------------------------------------------------------
echo "==> Starting agent-2 (client, peer $AGENT1_QUIC, C2I $AGENT2_C2I)..."
"$AGENT_BIN" \
    --agent-endpoint "$AGENT2_C2I" \
    client \
    --peer "$AGENT1_QUIC" \
    --cert "$CERT_FILE" \
    &
AGENT2_PID=$!

# ---------------------------------------------------------------------------
# Wait for C2I to become reachable on agent-1.
# The Thrift server only starts after the QUIC handshake completes.
# ---------------------------------------------------------------------------
echo "==> Waiting for agent-1 C2I ($AGENT1_C2I) to become reachable..."
for i in $(seq 1 40); do
    if nc -z 127.0.0.1 9090 2>/dev/null; then
        echo "==> C2I reachable after ~$((i * 250))ms"
        break
    fi
    sleep 0.25
    if [[ $i -eq 40 ]]; then
        echo "ERROR: agent-1 C2I not reachable after 10s." >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Run the smoke check via quelay-example.
# ---------------------------------------------------------------------------
echo "==> Running quelay-example smoke check against agent-1 ($AGENT1_C2I)..."
"$EXAMPLE_BIN" --agent-endpoint "$AGENT1_C2I"

echo ""
echo "==> Smoke test PASSED (QUIC handshake + C2I baseline — data pump not yet tested)."
