#!/bin/sh
# e2e-entrypoint.sh — execs e2e-test with topology args from environment.
#
# --sender-c2i and --receiver-c2i now accept hostnames directly; DNS
# resolution is handled inside e2e-test at connect time.
# --callback-ip still requires a numeric IP (it is a local bind address).
#
# Args precedence:
#   1. Args passed directly:  docker compose run --rm e2e multi-file --size-mb 10
#   2. E2E_ARGS env var:      E2E_ARGS="multi-file --size-mb 10" docker compose up
#   3. Hardcoded default:     multi-file --size-mb 100 --bidirectional

CB_IP=$(hostname -i | awk '{print $1}')

if [ $# -gt 0 ]; then
    set -- "$@"
else
    # shellcheck disable=SC2086
    set -- ${E2E_ARGS:-multi-file --size-mb 100 --bidirectional}
fi

exec e2e-test \
    --sender-c2i   "agent-client:9190" \
    --receiver-c2i "agent-server:9191" \
    --callback-ip  "${CB_IP}" \
    "$@"
