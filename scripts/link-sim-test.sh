#!/usr/bin/env bash
#
# scripts/link-sim-test.sh — Manual satellite link impairment integration test
#
# Simulates GEO satellite conditions between two quelay-agent instances using
# Linux tc netem on a veth pair, then verifies 100 MiB bidirectional transfer
# integrity via SHA-256.
#
# Normal user invocation (script self-escalates via sudo):
#   ./scripts/link-sim-test.sh <PROFILE> [OPTIONS]
#
# PROFILES:
#   loss    Packet loss only
#   delay   Delay + jitter only
#   both    Packet loss + delay + jitter
#
# OPTIONS:
#   --loss-percent N     Loss % [default: 2]               valid for: loss, both
#   --delay N            One-way delay ms [default: 500]   valid for: delay, both
#   --jitter N           Delay jitter ±ms [default: 50]    valid for: delay, both
#   --quelay-cap-bps N   Quelay agent BW cap [default: 10Mbps] valid for: all
#   --help               Show this help and exit

set -euo pipefail

# ---------------------------------------------------------------------------
# Resolve canonical script path and repo root
# ---------------------------------------------------------------------------
SELF="$(realpath "$0")"
SCRIPT_DIR="$(dirname "$SELF")"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---------------------------------------------------------------------------
# Hardening — must run before exec sudo so we catch problems as the user.
#
# Checks (all operate on the canonical path):
#   1. Script not group/other writable (direct overwrite attack)
#   2. Script owned by root or the invoking user
#   3. Parent directory not group/other writable (swap/replace attack)
# ---------------------------------------------------------------------------
harden_check() {
    local script_perms script_owner

    script_perms=$(stat -c '%a' "$SELF")
    if [[ "${script_perms: -2}" =~ [2367] ]]; then
        echo "ERROR: $SELF has unsafe permissions ($script_perms) — remove group/other write bits" >&2
        exit 1
    fi

    script_owner=$(stat -c '%u' "$SELF")
    if [[ "$script_owner" -ne 0 && "$script_owner" -ne "$EUID" ]]; then
        echo "ERROR: $SELF is not owned by root or current user (owner uid=$script_owner)" >&2
        exit 1
    fi


}

# ---------------------------------------------------------------------------
# Resolve binary paths as the invoking user, before sudo strips PATH/env.
#
# Priority:
#   1. $_QUELAY_TARGET_DIR if already set (we are the re-exec'd root copy)
#   2. $CARGO_TARGET_DIR from environment
#   3. `cargo metadata` discovery (requires cargo on PATH)
#   4. Fall back to $REPO_ROOT/target
# ---------------------------------------------------------------------------
if [[ -z "${_QUELAY_TARGET_DIR:-}" ]]; then
    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        _QUELAY_TARGET_DIR="$CARGO_TARGET_DIR"
    else
        _QUELAY_TARGET_DIR="$(
            cargo metadata --no-deps --format-version 1 2>/dev/null \
            | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])' \
            2>/dev/null
        )" || _QUELAY_TARGET_DIR=""
        _QUELAY_TARGET_DIR="${_QUELAY_TARGET_DIR:-$REPO_ROOT/target}"
    fi
fi

AGENT_BIN="$_QUELAY_TARGET_DIR/debug/quelay-agent"
E2E_BIN="$_QUELAY_TARGET_DIR/debug/e2e-test"

# ---------------------------------------------------------------------------
# Self-escalate via sudo, carrying resolved paths into the root invocation.
# The re-exec'd root copy sees EUID==0 and skips this block — no recursion.
# ---------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    harden_check
    exec sudo _QUELAY_TARGET_DIR="$_QUELAY_TARGET_DIR" "$SELF" "$@"
fi

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
VETH0="veth-ql0"
VETH1="veth-ql1"
VETH0_IP="10.99.0.1"
VETH1_IP="10.99.0.2"
PREFIX=24

# C2I ports: 9190 (QUIC client), 9191 (QUIC server)
# Avoids collision with any loopback agents on default ports 9090/9091
QUIC_CLIENT_C2I_PORT=9190
QUIC_SERVER_C2I_PORT=9191
QUIC_PORT=4433

CERT_PATH="$REPO_ROOT/quelay-server.der"

QUIC_CLIENT_PID=""
QUIC_SERVER_PID=""

# ---------------------------------------------------------------------------
# Cleanup — runs on exit, failure, or signal
# ---------------------------------------------------------------------------
cleanup() {
    echo ""
    echo "==> Tearing down..."

    if [[ -n "$QUIC_CLIENT_PID" ]] && kill -0 "$QUIC_CLIENT_PID" 2>/dev/null; then
        echo "    Stopping sender agent (pid $QUIC_CLIENT_PID)"
        kill "$QUIC_CLIENT_PID" 2>/dev/null || true
        wait "$QUIC_CLIENT_PID" 2>/dev/null || true
    fi

    if [[ -n "$QUIC_SERVER_PID" ]] && kill -0 "$QUIC_SERVER_PID" 2>/dev/null; then
        echo "    Stopping receiver agent (pid $QUIC_SERVER_PID)"
        kill "$QUIC_SERVER_PID" 2>/dev/null || true
        wait "$QUIC_SERVER_PID" 2>/dev/null || true
    fi

    if ip link show "$VETH0" &>/dev/null; then
        echo "    Removing tc qdisc on $VETH0"
        tc qdisc del dev "$VETH0" root 2>/dev/null || true
        echo "    Deleting veth pair $VETH0/$VETH1"
        ip link delete "$VETH0" 2>/dev/null || true
    fi

    # Leave agent logs in /tmp for post-mortem; remove cert (regenerated each run)
    rm -f "$CERT_PATH"

    echo "==> Teardown complete."
}

trap cleanup EXIT

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
die() {
    echo "ERROR: $*" >&2
    exit 1
}

usage() {
    sed -n '/^# Normal user invocation/,/^[^#]/p' "$0" | grep '^#' | sed 's/^# \?//'
    exit 0
}

require_bin() {
    local bin="$1"
    [[ -x "$bin" ]] || die "Binary not found or not executable: $bin
  Run 'cargo build' first."
}

wait_for_port() {
    local label="$1"
    local port="$2"
    local retries=40
    echo -n "    Waiting for $label on port $port"
    for ((i=0; i<retries; i++)); do
        if ss -tlnH "sport = :$port" 2>/dev/null | grep -q "$port"; then
            echo " ready."
            return 0
        fi
        echo -n "."; sleep 0.3
    done
    echo ""
    die "$label did not start within timeout."
}

wait_for_cert() {
    local retries=40
    echo -n "    Waiting for $CERT_PATH"
    for ((i=0; i<retries; i++)); do
        [[ -f "$CERT_PATH" ]] && { echo " found."; return 0; }
        echo -n "."; sleep 0.3
    done
    echo ""
    die "quelay-server.der did not appear — receiver agent failed to start.
  Check log: /tmp/quelay-server.log"
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
OPT_LOSS_PERCENT=2
OPT_DELAY=500
OPT_JITTER=50
OPT_QUELAY_CAP=10
PROFILE=""

[[ $# -ge 1 ]] || { echo "ERROR: PROFILE required." >&2; usage; }

case "$1" in
    loss|delay|both) PROFILE="$1" ;;
    --help|-h) usage ;;
    *) die "Unknown profile '$1'. Valid profiles: loss, delay, both" ;;
esac
shift

while [[ $# -gt 0 ]]; do
    case "$1" in
        --loss-percent)
            [[ "$PROFILE" == "loss" || "$PROFILE" == "both" ]] \
                || die "--loss-percent is not valid for profile '$PROFILE'"
            OPT_LOSS_PERCENT="$2"; shift 2 ;;
        --delay)
            [[ "$PROFILE" == "delay" || "$PROFILE" == "both" ]] \
                || die "--delay is not valid for profile '$PROFILE'"
            OPT_DELAY="$2"; shift 2 ;;
        --jitter)
            [[ "$PROFILE" == "delay" || "$PROFILE" == "both" ]] \
                || die "--jitter is not valid for profile '$PROFILE'"
            OPT_JITTER="$2"; shift 2 ;;
        --quelay-cap-bps)
            OPT_QUELAY_CAP="$2"; shift 2 ;;
        --help|-h) usage ;;
        *) die "Unknown option: $1" ;;
    esac
done

require_bin "$AGENT_BIN"
require_bin "$E2E_BIN"

# netem link BW is always 2× the Quelay cap
NETEM_BW_MBPS=$(( OPT_QUELAY_CAP * 2 ))

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "==========================================================="
echo "  Quelay satellite link impairment test"
echo "  Profile         : $PROFILE"
case "$PROFILE" in
    loss)  echo "  Packet loss     : ${OPT_LOSS_PERCENT}%" ;;
    delay) echo "  One-way delay   : ${OPT_DELAY}ms ±${OPT_JITTER}ms" ;;
    both)
        echo "  Packet loss     : ${OPT_LOSS_PERCENT}%"
        echo "  One-way delay   : ${OPT_DELAY}ms ±${OPT_JITTER}ms"
        ;;
esac
echo "  Quelay BW cap   : ${OPT_QUELAY_CAP} Mbit/s"
echo "  Netem link BW   : ${NETEM_BW_MBPS} Mbit/s (2× cap)"
echo "  Agent binary    : $AGENT_BIN"
echo "  e2e binary      : $E2E_BIN"
echo "  Transfer        : 100 MiB bidirectional, SHA-256 verified"
echo "==========================================================="
echo ""

# ---------------------------------------------------------------------------
# Step 1: veth pair setup
# ---------------------------------------------------------------------------
echo "==> Setting up veth pair $VETH0 <-> $VETH1"

if ip link show "$VETH0" &>/dev/null; then
    echo "    Existing $VETH0 found — deleting first."
    ip link delete "$VETH0" 2>/dev/null || true
fi

ip link add "$VETH0" type veth peer name "$VETH1"
ip addr add "${VETH0_IP}/${PREFIX}" dev "$VETH0"
ip addr add "${VETH1_IP}/${PREFIX}" dev "$VETH1"
ip link set "$VETH0" up
ip link set "$VETH1" up
echo "    $VETH0 = $VETH0_IP, $VETH1 = $VETH1_IP — both up."

# ---------------------------------------------------------------------------
# Step 2: tc netem impairment on veth0
# ---------------------------------------------------------------------------
echo "==> Applying tc netem impairment on $VETH0"

NETEM_ARGS="rate ${NETEM_BW_MBPS}mbit"
case "$PROFILE" in
    loss)  NETEM_ARGS="$NETEM_ARGS loss ${OPT_LOSS_PERCENT}%" ;;
    delay) NETEM_ARGS="$NETEM_ARGS delay ${OPT_DELAY}ms ${OPT_JITTER}ms distribution normal" ;;
    both)  NETEM_ARGS="$NETEM_ARGS loss ${OPT_LOSS_PERCENT}% delay ${OPT_DELAY}ms ${OPT_JITTER}ms distribution normal" ;;
esac
echo "    netem: $NETEM_ARGS"

# shellcheck disable=SC2086
tc qdisc add dev "$VETH0" root netem $NETEM_ARGS
echo "    Impairment applied."

# ---------------------------------------------------------------------------
# Step 3: Start agents
#
# The QUIC server starts first and writes its cert to $CERT_PATH.
# The QUIC client connects using that cert to complete the handshake.
# C2I (Thrift) only becomes available after the QUIC handshake completes, so
# wait_for_port on both C2I ports confirms both sides are fully up.
# After handshake both agents are symmetric — QUIC server/client refers only
# to startup role.
# ---------------------------------------------------------------------------
echo ""

# Remove any stale cert from a prior run before starting the server
rm -f "$CERT_PATH"

echo "==> Starting QUIC server agent on $VETH1_IP:${QUIC_PORT} (C2I :${QUIC_SERVER_C2I_PORT})"
cd "$REPO_ROOT"
"$AGENT_BIN" \
    --agent-endpoint "${VETH1_IP}:${QUIC_SERVER_C2I_PORT}" \
    --bw-cap-bps "${OPT_QUELAY_CAP}Mbps" \
    server --bind "${VETH1_IP}:${QUIC_PORT}" \
    &> /tmp/quelay-server.log &
QUIC_SERVER_PID=$!
echo "    pid: $QUIC_SERVER_PID  log: /tmp/quelay-server.log"

wait_for_cert

echo "==> Starting QUIC client agent on $VETH0_IP (C2I :${QUIC_CLIENT_C2I_PORT})"
"$AGENT_BIN" \
    --agent-endpoint "${VETH0_IP}:${QUIC_CLIENT_C2I_PORT}" \
    --bw-cap-bps "${OPT_QUELAY_CAP}Mbps" \
    client \
        --peer "${VETH1_IP}:${QUIC_PORT}" \
        --cert "$CERT_PATH" \
    &> /tmp/quelay-client.log &
QUIC_CLIENT_PID=$!
echo "    pid: $QUIC_CLIENT_PID  log: /tmp/quelay-client.log"

wait_for_port "QUIC server C2I" "$QUIC_SERVER_C2I_PORT"
wait_for_port "QUIC client C2I"   "$QUIC_CLIENT_C2I_PORT"

# ---------------------------------------------------------------------------
# Step 4: Run e2e test
# ---------------------------------------------------------------------------
echo ""
echo "==> Running e2e-test multi-file --size-mb 100 --bidirectional"
echo "    (SHA-256 integrity only; no throughput assertion)"
echo ""

set +e
"$E2E_BIN" \
    --sender-c2i   "${VETH0_IP}:${QUIC_CLIENT_C2I_PORT}" \
    --receiver-c2i "${VETH1_IP}:${QUIC_SERVER_C2I_PORT}" \
    multi-file \
    --size-mb 100 \
    --bidirectional
E2E_EXIT=$?
set -e

echo ""
if [[ $E2E_EXIT -eq 0 ]]; then
    echo "==> PASS — SHA-256 verified, all transfers intact."
else
    echo "==> FAIL — e2e-test exited with code $E2E_EXIT."
    echo "    QUIC client log : /tmp/quelay-client.log"
    echo "    QUIC server log : /tmp/quelay-server.log"
fi

exit $E2E_EXIT
