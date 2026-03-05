# Quelay

A quelay in Rust, using QUIC as the transport layer, with support for both file
transfers and open-ended streams of unknown length.

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.

---

## Crates

| Crate            | Description                                                                                 |
|:-----------------|:--------------------------------------------------------------------------------------------|
| `quelay-domain`  | Domain model: transport traits, DRR scheduler, priority types, session / handler interfaces |
| `quelay-quic`    | QUIC transport via `quinn`                                                                  |
| `quelay-thrift`  | Apache Thrift C2I service stubs                                                             |
| `quelay-agent`   | Deployable relay daemon — data pump, `SessionManager`, rate limiter, reconnect              |
| `quelay-example` | Demonstrating clients and Docker healthcheck probe                                          |

---

## Architecture

```
External clients (Rust / C++ / Python)
            │
            │← quelay-thrift / Apache Thrift C2I
            │
    SessionManager (reconnection, in-memory spool, pending queue)
            │
    DRR Scheduler  ←→  RateLimiter (timer-task BW cap)
            │
    QueLayTransport trait  (quelay-domain)
     └── quelay-quic       (production — QUIC over UDP)
```

---

## Key Design Decisions

**QUIC over UDP** — The shared satellite link environment prohibits TCP.
TCP's congestion control competes unpredictably with other tenants sharing
the link. QUIC gives per-stream multiplexing and ordered delivery over UDP.

**Rate cap via `RateLimiter`** — Each uplink stream is metered by a dedicated
timer task that wakes on a computed interval (clamped to 5–100 ms), drains up
to a byte budget from an mpsc queue per tick, and discards unused budget. The
operator configures a hard ceiling (e.g. 2 Mbit/s); QUIC's own congestion
control operates below that ceiling and handles packet-loss backoff
transparently. The rate limiter samples `wire_bytes_sent()` each tick and
deducts retransmit overhead so the cap is enforced at the wire level, not just
the payload level.

**DRR scheduler** — Deficit Round Robin distributes the available budget
fairly across active bulk streams. C2I messages use a strict-priority
queue and are always drained before any bulk stream is scheduled.

**Logical session above QUIC** — If the QUIC connection drops (intermittent link
outages are expected), the session layer reconnects and resumes in-flight streams
from the last ACK\'d byte.

Each uplink stream maintains a fixed-size in-memory `SpoolBuffer` (1 MiB, hardcoded
— moving it to a startup config option is a planned trivial refactor). Three pointers
track progress: `A` (bytes acked by the receiver), `Q` (next byte to write to QUIC),
and `T` (head — next byte written by the TCP reader). On link failure the pump rewinds
to `A` and waits; on reconnect it replays `A..T` on the fresh stream and resumes. Once
the receiver writes data to its client socket and acks back, those bytes are released
from the spool and need never be replayed. If the spool fills (outage longer than the
spool depth), back-pressure is applied to the sender's TCP read socket — the writer
blocks rather than losing data.

**Unified file / stream namespace** — Files and open-ended streams share
the same UUID namespace and priority queue. They are treated identically
internally wherever possible.

**System-level constraint** — Quelay can absorb bursts (a link outage is
equivalent to a burst of backlog), but the long-term average input rate
must be ≤ the allotted bandwidth. This is a system design constraint,
not something Quelay can correct. Client writers do not need to meter
their own output — if the spool fills, back-pressure on the write socket
will block the client automatically.

## Documentation

| Document | Description |
|:---------|:------------|
| [Architecture](docs/contributing/ARCHITECTURE.md)   | Crate structure, layering, spool design, bandwidth management |
| [Quick Start](docs/contributing/QUICK_START.md)     | Build, test, and run in 5 minutes |
| [Testing](docs/contributing/TESTING.md)             | Test strategy, how to run CI locally |
| [Code Style](docs/contributing/CODE_STYLE.md)       | Formatting, EMBP, naming conventions |
| [Local Testing](docs/contributing/LOCAL_TESTING.md) | Running the full CI suite before pushing |
| [quelay-agent](quelay-agent/README.md)              | Daemon CLI reference, TLS, internal structure |
| [e2e_test](quelay-agent/src/bin/README.md)          | Integration test design and subcommand reference |
| [Link Sim Findings](docs/link-sim-findings.md)      | Network impairment test results, architecture, future work |

---

## Building

```bash
cargo build --workspace
cargo test --workspace
```

---

## Network Impairment Testing

`scripts/link-sim-test.sh` runs the link simulation test suite using Docker
Compose. A `link-sim` sidecar container shares the network namespace of
`agent-client` and applies a single `tc netem` qdisc that atomically combines
rate cap, delay, loss, corruption, and duplication — no Pumba required, no
host kernel namespaces, no `sudo`.

Impairment profiles live in `docker/link-sim/profiles/`:

| Profile | Description |
|:--------|:------------|
| `BLOS-750ms` | Clean satellite: 100 kbps uplink, 750 ms RTT |
| `LOS-250ms`  | Line-of-sight: 500 kbps uplink, 250 ms RTT, 10 ms jitter |
| `Degraded-BLOS` | Stressed satellite: 5% loss, 1% corrupt, 3% duplicate, 750 ms RTT |
| `clean` | No impairment — baseline |

```
┌─────────────────────────────────────────────────────┐
│  Docker Compose                                     │
│                                                     │
│  ┌────────────┐    ┌──────────┐  quic-net  ┌──────┐ │
│  │agent-client│◄──►│ link-sim │◄──────────►│agent │ │
│  └─────┬──────┘    │  (netem) │            │-serv │ │
│        │           └──────────┘            └──┬───┘ │
│        │   c2i-net                            │     │
│        └───────────────┬──────────────────────┘     │
│                    ┌───┴───┐                        │
│                    │  e2e  │                        │
│                    └───────┘                        │
└─────────────────────────────────────────────────────┘
```

```bash
# Clean satellite link (750ms RTT, 100kbps uplink)
./scripts/link-sim-test.sh BLOS-750ms --size-mb 10

# Stressed satellite (5% loss, corrupt, duplicate, 750ms RTT)
./scripts/link-sim-test.sh Degraded-BLOS --size-mb 10 --bw-cap 80kbps

# Line-of-sight link (250ms RTT, 500kbps uplink)
./scripts/link-sim-test.sh LOS-250ms --size-mb 10

# Baseline — no impairment
./scripts/link-sim-test.sh clean --size-mb 10
```

Requires Docker with Compose v2. See
[`docs/link-sim-findings.md`](docs/link-sim-findings.md) for test results,
architecture notes, and future work.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
