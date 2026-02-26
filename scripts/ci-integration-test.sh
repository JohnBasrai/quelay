#!/usr/bin/env bash
# ci-integration-test.sh
#
# Full integration test suite for Quelay.
# Replaces ci-e2e-test.sh.
#
# Runs all e2e_test subcommands across two agent configurations:
#   Phase A — 100 Mbit/s: large file throughput + rate limiter accuracy
#   Phase B —  10 Mbit/s: small files, link outage, DRR priority, edge cases
#
# Lower cap in Phase B gives the link-outage window and spool fill timing
# enough resolution to be deterministic on a loopback interface.
#
# Usage:
#   ./scripts/ci-integration-test.sh
#
# Exit codes:
#   0  all tests passed
#   1  build failed, agent startup failed, or any test assertion failed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"

AGENT1_C2I="127.0.0.1:9090"
AGENT2_C2I="127.0.0.1:9091"
AGENT1_QUIC="127.0.0.1:5000"

CERT_SRC="$WORKSPACE/quelay-server.der"
CERT_FILE="/tmp/quelay-integ-server.der"

AGENT1_PID=""
AGENT2_PID=""

# ---------------------------------------------------------------------------
# Cleanup — always kill both agents on exit
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

cargo build --bin quelay-agent --bin e2e-test

TARGET_DIR="$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')"

AGENT_BIN="$TARGET_DIR/debug/quelay-agent"
E2E_BIN="$TARGET_DIR/debug/e2e-test"
AGENT_EXTRA_ARGS=


# ---------------------------------------------------------------------------
# Helper: start both agents at a given BW cap
# ---------------------------------------------------------------------------
start_agents() {
    # ---
    local cap_mbps="$1"

    echo ""
    echo "==> Starting agents at ${cap_mbps} Mbit/s..."

    rm -f "$CERT_SRC" "$CERT_FILE"

    "$AGENT_BIN" $AGENT_EXTRA_ARGS \
        --bw-cap-mbps "$cap_mbps" \
        --agent-endpoint "$AGENT1_C2I" \
        server \
        --bind "$AGENT1_QUIC" \
        &
    AGENT1_PID=$!

    # Wait for the TLS cert written by the server agent.
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

    "$AGENT_BIN" $AGENT_EXTRA_ARGS \
        --bw-cap-mbps "$cap_mbps" \
        --agent-endpoint "$AGENT2_C2I" \
        client \
        --peer "$AGENT1_QUIC" \
        --cert "$CERT_FILE" \
        &
    AGENT2_PID=$!

    # Wait for both C2I ports to be reachable.
    for i in $(seq 1 40); do
        if nc -z 127.0.0.1 9090 2>/dev/null && nc -z 127.0.0.1 9091 2>/dev/null; then
            echo "==> Agents ready after ~$((i * 250))ms"
            break
        fi
        sleep 0.25
        if [[ $i -eq 40 ]]; then
            echo "ERROR: agents not reachable after 10s." >&2
            exit 1
        fi
    done
}

# ---------------------------------------------------------------------------
# Helper: stop both agents cleanly
# ---------------------------------------------------------------------------
stop_agents() {
    echo "==> Stopping agents..."
    [[ -n "$AGENT1_PID" ]] && kill "$AGENT1_PID" 2>/dev/null || true
    [[ -n "$AGENT2_PID" ]] && kill "$AGENT2_PID" 2>/dev/null || true
    AGENT1_PID=""
    AGENT2_PID=""
    sleep 0.5
}

# ===========================================================================
# Phase A — 100 Mbit/s
#   Large file throughput and rate limiter accuracy at a high rate.
# ===========================================================================
start_agents 100

############################################################
##   Testing 3 large files at 100 Mbit/s
##   Files: 30 MiB, 2 MiB, 512 KiB — bidirectional.
##   Asserts BW utilization within ±10% and SHA-256 integrity.
############################################################
"$E2E_BIN" \
    --sender-c2i "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    multi-file \
    --large \
    --bidirectional

stop_agents

# ===========================================================================
# Phase B — 10 Mbit/s
#   Lower cap gives link-outage and spool-fill timing deterministic windows.
# ===========================================================================
start_agents 10

############################################################
##   Testing small boundary-condition files
##   Files: 9000B (8 chunks+fragment), 1024B (exact chunk),
##          512B (half chunk), 1B (minimum stream).
##   Agents reconfigured to 1 KiB chunk size for this test.
##   Each size exercises a specific framing boundary identified
##   from field bugs in the legacy C++ system.
############################################################
"$E2E_BIN" \
    --sender-c2i "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    small-file-edge-cases

############################################################
##   Testing link outage recovery with 2 files
##   Starts a transfer, drops the link mid-stream, queues a
##   second file while the link is down, then re-enables the
##   link. Verifies both files complete with SHA-256 intact.
##   All timing derived from the agent's 10 Mbit/s BW cap.
############################################################
"$E2E_BIN" \
    --sender-c2i "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    multi-file \
    --count 2 \
    --link-outage


############################################################
##   Testing DRR scheduler priority ordering
##   Configures agents to 1 concurrent stream, enqueues an
##   anchor file at priority 0 plus 3 files at low/high/med
##   priorities. Asserts the pending queue returns
##   high → med → low order.
############################################################
"$E2E_BIN" \
    --sender-c2i "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    drr

stop_agents

AGENT_EXTRA_ARGS="--max-concurrent 2"
start_agents 10

############################################################
##   Testing bandwidth caps enfoced with multiple files
############################################################
"$E2E_BIN" \
    --sender-c2i   "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    multi-file \
    --count 2 \
    --large

AGENT_EXTRA_ARGS=

echo ""
echo "==> ci-integration-test.sh: all tests PASSED."
