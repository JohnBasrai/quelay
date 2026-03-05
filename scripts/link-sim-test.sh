#!/usr/bin/env bash
# Copyright (c) 2026 John Basrai
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# scripts/link-sim-test.sh
#
# Run the Quelay link-simulation integration test suite using Docker Compose.
# Network impairment is applied by the link-sim sidecar container using
# tc netem TOML profiles.
#
# Usage:
#   ./scripts/link-sim-test.sh <profile> [options]
#
# Profile:
#   BLOS-750ms      Beyond Line-of-Sight, 750ms RTT, clean
#   LOS-250ms       Line-of-Sight, 250ms RTT, clean
#   Degraded-BLOS   BLOS with packet loss, corruption, duplicates
#   clean           No impairment (baseline sanity check)
#
# Options:
#   --bw-cap    STR  Quelay daemon BW cap, e.g. 10Mbps  (default: 10Mbps)
#   --size-mb   N    Payload size in MiB                (default: 100)
#   --e2e-args  STR  Override entire e2e subcommand     (default: see below)
#   --no-build       Skip docker compose build
#   -h, --help       Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker-compose.yml"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
PROFILE=""
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
if [[ "${PROFILE}" == "-h" || "${PROFILE}" == "--help" ]]; then
    usage 0
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bw-cap)    OPT_BW_CAP="$2";   shift 2 ;;
        --size-mb)   OPT_SIZE_MB="$2";  shift 2 ;;
        --e2e-args)  OPT_E2E_ARGS="$2"; shift 2 ;;
        --no-build)  OPT_NO_BUILD=1;    shift   ;;
        -h|--help)   usage 0 ;;
        *) die "Unknown option: $1" ;;
    esac
done

# ---------------------------------------------------------------------------
# Translate profile → LINK_SIM_PROFILE
# ---------------------------------------------------------------------------
case "$PROFILE" in
    clean)
        LINK_SIM_PROFILE=""
        ;;
    BLOS-750ms|LOS-250ms|Degraded-BLOS)
        LINK_SIM_PROFILE="${PROFILE}.toml"
        ;;
    *.toml)
        # Allow passing a full filename directly
        LINK_SIM_PROFILE="${PROFILE}"
        ;;
    *)
        die "Unknown profile '${PROFILE}'. Valid: BLOS-750ms | LOS-250ms | Degraded-BLOS | clean"
        ;;
esac

# ---------------------------------------------------------------------------
# Compose e2e command
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
echo "    profile         : ${PROFILE}"
echo "    link-sim profile: ${LINK_SIM_PROFILE:-<none — clean link>}"
echo "    bw-cap          : ${OPT_BW_CAP}"
echo "    e2e args        : ${OPT_E2E_ARGS}"
echo ""

export QUELAY_CAP="$OPT_BW_CAP"
export LINK_SIM_PROFILE
export E2E_ARGS="$OPT_E2E_ARGS"

BUILD_FLAG="--build"
[[ $OPT_NO_BUILD -eq 1 ]] && BUILD_FLAG=""

docker compose \
    -f "$COMPOSE_FILE" \
    up \
    $BUILD_FLAG \
    --abort-on-container-exit \
    --exit-code-from e2e
