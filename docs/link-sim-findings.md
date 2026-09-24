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

### TOML Link Sim Profiles

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

| Interface | Network                    | Purpose               |
|-----------|----------------------------|-----------------------|
| `eth0`    | `c2i-net` (172.18.0.0/16)  | Thrift C2I, callbacks |
| `eth1`    | `quic-net` (172.19.0.0/16) | QUIC data             |

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

**Problem:** Even after fixing the interface, transfers still
completed in ~8 seconds — matching the unimpaired baseline — with no
impairment visible.

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

> **Update (PR #28):** narrower than originally thought. Re-running
> Degraded-BLOS with NewReno/BBR/Cubic selectable (see
> [Completed Investigations #1](#1-bbr-congestion-controller-pr-28-v040)) shows
> Cubic and NewReno both report real RTT values (753–849 ms); only
> **BBR** still reports 0ms throughout the transfer. See
> [Remaining Future Work #2](#2-resolve-rtt-reporting-bbr-specific) for the narrowed
> issue.

### BW Utilization Assert Fails on Impaired Links — Resolved (PR #28)

The original `multi-file` BW utilization check asserted realized BW was within
±10% of the Quelay cap. That was correct for clean links but wrong for
impaired ones, where Quinn can legitimately self-limit below the cap. PR #28
added `--skip-bw-check` for impaired-link runs while preserving the SHA-256
integrity check; clean-link runs retain the ±10% assertion.

---

## Completed Investigations

### 1. BBR Congestion Controller (PR #28, v0.4.0)

Quinn supports pluggable congestion controllers via
`TransportConfig::congestion_controller_factory()`. BBR measures bandwidth and
RTT directly rather than using loss as a congestion signal, making it far better
suited to BLOS links.

**Prediction (validated directionally):** BBR would achieve 70-80% effective
BW on Degraded-BLOS versus NewReno's 38%. The reruns measured 70–92% for BBR
and 38% for NewReno, with the same-cap comparison confirming a substantial
throughput advantage for BBR.

**Action taken:** Added a `CongestionAlgo` enum (`NewReno` / `Bbr` / `Cubic`)
to `quelay-quic` (`transport.rs`), wired through
`quinn::congestion::{NewRenoConfig, BbrConfig, CubicConfig}` and selectable
at runtime via `--congestion` on `quelay-agent`. Re-ran Degraded-BLOS
(750ms RTT, 5% loss, 1% corrupt, 3% dup, 200 Kbps ARL cap on an 800 Kbit/s
link) with each algorithm — four bidirectional transfers per algorithm.

#### Algorithm comparison (Degraded-BLOS, 200 Kbps cap)

| Algorithm | BW Utilization | Congestion events | CWND | RTT (Quinn) | Wire efficiency |
|:----------|:----------------|:-------------------|:-----|:------------|:------------------|
| NewReno (baseline)¹ | 38% | window collapses instead of counting events | 3000 KiB → collapses to 20–41 KiB | 0 ms (bug) | not measured |
| Cubic | 92–112% | 0–23 per transfer | 27–66 KiB | 753–849 ms | 0.868–0.939 |
| **BBR** | **70–92%** | **0** | **1.3–2.4 MiB, stable** | 0 ms (bug, now narrowed — see [#2](#2-resolve-rtt-reporting-bbr-specific)) | 0.935–0.936 |

¹ The 38% figure is from the original test session above, which used a
different bandwidth-cap configuration than the 200 Kbps-cap reruns used for
Cubic/BBR, so treat the percentage as directional rather than a strict
apples-to-apples comparison. The PR #28 commit re-ran NewReno under the
*same* 200 Kbps-cap conditions as BBR/Cubic and measured 12–17 kBps
throughput vs BBR's 31–74 kBps — a 2–5× improvement, consistent with the
qualitative gap in the table (window collapse vs. stable CWND).

Sample logs:
[`sample-logs/Degraded-BLOS-200Kbps.txt`](../sample-logs/Degraded-BLOS-200Kbps.txt) (BBR),
[`sample-logs/Degraded-BLOS-200Kbps-cubic.txt`](../sample-logs/Degraded-BLOS-200Kbps-cubic.txt) (Cubic).

**Reading the comparison:** BBR and Cubic both clearly beat NewReno, but for
different reasons and with different trade-offs. Cubic reaches the highest
raw utilization (up to 111%) but gets there by racing up to the cap and
backing off hard on loss — congestion events and packet loss (up to 7.75% in
one run) come along for the ride, and CWND stays tiny (27–66 KiB) because
it's perpetually recovering from the last backoff. BBR trades a little peak
utilization for consistency: zero congestion events across all four runs,
because it paces to a measured bandwidth/RTT estimate instead of reacting to
loss, and CWND stays large and stable (1.3–2.4 MiB) rather than sawtoothing.
For a satellite link where loss is expected and “fair but occasionally lossy”
matters less than “predictable and not spiraling,” BBR's behavior is the
better fit — this is also why it was separately validated as a good neighbor
under ARL enforcement (102–103% utilization on a clean 1 Mbps-capped link
across 4 runs, i.e. it respects the operator ceiling rather than fighting
it).

**New finding:** narrowing the RTT-always-0ms issue (see Known Issues above).
Cubic and NewReno both report real RTT values under this test; only BBR's
`conn.rtt()` stays at 0ms throughout, pointing at something specific to
quinn's BBR implementation rather than a general instrumentation bug — see
[#2](#2-resolve-rtt-reporting-bbr-specific) below.

**Recommendation:** Make BBR the default `--congestion` choice for
BLOS/Degraded-BLOS deployments given its stability and good-neighbor
behavior; keep NewReno for compatibility and document Cubic as an
alternative for links where BBR's still-open RTT-reporting gap
([#2](#2-resolve-rtt-reporting-bbr-specific)) matters for monitoring.

### 2. Wire Efficiency Metric (PR #28, v0.4.0)

Added **wire efficiency** to the transfer report:

```
wire_efficiency = payload_bytes / udp_tx_bytes
```

Implemented as `wire_bytes_absolute()` on `AggregateRateLimiter`, which
returns the raw session UDP byte counter without rolling-baseline
subtraction (the original `wire_bytes_now()` was reset every ARL tick, ~100ms,
and produced incorrect numbers over a full transfer — since removed).
Observed uplink wire efficiency on Degraded-BLOS (200 Kbps cap): 0.935–0.936
for BBR, 0.868–0.939 for Cubic — roughly 6–13% of wire capacity spent on
retransmits and QUIC framing overhead, directly comparable across algorithms
in the [BBR comparison](#1-bbr-congestion-controller-pr-28-v040) above.

### 3. BW Utilization for Impaired Links (PR #28, v0.4.0)

Replaced the ±10% BW utilization assertion with a mode-aware check: added
`--skip-bw-check` to the `e2e-test` binary (and `scripts/link-sim-test.sh`),
which skips the ±10% assertion while preserving the SHA-256 integrity check.
Clean-link runs keep the original ±10% assertion; impaired-link runs use
`--skip-bw-check` since the CC algorithm is expected to underutilize the cap.

---

## Remaining Future Work

### 1. UDT Evaluation

The legacy FTA system used [UDT](https://udt.sourceforge.io/) — a UDP-based
protocol specifically designed for high-speed, high-BDP data transfer. A Rust
binding exists at [docs.rs/udt](https://docs.rs/udt/latest/udt/).

UDT's congestion control was built for exactly the BLOS SATCOM use case:
- High latency (hundreds of ms RTT)
- Occasional burst loss (solar flares, link outages)
- Shared bandwidth pool with other contractors

**Action:** Evaluate `udt` crate as an alternative transport backend to
`quelay-quic`. Compare throughput on Degraded-BLOS profile.

### 2. Resolve RTT Reporting (BBR-specific)

Identify why `conn.rtt()` returns zero and fix. RTT is a critical metric for
SATCOM link health monitoring.

**Narrowed by PR #28:** this is no longer a general instrumentation bug.
Re-running Degraded-BLOS with algorithm selection (see
[#1](#1-bbr-congestion-controller-pr-28-v040)) shows Cubic and NewReno
both report real RTT values (753–849 ms); only **BBR** connections report
`Duration::ZERO` throughout, including at the end of 40–60s transfers — ruling
out "not enough ACKs yet." Candidates:

- BBR's internal RTT sampling (min-RTT / bandwidth-probe cycle) isn't
  surfaced through the same `Connection::rtt()` path NewReno/Cubic use
- Quinn 0.11.9's BBR implementation has a bug or incomplete RTT wiring

Since BBR is now the recommended algorithm for BLOS links (see
[Completed Investigations #1](#1-bbr-congestion-controller-pr-28-v040)), this
should be prioritized — it's the one case where the best-performing CC
algorithm is also the one without usable RTT telemetry.

---

## References

- [UDT Protocol](https://udt.sourceforge.io/)
- [udt Rust crate](https://docs.rs/udt/latest/udt/)
- [Quinn PathStats](https://docs.rs/quinn/latest/quinn/struct.PathStats.html)
- [RFC 9002: QUIC Loss Detection and Congestion Control](https://www.rfc-editor.org/rfc/rfc9002)
- [BBR Congestion Control](https://datatracker.ietf.org/doc/html/draft-cardwell-iccrg-bbr-congestion-control)
- [TCP over SATCOM performance issues](https://www.rfc-editor.org/rfc/rfc2488)
