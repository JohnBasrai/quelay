#!/bin/sh
# e2e-entrypoint.sh — resolves container IPs and execs e2e-test.
#
# --callback-ip, --sender-c2i, --receiver-c2i all require std::net::IpAddr
# (not hostnames).  This script resolves the Docker DNS names at startup
# and prepends the topology args before any args passed by the caller.
#
# Args precedence:
#   1. Args passed directly:  docker compose run --rm e2e multi-file --size-mb 10
#   2. E2E_ARGS env var:      E2E_ARGS="multi-file --size-mb 10" docker compose up
#   3. Hardcoded default:     multi-file --size-mb 100 --bidirectional
CB_IP=$(hostname -i | awk '{print $1}')
SENDER_IP=$(getent hosts agent-client | awk '{print $1}')
RECEIVER_IP=$(getent hosts agent-server | awk '{print $1}')

if [ $# -gt 0 ]; then
    set -- "$@"
else
    # shellcheck disable=SC2086
    set -- ${E2E_ARGS:-multi-file --size-mb 100 --bidirectional}
fi

exec e2e-test \
    --sender-c2i   "${SENDER_IP}:9190" \
    --receiver-c2i "${RECEIVER_IP}:9191" \
    --callback-ip  "${CB_IP}" \
    "$@"
