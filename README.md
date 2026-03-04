# Quelay

A quelay in Rust, using QUIC as the transport layer, with support for both file
transfers and open-ended streams of unknown length.

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.

---

## Crates

| Crate            | Description |
|:-----------------|:------------|
| `quelay-domain`  | Domain model: transport traits, DRR scheduler, priority types, session / handler interfaces |
| `quelay-quic`    | QUIC transport via `quinn` |
| `quelay-thrift`  | Apache Thrift C2I service stubs |
| `quelay-agent`   | Deployable relay daemon — data pump, `SessionManager`, rate limiter, reconnect |
| `quelay-example` | Demonstrating clients and Docker healthcheck probe |

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

---

## Building

```bash
cargo build --workspace
cargo test --workspace
```

---

## Network Impairment Testing

`scripts/link-sim-test.sh` runs the full link simulation test suite using
Docker Compose. Two `quelay-agent` containers communicate over an isolated
`quic-net` bridge; network impairment is applied by
[Pumba](https://github.com/alexei-led/pumba) on that bridge. No host kernel
namespaces, `veth` pairs, or `sudo` required.

```
┌─────────────────────────────────────────────┐
│  Docker Compose                             │
│                                             │
│  ┌────────────┐   quic-net   ┌───────────┐  │
│  │agent-client│◄────────────►│agent-serv │  │
│  └─────┬──────┘    Pumba     └─────┬─────┘  │
│        │          impairs          │        │
│ c2i-net│                   c2i-net │        │
│        └──────────┬───────────┘    │        │
│               ┌───┴───┐            │        │
│               │ e2e   │────────────┘        │
│               └───────┘                     │
└─────────────────────────────────────────────┘
```

```bash
# 5% packet loss
./scripts/link-sim-test.sh loss

# 600ms delay ±100ms jitter
./scripts/link-sim-test.sh delay

# Loss + delay combined
./scripts/link-sim-test.sh both --loss-percent 5 --delay-ms 300

# Link bandwidth cap at 2mbit (agent cap must be ≤ link cap)
./scripts/link-sim-test.sh rate --rate 2mbit --bw-cap 1Mbps --size-mb 2

# Baseline — no impairment
./scripts/link-sim-test.sh clean
```

Requires Docker with Compose v2. See
[`quelay-agent/README.md`](quelay-agent/README.md#network-impairment-testing)
for the full option reference.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
