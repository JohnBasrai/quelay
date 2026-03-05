#!/bin/sh
# agent-entrypoint.sh — launch quelay-agent from environment variables.
#
# Environment variables (set in docker-compose.yml):
#   QUELAY_MODE          server | client  (required)
#   QUELAY_AGENT_ENDPOINT  bind address for Thrift C2I (default: 0.0.0.0:9190)
#   QUELAY_CAP           bandwidth cap, e.g. 10Mbps  (default: uncapped)
#   QUELAY_BIND          QUIC bind address for server mode (default: 0.0.0.0:4433)
#   QUELAY_PEER          peer host:port for client mode (e.g. agent-server:4433)
#   QUELAY_CERT          path to server cert DER for client mode
#
# DNS resolution of QUELAY_PEER is handled inside quelay-agent at connect time.
# No getent / hostname gymnastics needed here.

set -e

: "${QUELAY_MODE:?QUELAY_MODE must be set to 'server' or 'client'}"
: "${QUELAY_AGENT_ENDPOINT:=0.0.0.0:9190}"

COMMON_ARGS="--agent-endpoint ${QUELAY_AGENT_ENDPOINT}"

if [ -n "${QUELAY_CAP}" ]; then
    COMMON_ARGS="${COMMON_ARGS} --bw-cap-bps ${QUELAY_CAP}"
fi

case "${QUELAY_MODE}" in
    server)
        : "${QUELAY_BIND:=0.0.0.0:4433}"
        exec quelay-agent ${COMMON_ARGS} server --bind "${QUELAY_BIND}"
        ;;
    client)
        : "${QUELAY_PEER:?QUELAY_PEER must be set in client mode}"
        : "${QUELAY_CERT:?QUELAY_CERT must be set in client mode}"

        # Wait for server cert to appear on the shared volume.
        until [ -f "${QUELAY_CERT}" ]; do
            sleep 0.2
        done

        exec quelay-agent ${COMMON_ARGS} client \
            --peer "${QUELAY_PEER}" \
            --cert "${QUELAY_CERT}"
        ;;
    *)
        echo "ERROR: QUELAY_MODE must be 'server' or 'client', got '${QUELAY_MODE}'" >&2
        exit 1
        ;;
esac
