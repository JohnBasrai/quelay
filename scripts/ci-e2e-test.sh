#!/usr/bin/env bash
# ci-e2e-test.sh
#
# End-to-end transfer test: exercises the full data pump path through two
# live agents.
#
# What this tests (once data pump is implemented):
#   1. Sender client calls set_callback, stream_start
#   2. Sender agent opens QUIC stream, writes StreamHeader
#   3. Sender agent opens ephemeral TCP port, fires stream_started callback
#   4. Sender client connects to ephemeral port, writes file bytes
#   5. Receiver agent accepts QUIC stream, reads StreamHeader
#   6. Receiver agent opens ephemeral TCP port, fires stream_started callback
#   7. Receiver client connects, reads bytes, writes to disk
#   8. Receiver agent fires stream_done callback on EOF
#   9. Sender agent receives WormholeMsg::Done, fires stream_done callback
#  10. Assert: received file matches sent file (size + sha256)
#
# Usage:
#   ./scripts/ci-e2e-test.sh
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

CERT_SRC="$WORKSPACE/quelay-server.der"
CERT_FILE="/tmp/quelay-e2e-server.der"

AGENT1_PID=""
AGENT2_PID=""

# ---------------------------------------------------------------------------
# Cleanup
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
rm -f "$CERT_SRC" "$CERT_FILE"

cargo build --bin quelay-agent --bin quelay-example

TARGET_DIR="$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')"

AGENT_BIN="$TARGET_DIR/debug/quelay-agent"
EXAMPLE_BIN="$TARGET_DIR/debug/quelay-example"

# ---------------------------------------------------------------------------
# Start agents
# ---------------------------------------------------------------------------
echo "==> Starting agent-1 (server, QUIC $AGENT1_QUIC, C2I $AGENT1_C2I)..."
"$AGENT_BIN" \
    --agent-endpoint "$AGENT1_C2I" \
    server \
    --bind "$AGENT1_QUIC" \
    &
AGENT1_PID=$!

echo "==> Waiting for server cert ($CERT_SRC)..."
for i in $(seq 1 20); do
    if [[ -f "$CERT_SRC" ]]; then
        cp "$CERT_SRC" "$CERT_FILE"
        break
    fi
    sleep 0.25
    if [[ $i -eq 20 ]]; then
        echo "ERROR: server cert not written after 5s." >&2
        exit 1
    fi
done
echo "==> Cert found, copied to $CERT_FILE"

echo "==> Starting agent-2 (client, peer $AGENT1_QUIC, C2I $AGENT2_C2I)..."
"$AGENT_BIN" \
    --agent-endpoint "$AGENT2_C2I" \
    client \
    --peer "$AGENT1_QUIC" \
    --cert "$CERT_FILE" \
    &
AGENT2_PID=$!

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
# Run e2e transfer test
# ---------------------------------------------------------------------------
echo "==> Running e2e transfer test..."
"$EXAMPLE_BIN" \
    --agent-endpoint    "$AGENT1_C2I" \
    --receiver-endpoint "$AGENT2_C2I"

echo ""
echo "==> E2E test PASSED."
