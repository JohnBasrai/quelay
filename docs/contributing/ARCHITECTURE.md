# Architecture

## Crate Dependency Graph

```
quelay-thrift ──┐
quelay-quic   ──┴──► quelay-domain

quelay-agent   ──► quelay-domain, quelay-thrift, quelay-quic
quelay-example ──► quelay-domain, quelay-thrift, quelay-quic
```

`quelay-domain` has no intra-workspace dependencies. The library crates
(`quelay-thrift`, `quelay-quic`) depend only on `quelay-domain` and never on
each other. `quelay-agent` and `quelay-example` are binaries that compose the
library crates; they sit outside the library dependency graph and are not
published.

## quelay-domain — The Domain Model

Defines the vocabulary of the system. No implementations live here.

| Module      | Contents |
|-------------|----------|
| `error`     | `QueLayError`, `Result<T>` |
| `priority`  | `Priority` enum (`C2I`, `BulkTransfer`) |
| `transport` | `QueLayStream`, `QueLaySession`, `QueLayTransport` traits; `LinkState` |
| `scheduler` | `DrrScheduler` — Deficit Round Robin |
| `session`   | `StreamMeta`, `QueLayHandler`, `TransferProgress`, `Direction` |

`lib.rs` is the EMBP gateway — it declares all modules privately and
re-exports only the public API surface.

## Layering

```
External clients (Rust / C++ / Python)
            │ Apache Thrift C2I
        quelay-thrift service handlers
            │
    SessionManager
    (reconnection, in-memory spool, UUID mapping)
            │
    DrrScheduler  ←→  RateLimiter (timer-task BW cap)
            │
    QueLayTransport trait
     └── QuicTransport  (quelay-quic)
```

## Bandwidth Management

Each uplink stream is metered by a `RateLimiter` — a dedicated timer task that
wakes on a computed interval (clamped to 5–100 ms), drains up to a byte budget
from an mpsc queue, and discards unused budget:

```text
bytes_per_tick = CHUNKS_PER_TICK × CHUNK_SIZE   (target: 8 chunks per tick)
interval_ms    = bytes_per_tick × 1000 / rate_bytes_per_sec
                 (clamped to [5 ms, 100 ms])
budget_bytes   = rate_bytes_per_sec × interval_ms / 1000
```

The operator configures a hard ceiling via `--bw-cap-mbps`. QUIC's built-in
congestion control operates below that ceiling and handles packet-loss backoff
transparently — no application-layer AIMD is needed. The `RateLimiter` survives
link outages: `link_down()` drains and discards queued chunks; `link_up(new_tx)`
installs a fresh write half without reconstructing the limiter.

## Priority and Scheduling

`Priority::C2I` streams are drained via a strict-priority queue before
any `Priority::BulkTransfer` stream is scheduled. Bulk streams are
scheduled fairly by `DrrScheduler` using Deficit Round Robin.

`Priority` is the operator-visible concept. DRR `quantum` is the
internal scheduling weight. The scheduler maps one to the other and
may adjust quanta dynamically (e.g. when streams join or leave).

## Session Resilience

The logical session layer sits above QUIC. When the QUIC connection
drops (link outage), Quelay:

1. Transitions `LinkState` to `Failed`.
2. Pauses active uplink pumps (the rate limiter timer task drains and
   discards queued chunks, then blocks waiting for `LinkUp`).
3. Reconnects to the remote endpoint with exponential backoff (1 s → 30 s cap).
4. Calls `restore_active` — opens a fresh QUIC stream per in-flight uplink,
   writes a `ReconnectHeader` with `replay_from = bytes_acked`, and delivers
   the stream to the pump via channel.  The pump replays `A..T` from its
   `SpoolBuffer` and resumes.
5. Calls `drain_pending` — re-issues streams that were queued while the link
   was down.
6. Notifies the `accept_loop` via `session_restored` so inbound reconnect
   streams from the peer are dispatched to the correct downlink pumps.
7. Transitions `LinkState` back to `Normal`.

The spool is fixed at 1 MiB (`SPOOL_CAPACITY`), sized to cover data buffered in
QUIC plus at most one in-flight packet on the receiver side. Once the receiver acks
data (after writing it to the client socket), those bytes are released from the
spool and never need to be replayed. If the spool fills during a prolonged outage,
back-pressure is applied to the sender's TCP read socket — the writer blocks rather
than losing data. Moving `SPOOL_CAPACITY` to a startup config option is a planned
trivial refactor; disk spooling is deferred until a real-time SIGINT use case
requires non-blocking writers.

## SpoolBuffer — Uplink Spool Design

Each uplink stream maintains a fixed-size in-memory ring buffer (`SpoolBuffer`)
shared between two decoupled sub-tasks:

- **TCP reader** — reads bytes from the local client's ephemeral TCP socket and
  pushes them into the spool (`T` advances). Blocks when the spool is full
  (back-pressure to the sender).
- **QUIC pump** — drains bytes from the spool into the metered QUIC stream
  (`Q` advances). On link failure it rewinds `Q = A` and waits for a fresh stream.
  On reconnect it replays `A..T` on the new stream and resumes.

```text
spool [capacity: SPOOL_CAPACITY = 1 MiB]
[.......................................................]
 ^               ^                                     ^
 A               Q                                     T
 bytes_acked     next_quic_write      head (next TCP write)
```

`A` advances when `WormholeMsg::Ack { bytes_received }` arrives from the
receiver (sent after it writes to its client socket). Acked bytes are released
from the spool immediately — they will never need to be replayed.

Invariants: `A ≤ Q ≤ T`, `T − A ≤ SPOOL_CAPACITY`.

The spool depth is sized to absorb data buffered inside QUIC plus at most one
in-flight packet on the receiver side — estimated at ~512 KiB, rounded up to
1 MiB for headroom. Moving the constant to `config.rs` for startup configuration
is a planned trivial refactor.

## quelay-agent — The Daemon Binary

`quelay-agent` is the deployable process that bridges local Thrift C2I
clients to a remote Quelay peer over QUIC. It covers all layers of the
diagram above: the Thrift service handlers, `SessionManager`, data pumps,
and rate limiter.

The synchronous Thrift handler and the async session loop communicate
through an `mpsc` channel so neither half needs to know about the other's
runtime. Each uplink stream is managed by an `ActiveStream` task that reads
from an ephemeral TCP port, spools bytes via `SpoolBuffer`, and meters them
into the QUIC stream through `RateLimiter`. Downlink streams are the mirror:
the agent accepts QUIC streams, reads chunks, and writes them to an ephemeral
TCP port for the local client.

`quelay-example` provides demos and in-process smoke checks using
`quelay-quic` directly, without requiring a full two-agent setup.

See [`quelay-agent/README.md`](../../quelay-agent/README.md) for
operator usage, CLI reference, TLS/cert-pinning notes, and a detailed
breakdown of the internal structure.

## EMBP

All library crates follow the Explicit Module Boundary Pattern (EMBP):

- `lib.rs` is the gateway and defines the entire public API surface.
- Submodules are declared with `mod` (never `pub mod`).
- Public symbols are hoisted via `pub use` in `lib.rs`.
- Sibling modules import from each other with `super::`.
- External crates import only from the crate root, never drilling past
  the gateway into submodules.
