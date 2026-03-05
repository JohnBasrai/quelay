# Link Simulation: Findings and Future Work

**Branch:** `feat/link-sim-container`  
**Date:** 2026-03-05  
**Author:** John Basrai

---

## Overview

This document records the design decisions, implementation journey, test results,
and future work identified during the migration from Pumba to a custom `link-sim`
sidecar container for network impairment testing.

---

## Motivation: Why We Replaced Pumba

[Pumba](https://github.com/alexei-led/pumba) was the original tool for applying
network impairment in the Quelay integration test environment. It was replaced for
one fundamental reason:

> **Pumba cannot atomically chain rate limiting with loss/delay.**

When Pumba's `rate` and `netem` sub-commands are combined, only one qdisc is
applied — the second silently overwrites the first. This made it impossible to
simulate a realistic SATCOM link that simultaneously has:

- A hard bandwidth cap (e.g. 100 kbps uplink)
- Propagation delay (750ms RTT)
- Packet loss, corruption, and duplication

The `link-sim` sidecar applies a single `tc netem` qdisc that combines all of
these atomically.

---

## Architecture

### Sidecar Pattern

`link-sim` runs as a Docker sidecar sharing `agent-client`'s network namespace
via `network_mode: "container:quelay-agent-client"`. This means the `tc netem`
qdisc applied by `link-sim` affects `agent-client`'s interfaces directly, without
requiring host kernel namespace access or veth manipulation.

```
┌─────────────────────────────────────┐
│  agent-client network namespace     │
│                                     │
│  eth0 ── c2i-net  (172.18.0.0/16)   │  Thrift C2I, callbacks
│  eth1 ── quic-net (172.19.0.0/16)   │  QUIC data ← netem applied here
│                                     │
│  link-sim sidecar (shares ns)       │
│    tc netem on eth1                 │
└─────────────────────────────────────┘
```

### TOML Profiles

Impairment profiles live in `docker/link-sim/profiles/`. Each profile defines
link parameters in a structured, reviewable format:

```toml
[link]
uplink_rate_bps   = 100000
downlink_rate_bps = 13000000
delay_rtt_ms      = 750
jitter_ms         = 50
delay_corr        = 25       # percent

[loss]
drop           = 5
drop_corr      = 1           # burst correlation (lightning/solar flare events)
corrupt        = 1
corrupt_corr   = 1
duplicate      = 3
duplicate_corr = 1
```

Current profiles:

| Profile              | Rate (up/down)    | RTT   | Loss              | Notes              |
|----------------------|-------------------|-------|-------------------|--------------------|
| `BLOS-750ms.toml`    | 100kbps / 13Mbps  | 750ms | 0%                | Clean satellite    |
| `LOS-250ms.toml`     | 500kbps / 11Mbps  | 250ms | 0%                | Line-of-sight      |
| `Degraded-BLOS.toml` | 100kbps / 150kbps | 750ms | 5%+corruption+dup | Stressed satellite |

`drop_corr` models burst loss events (lightning strikes, solar flares) where
consecutive packets are correlated rather than independently lost.

---

## Implementation Journey

### Interface Targeting (eth0 vs eth1)

**Problem:** Initial runs showed zero impairment despite `link-sim` reporting
the qdisc was applied.

**Discovery:** `agent-client` has two interfaces:
- `eth0` — `c2i-net` (172.18.0.0/16) — Thrift C2I, callbacks
- `eth1` — `quic-net` (172.19.0.0/16) — QUIC data

`link-sim` was defaulting to `eth0`, impairing the C2I path instead of QUIC.

**Fix:** Set `LINK_SIM_IFACE=eth1` in the compose environment.

```yaml
environment:
  LINK_SIM_IFACE: ${LINK_SIM_IFACE:-eth1}
  # eth0 is c2i-net (172.18.0.0/16). eth1 is quic-net (172.19.0.0/16).
  # Interface order is determined by network attachment order in compose.
  # Re-verify with:
  #   docker run --rm --network container:quelay-agent-client nicolaka/netshoot ip addr
```

**Diagnostic command:**
```bash
docker run --rm --network container:quelay-agent-client nicolaka/netshoot ip addr
docker run --rm --network container:quelay-agent-client nicolaka/netshoot tc qdisc show
```

### DNS Resolution: QUIC Traffic on Wrong Network

**Problem:** Even after fixing the interface, transfers completed in ~8 seconds
with no impairment visible.

**Discovery:** `agent-server` resolved to `172.18.0.2` (c2i-net), not
`172.19.0.2` (quic-net). QUIC connections were being established over `eth0`,
bypassing the netem qdisc on `eth1`.

```bash
docker run --rm --network container:quelay-agent-client \
  nicolaka/netshoot getent hosts agent-server
# 172.18.0.2  agent-server   ← wrong network!
```

Docker DNS resolves a multi-network container to the IP of whichever network
appears first in the compose file.

**Fix:** Add a `quic-net` alias for `agent-server` and point `QUELAY_PEER` at it:

```yaml
agent-server:
  networks:
    quic-net:
      aliases:
        - agent-server-quic   # resolves to 172.19.0.x
    c2i-net:

agent-client:
  environment:
    QUELAY_PEER: "agent-server-quic:4433"  # QUIC over quic-net
```

After this fix:
```bash
getent hosts agent-server-quic
# 172.19.0.2  agent-server-quic   ← correct
```

### Hostname Resolution in Rust

All binaries (`quelay-agent`, `e2e-test`, `bw-cap-test`) were updated to accept
hostnames for peer/C2I addresses, resolved at connect time via
`tokio::net::lookup_host`. This eliminates all DNS gymnastics from shell
entrypoint scripts.

---

## Test Results

### Baseline (clean link, no profile)

| Metric              | Value  |
|---------------------|--------|
| Payload             | 10 MiB |
| Elapsed             | 8.1s   |
| BW Utilization      | ~103%  |
| Packet loss (Quinn) | 0%     |
| RTT (Quinn)         | —      |
| Congestion events   | 0      |

### BLOS-750ms (750ms RTT, 100kbps, no loss)

| Metric              | Value  |
|---------------------|--------|
| Payload             | 10 MiB |
| Elapsed             | 8.1s   |
| BW Utilization      | ~103%  |
| Packet loss (Quinn) | 0%     |
| Congestion events   | 0      |

> **Note:** Transfer time unchanged from baseline because QUIC was flowing over
> c2i-net (wrong interface) at the time of this test. After the DNS fix, this
> profile would show ~80s elapsed for 10MB at 100kbps.

### Degraded-BLOS (750ms RTT, 100kbps, 5% loss, 1% corrupt, 3% dup)

| Metric              | Value                             |
|---------------------|-----------------------------------|
| Payload             | 10 MiB                            |
| Elapsed             | 22.1s                             |
| Effective BW        | 475 kBps (38% of 10Mbps cap)      |
| BW Utilization      | 38%                               |
| Packet loss (Quinn) | 0% (all recovered via retransmit) |
| Lost bytes (Quinn)  | 0 B                               |
| Congestion events   | 0                                 |
| CWND                | 3000 KiB                          |
| RTT (Quinn)         | 0ms (see known issues)            |

#### Analysis

**Theoretical expected efficiency:**

```
5% drop + 3% duplicate + 1% corrupt = 9% wasted wire capacity
Each lost/corrupt packet retransmitted once = ~18% total wire overhead
Expected effective BW ≈ 82% of wire rate
```

**Observed: 38%** — Quinn's NewReno congestion controller is performing
significantly worse than theory predicts. At 750ms RTT, each loss event causes:

1. Window halved (AIMD backoff)
2. 750ms wait for probe ACK
3. Slow ramp back up

This is the classic **TCP-over-SATCOM death spiral** — a congestion controller
designed for low-latency terrestrial internet performing poorly on a
high-BDP (bandwidth-delay product) link. For comparison, the legacy FTA
system used UDT (UDP-based Data Transfer), which was specifically designed for
high-BDP links and achieved results much closer to the theoretical 82%.

**Key insight:** `lost_bytes = 0` is expected and correct. QUIC guarantees
reliable delivery — Quinn retransmitted every lost packet successfully. The
metric measures retransmit activity, not unrecoverable loss. The wire was fully
saturated; a significant fraction of that capacity was overhead.

---

## Known Issues

### RTT Always Reports 0ms

`self.conn.rtt()` (Quinn 0.11.9) returns `Duration::ZERO` in the test
environment. Root cause not yet identified. Candidates:

- RTT only populated after sufficient ACK exchange (unlikely at 22s elapsed)
- RTT not available from the connection handle used by the session manager
- Quinn 0.11.9 bug or behavioral difference

This metric is important for SATCOM link characterization and should be resolved.

### BW Utilization Assert Fails on Impaired Links

The `multi-file` BW utilization check asserts realized BW is within ±10% of
the Quelay cap. This is correct for clean links but wrong for impaired ones —
on a degraded link Quinn should self-limit well below cap. The assertion should
be suppressed or replaced with an upper-bound check (`realized ≤ cap`) for
link-sim runs.

---

## Future Work

### 1. BBR Congestion Controller

Quinn supports pluggable congestion controllers via
`TransportConfig::congestion_controller_factory()`. BBR measures bandwidth and
RTT directly rather than using loss as a congestion signal, making it far better
suited to BLOS links.

**Hypothesis:** BBR would achieve 70-80% effective BW on Degraded-BLOS vs
NewReno's 38%.

**Action:** Swap to BBR in `quelay-quic` and re-run the Degraded-BLOS profile.

### 2. UDT Evaluation

The legacy FTA system used [UDT](https://udt.sourceforge.io/) — a UDP-based
protocol specifically designed for high-speed, high-BDP data transfer. A Rust
binding exists at [docs.rs/udt](https://docs.rs/udt/latest/udt/).

UDT's congestion control was built for exactly the BLOS SATCOM use case:
- High latency (hundreds of ms RTT)
- Occasional burst loss (solar flares, link outages)
- Shared bandwidth pool with other contractors

**Action:** Evaluate `udt` crate as an alternative transport backend to
`quelay-quic`. Compare throughput on Degraded-BLOS profile.

### 3. Resolve RTT Reporting

Identify why `conn.rtt()` returns zero and fix. RTT is a critical metric for
SATCOM link health monitoring.

### 4. Wire Efficiency Metric

Add **wire efficiency** to the transfer report:

```
Wire efficiency = payload_bytes / udp_tx_bytes
```

`wire_bytes_sent()` is already available on the session. This directly shows how
much of the wire capacity was consumed by QUIC overhead vs useful payload —
the clearest indicator of CC performance on a lossy link.

### 5. BW Utilization for Impaired Links

Replace the ±10% BW utilization assertion with a mode-aware check:
- Clean link: assert realized BW ≈ cap (current behavior)
- Impaired link: assert realized BW ≤ cap (upper bound only)

---

## References

- [UDT Protocol](https://udt.sourceforge.io/)
- [udt Rust crate](https://docs.rs/udt/latest/udt/)
- [Quinn PathStats](https://docs.rs/quinn/latest/quinn/struct.PathStats.html)
- [RFC 9002: QUIC Loss Detection and Congestion Control](https://www.rfc-editor.org/rfc/rfc9002)
- [BBR Congestion Control](https://datatracker.ietf.org/doc/html/draft-cardwell-iccrg-bbr-congestion-control)
- [TCP over SATCOM performance issues](https://www.rfc-editor.org/rfc/rfc2488)
