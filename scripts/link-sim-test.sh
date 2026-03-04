#!/usr/bin/env bash
# Copyright (c) 2026 John Basrai
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# scripts/link-sim-test.sh
#
# Run the Quelay link-simulation integration test suite using Docker Compose.
# All network impairment is applied by Pumba on the quic-net bridge; no host
# kernel namespaces or veth pairs required.
#
# Usage:
#   ./scripts/link-sim-test.sh <profile> [options]
#
# Profiles:
#   loss    Packet loss only
#   delay   Latency / jitter only
#   rate    Bandwidth cap only (via pumba rate)
#   both    Loss + delay combined
#   clean   No impairment (baseline sanity check)
#
# Options:
#   --loss-percent  N    Packet loss % for 'loss' / 'both' profiles  (default: 5)
#   --delay-ms      N    Base delay ms for 'delay' / 'both' profiles (default: 600)
#   --jitter-ms     N    Jitter ms for 'delay' / 'both' profiles     (default: 100)
#   --rate          STR  Bandwidth for 'rate' profile, e.g. 2mbit     (default: 2mbit)
#   --bw-cap        STR  Quelay daemon BW cap, e.g. 10Mbps            (default: 10Mbps)
#   --size-mb       N    Payload size per stream in MiB               (default: 100)
#   --e2e-args      STR  Override entire e2e command after binary     (default: see below)
#   --no-build           Skip --build flag (use cached images)
#   -h, --help           Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker-compose.yml"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
PROFILE=""
OPT_LOSS_PERCENT=5
OPT_DELAY_MS=600
OPT_JITTER_MS=100
OPT_RATE="2mbit"
: "${OPT_BW_CAP:=10Mbps}"
OPT_SIZE_MB=100
OPT_E2E_ARGS=""
OPT_NO_BUILD=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
usage() {
    sed -n '/^# Usage:/,/^[^#]/{ /^[^#]/d; s/^# \{0,3\}//; p }' "$0"
    exit "${1:-0}"
}

die() { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
[[ $# -eq 0 ]] && usage 1

PROFILE="$1"; shift
if [ "${PROFILE}" = "-h" -o "${PROFILE}" = "--help" ] ; then
    usage 0
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --loss-percent) OPT_LOSS_PERCENT="$2"; shift 2 ;;
        --delay-ms)     OPT_DELAY_MS="$2";     shift 2 ;;
        --jitter-ms)    OPT_JITTER_MS="$2";    shift 2 ;;
        --rate)         OPT_RATE="$2";          shift 2 ;;
        --bw-cap)       OPT_BW_CAP="$2";        shift 2 ;;
        --size-mb)      OPT_SIZE_MB="$2";        shift 2 ;;
        --e2e-args)     OPT_E2E_ARGS="$2";      shift 2 ;;
        --no-build)     OPT_NO_BUILD=1;          shift   ;;
        -h|--help)      usage 0 ;;
        *) die "Unknown option: $1" ;;
    esac
done

# ---------------------------------------------------------------------------
# Translate profile → PUMBA_ARGS
# ---------------------------------------------------------------------------
case "$PROFILE" in
    loss)
        PUMBA_ARGS="loss --percent ${OPT_LOSS_PERCENT}"
        ;;
    delay)
        PUMBA_ARGS="delay --time ${OPT_DELAY_MS} --jitter ${OPT_JITTER_MS}"
        ;;
    rate)
        PUMBA_ARGS="rate --rate ${OPT_RATE}"
        ;;
    both)
        # Pumba accepts multiple netem sub-commands chained on one invocation.
        PUMBA_ARGS="loss --percent ${OPT_LOSS_PERCENT} delay --time ${OPT_DELAY_MS} --jitter ${OPT_JITTER_MS}"
        ;;
    clean)
        # No impairment. Pumba still starts but applies a 1ms delay, no jitter.
        # delay --time 0 is rejected; jitter must be < delay so omit it here.
        PUMBA_ARGS="delay --time 1 --jitter 0"
        ;;
    *)
        die "Unknown profile '${PROFILE}'. Valid: loss | delay | rate | both | clean"
        ;;
esac

# ---------------------------------------------------------------------------
# Compose e2e command override
# ---------------------------------------------------------------------------
if [[ -z "$OPT_E2E_ARGS" ]]; then
    OPT_E2E_ARGS="multi-file --size-mb ${OPT_SIZE_MB} --bidirectional"
fi

# ---------------------------------------------------------------------------
# Cleanup trap
# ---------------------------------------------------------------------------
cleanup() {
    local rc=$?
    echo ""
    echo "==> Tearing down containers..."
    docker compose -f "$COMPOSE_FILE" down --volumes --remove-orphans 2>/dev/null || true
    exit $rc
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------
echo "==> Quelay link-sim-test"
echo "    profile    : ${PROFILE}"
echo "    pumba args : ${PUMBA_ARGS}"
echo "    bw-cap     : ${OPT_BW_CAP}"
echo "    e2e args   : ${OPT_E2E_ARGS}"
echo ""

export QUELAY_CAP="$OPT_BW_CAP"
export PUMBA_ARGS
export E2E_ARGS="$OPT_E2E_ARGS"

BUILD_FLAG="--build"
[[ $OPT_NO_BUILD -eq 1 ]] && BUILD_FLAG=""

docker compose \
    -f "$COMPOSE_FILE" \
    up \
    $BUILD_FLAG \
    --abort-on-container-exit \
    --exit-code-from e2e

# Exit code is propagated from the e2e container by --exit-code-from.
# The trap will call cleanup() with whatever exit code docker compose returns.
