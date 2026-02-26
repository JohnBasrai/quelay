#!/usr/bin/env bash
# ci-bw-cap-test.sh
#
# Usage:
#   ./scripts/ci-bw-cap-test.sh
#
# Exit codes:
#   0  all tests passed
#   1  build failed, agent startup failed, or any test assertion failed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"

. "${SCRIPT_DIR}/.common"

AGENT1_C2I="127.0.0.1:9090"
AGENT2_C2I="127.0.0.1:9091"
AGENT1_QUIC="127.0.0.1:5000"

CERT_SRC="$WORKSPACE/quelay-server.der"
CERT_FILE="/tmp/quelay-integ-server.der"

AGENT1_PID=""
AGENT2_PID=""

trap cleanup EXIT

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "==> Building workspace..."
cd "$WORKSPACE"
rm -f "$CERT_SRC" "$CERT_FILE"

cargo build --bin quelay-agent --bin bw-cap-test

AGENT_BIN="$TARGET_DIR/debug/quelay-agent"
BWC_BIN="$TARGET_DIR/debug/bw-cap-test"

# --bw-cap-mbps 10  --max-concurrent 4
start_agents 10 4

########################################################################
##   Testing bandwidth caps enforcement with multiple concurrent files
########################################################################
"$BWC_BIN" \
    --sender-c2i   "$AGENT1_C2I" \
    --receiver-c2i "$AGENT2_C2I" \
    --count 3 --duration-secs 5
echo ""
echo "==> ci-bw-cap-test.sh: all tests PASSED."
