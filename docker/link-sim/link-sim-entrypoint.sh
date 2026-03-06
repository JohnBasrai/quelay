#!/bin/sh
# link-sim-entrypoint.sh — apply tc netem impairment from a TOML profile.
#
# Environment variables (set in docker-compose.yml):
#   LINK_SIM_PROFILE   filename of TOML profile under /profiles/
#                      (e.g. BLOS-750ms.toml). Leave unset for no impairment.
#   LINK_SIM_IFACE     network interface to impair (default: eth0)
#
# Runs as a sidecar sharing quelay-agent-client's network namespace.
# Requires cap_add: [NET_ADMIN].

set -e

IFACE="${LINK_SIM_IFACE:-eth0}"

if [ -z "${LINK_SIM_PROFILE}" ]; then
    echo "link-sim: LINK_SIM_PROFILE not set — no impairment applied"
    exec sleep infinity
fi

PROFILE_PATH="/profiles/${LINK_SIM_PROFILE}"

if [ ! -f "${PROFILE_PATH}" ]; then
    echo "ERROR: profile not found: ${PROFILE_PATH}" >&2
    exit 1
fi

exec /usr/local/bin/apply-profile.sh "${PROFILE_PATH}" "${IFACE}"
