# Architecture

## Crate Dependency Graph

```
quelay-thrift ──┐
quelay-quic   ──┼──► quelay-domain
quelay-link-sim   ──┘

quelay-agent   ──► quelay-domain, quelay-thrift, quelay-quic
quelay-example ──► quelay-domain, quelay-thrift, quelay-quic, quelay-link-sim
```

`quelay-domain` has no intra-workspace dependencies. The library crates
(`quelay-thrift`, `quelay-quic`, `quelay-link-sim`) depend only on
`quelay-domain` and never on each other. `quelay-agent` and
`quelay-example` are binaries that compose the library crates; they sit
outside the library dependency graph and are not published.

## quelay-domain — The Domain Model

Defines the vocabulary of the system. No implementations live here.

| Module | Contents |
|---|---|
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
    Quelay session manager
    (reconnection, spooling, UUID mapping)
            │
    DrrScheduler  ←→  AIMD Pacer (TODO)
            │
    QueLayTransport trait
     ├── QuicTransport  (quelay-quic)
     └── MockTransport  (quelay-link-sim)
```

## Bandwidth Management

Quelay uses **AIMD within a rate cap**:

- The operator configures a maximum bandwidth (e.g. 20 % of 10 Mbit/s).
- The AIMD pacer (not yet implemented) gates bytes fed to the DRR
  scheduler each tick.
- When the link degrades, AIMD backs off naturally — no operator
  intervention needed.
- QUIC's built-in congestion control is left enabled but the pacer cap
  sits above it.

## Priority and Scheduling

`Priority::C2I` streams are drained via a strict-priority queue before
any `Priority::BulkTransfer` stream is scheduled. Bulk streams are
scheduled fairly by `DrrScheduler` using Deficit Round Robin.

`Priority` is the operator-visible concept. DRR `quantum` is the
internal scheduling weight. The scheduler maps one to the other and
may adjust quanta dynamically (e.g. when streams join or leave).

## Session Resilience *(planned)*

The logical session layer sits above QUIC. When the QUIC connection
drops (link outage), Quelay:

1. Transitions `LinkState` to `Failed`.
2. Spools incoming data locally.
3. Reconnects to the remote endpoint.
4. Resumes each in-flight stream from the last ACK'd byte, identified
   by UUID.
5. Transitions `LinkState` back to `Normal`.

## quelay-agent — The Daemon Binary

`quelay-agent` is the deployable process that bridges local Thrift C2I
clients to a remote Quelay peer over QUIC. It sits at the top two layers
of the diagram above: the Thrift service handlers and the session manager
beneath them. Internally the synchronous Thrift handler and the async
session loop communicate through an `mpsc` channel so neither half needs
to know about the other's runtime.

`quelay-example` exercises the same library crates in-process using
`quelay-link-sim`, without needing two running processes or a real network.

See [`quelay-agent/README.md`](../../quelay-agent/README.md) for
operator usage, CLI reference, TLS/cert-pinning notes, and a detailed
breakdown of the internal structure.

## EMBP

All library crates follow the
[Explicit Module Boundary Pattern](../../EMBP.md):

- `lib.rs` is the gateway and defines the entire public API surface.
- Submodules are declared with `mod` (never `pub mod`).
- Public symbols are hoisted via `pub use` in `lib.rs`.
- Sibling modules import from each other with `super::`.
