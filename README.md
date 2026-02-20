# Quelay — Quelay

A quelay in Rust, using QUIC as the transport layer,
with support for both file transfers and open-ended streams of unknown
length.

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.

---

## Crates

| Crate | Description |
|---|---|
| `quelay-domain` | Domain model: transport traits, DRR scheduler, priority types, session / handler interfaces |
| `quelay-link-sim` | In-process mock transport with link simulation (drops, duplication, BW cap, outages) |
| `quelay-quic` | QUIC transport via `quinn` (**stub — not yet implemented**) |
| `quelay-thrift` | Apache Thrift C2I service stubs (**IDL revision pending**) |

---

## Architecture

```
External clients (Rust / C++ / Python)
            │ Apache Thrift C2I
        quelay-thrift
            │
    Quelay session manager (reconnection, spooling)
            │
    DRR Scheduler  ←→  AIMD Pacer (rate cap enforcement)
            │
    QueLayTransport trait  (quelay-domain)
     ├── quelay-quic   (production — QUIC over UDP)
     └── quelay-link-sim   (testing — in-process channels)
```

---

## Key Design Decisions

**QUIC over UDP** — The shared satellite link environment prohibits TCP.
TCP's congestion control competes for bandwidth unpredictably with other
contractors on the link. QUIC gives per-stream multiplexing and ordered
delivery over UDP.

**AIMD within a rate cap** — Quelay adapts to instantaneous link degradation
like TCP, but never exceeds the operator-configured bandwidth allocation
(e.g. 20 % of 10 Mbit/s = 2 Mbit/s ceiling). If the link sags to 5
Mbit/s, AIMD naturally backs off to ~1 Mbit/s without operator
intervention.

**DRR scheduler** — Deficit Round Robin distributes the available budget
fairly across active bulk streams. C2I messages use a strict-priority
queue and are always drained before any bulk stream is scheduled.

**Logical session above QUIC** — If the QUIC connection drops (link
outages up to ~60 s are expected), the session layer reconnects and
resumes all in-flight streams from the last ACK'd byte via the spool.

**Unified file / stream namespace** — Files and open-ended streams share
the same UUID namespace and priority queue. They are treated identically
internally wherever possible.

**System-level constraint** — Quelay can absorb bursts (a link outage is
equivalent to a burst of backlog), but the long-term average input rate
must be ≤ the allotted bandwidth. This is a system design constraint,
not something Quelay can correct.

---

## Building

```bash
cargo build
cargo test
```

---

## Link Simulation (unit tests)

`quelay-link-sim` provides an in-process link simulator. No real sockets needed.

```rust
use quelay_link_sim::{MockTransport, LinkSimConfig};

let (client, server) = MockTransport::new(LinkSimConfig::outage_60s())
    .connected_pair();
```

For system integration tests with real containers, use Linux `tc netem`
on the network interface between containers. No code changes are needed
— `quelay-quic` and `quelay-link-sim` both implement `QueLayTransport`.

```bash
# 5 % packet loss, 200 ms delay on the test interface
tc qdisc add dev eth0 root netem loss 5% delay 200ms 50ms
```

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
