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

. "${SCRIPT_DIR}/.common"

trap cleanup EXIT

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "==> Building workspace..."
cd "$WORKSPACE"
rm -f "$CERT_SRC" "$CERT_FILE"

cargo build --bin quelay-agent --bin e2e-test

AGENT_BIN="$TARGET_DIR/debug/quelay-agent"
E2E_BIN="$TARGET_DIR/debug/e2e-test"

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

start_agents 10 2
"$E2E_BIN" \
    --sender-c2i "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    max-concurrent

echo ""
echo "==> ci-integration-test.sh: all tests PASSED."
