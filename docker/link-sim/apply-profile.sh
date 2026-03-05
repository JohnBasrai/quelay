#!/usr/bin/env bash
# Copyright (c) 2026 John Basrai
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# apply-profile.sh — parse a link-sim TOML profile and apply tc netem
#
# Usage: apply-profile.sh <profile.toml> [interface]
#
# Default interface: eth0
# Must run with NET_ADMIN capability inside the target container's
# network namespace (sidecar pattern).

set -euo pipefail

PROFILE="${1:-}"
IFACE="${2:-eth0}"

if [[ -z "$PROFILE" ]]; then
    echo "Usage: apply-profile.sh <profile.toml> [interface]" >&2
    exit 1
fi

if [[ ! -f "$PROFILE" ]]; then
    echo "ERROR: profile not found: $PROFILE" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Minimal TOML parser — handles key = value and key = "value" under [section]
# Only reads the fields we care about; ignores everything else.
# ---------------------------------------------------------------------------
parse_toml() {
    local file="$1"
    local section=""
    while IFS= read -r line; do
        # Strip inline comments and trim whitespace
        line="${line%%#*}"
        line="${line#"${line%%[![:space:]]*}"}"
        line="${line%"${line##*[![:space:]]}"}"
        [[ -z "$line" ]] && continue

        # Section header
        if [[ "$line" =~ ^\[([a-zA-Z_]+)\]$ ]]; then
            section="${BASH_REMATCH[1]}"
            continue
        fi

        # key = value or key = "value"
        if [[ "$line" =~ ^([a-zA-Z_]+)[[:space:]]*=[[:space:]]*\"?([^\"]*)\"?$ ]]; then
            local key="${BASH_REMATCH[1]}"
            local val="${BASH_REMATCH[2]}"
            # Emit as SECTION_KEY=value for eval
            echo "${section:+${section}_}${key}=${val}"
        fi
    done < "$file"
}

# Load profile into environment
eval "$(parse_toml "$PROFILE")"

# ---------------------------------------------------------------------------
# Required fields with defaults
# ---------------------------------------------------------------------------
uplink_rate_bps="${link_uplink_rate_bps:-0}"
downlink_rate_bps="${link_downlink_rate_bps:-0}"
delay_rtt_ms="${link_delay_rtt_ms:-0}"
jitter_ms="${link_jitter_ms:-0}"
delay_corr="${link_delay_corr:-0}"

drop_pct="${loss_drop:-0}"
drop_corr="${loss_drop_corr:-0}"
corrupt_pct="${loss_corrupt:-0}"
corrupt_corr="${loss_corrupt_corr:-0}"
duplicate_pct="${loss_duplicate:-0}"
duplicate_corr="${loss_duplicate_corr:-0}"

# ---------------------------------------------------------------------------
# Build tc netem command
# We impair egress on the interface that faces quic-net (eth0 in the sidecar).
# Pumba impaired agent-client egress (uplink); we preserve that convention.
# Rate is derived from uplink_rate_bps.
# ---------------------------------------------------------------------------

# Clear any existing qdisc
tc qdisc del dev "$IFACE" root 2>/dev/null || true

TC_CMD="tc qdisc add dev $IFACE root netem"

# Delay
if [[ "$delay_rtt_ms" -gt 0 ]]; then
    TC_CMD+=" delay ${delay_rtt_ms}ms"
    if [[ "$jitter_ms" -gt 0 ]]; then
        TC_CMD+=" ${jitter_ms}ms"
        if [[ "$delay_corr" -gt 0 ]]; then
            TC_CMD+=" ${delay_corr}%"
        fi
    fi
fi

# Loss / drop
if [[ "$drop_pct" -gt 0 ]]; then
    TC_CMD+=" loss ${drop_pct}%"
    if [[ "$drop_corr" -gt 0 ]]; then
        TC_CMD+=" ${drop_corr}%"
    fi
fi

# Corrupt
if [[ "$corrupt_pct" -gt 0 ]]; then
    TC_CMD+=" corrupt ${corrupt_pct}%"
    if [[ "$corrupt_corr" -gt 0 ]]; then
        TC_CMD+=" ${corrupt_corr}%"
    fi
fi

# Duplicate
if [[ "$duplicate_pct" -gt 0 ]]; then
    TC_CMD+=" duplicate ${duplicate_pct}%"
    if [[ "$duplicate_corr" -gt 0 ]]; then
        TC_CMD+=" ${duplicate_corr}%"
    fi
fi

# Rate (uplink — egress of agent-client)
if [[ "$uplink_rate_bps" -gt 0 ]]; then
    TC_CMD+=" rate ${uplink_rate_bps}bps"
fi

echo "link-sim: applying profile: $PROFILE"
echo "link-sim: interface: $IFACE"
echo "link-sim: $TC_CMD"

$TC_CMD

echo "link-sim: qdisc applied:"
tc qdisc show dev "$IFACE"

# Sleep forever — container stays alive holding the qdisc
trap 'exit 0' TERM INT
sleep infinity &
wait

