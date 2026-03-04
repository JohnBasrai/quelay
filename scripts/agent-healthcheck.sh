#!/bin/sh
# agent-healthcheck.sh — Docker HEALTHCHECK probe for quelay-agent containers.
#
# Usage: agent-healthcheck.sh <port>
#
# Calls get_version() on the Thrift C2I interface.  Exits 0 if the agent
# is up and responding, non-zero otherwise.
exec quelay-example --agent-endpoint "127.0.0.1:${1}" --healthcheck
