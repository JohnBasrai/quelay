# Copyright (c) 2026 John Basrai
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# link-sim — network impairment sidecar
#
# Runs as a sidecar sharing the network namespace of quelay-agent-client.
# Reads a TOML profile and applies a single tc netem qdisc to eth0,
# enabling combined rate + delay + loss + corrupt + duplicate impairment.
#
# Requires: cap_add: [NET_ADMIN]
# Usage:    network_mode: "container:quelay-agent-client"

FROM alpine:3.19

RUN apk add --no-cache iproute2 bash

COPY apply-profile.sh /usr/local/bin/apply-profile.sh
COPY link-sim-entrypoint.sh /usr/local/bin/entrypoints/link-sim-entrypoint.sh
RUN chmod +x \
      /usr/local/bin/apply-profile.sh \
      /usr/local/bin/entrypoints/link-sim-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/entrypoints/link-sim-entrypoint.sh"]
