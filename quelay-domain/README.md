# quelay-domain

Core traits, types, and DRR scheduler for the Quelay data relay.

This crate defines the vocabulary of the system.  All other crates depend on
`quelay-domain` and speak its types.  **No implementations live here** — it is
a pure interface crate with no dependency on QUIC, Thrift, or any runtime
besides `tokio`.

## Module overview

| Module | Contents |
|:-------|:---------|
| `error` | `QueLayError` and `Result<T>` alias |
| `priority` | `Priority` — strict vs. bulk scheduling tiers |
| `transport` | `QueLayStream`, `QueLaySession`, `QueLayTransport` traits |
| `scheduler` | `DrrScheduler` — Deficit Round Robin bandwidth distribution |
| `session` | `StreamInfo`, `StreamMeta`, `QueLayHandler` callback trait |

## Transport traits

### `QueLayStream`

A single logical data stream.  Implements `AsyncRead + AsyncWrite` so all
higher layers are transport-agnostic.  A stream's `Uuid` is stable across link
reconnections, allowing the session layer to resume from the last acknowledged
byte.

Key methods:
- `stream_id() -> Uuid` — stable across reconnects
- `finish()` — send FIN to remote (write side only)
- `reset(code)` — abort immediately with an application error code

### `QueLaySession`

A logical session between two Quelay endpoints.  Wraps a single underlying
transport connection (e.g. a `quinn::Connection`) and exposes:

- `open_stream(priority)` — open a new outbound stream
- `accept_stream()` — block until an inbound stream arrives
- `link_state() / link_state_rx()` — current link state and change watch
- `wire_bytes_sent() -> u64` — cumulative UDP bytes sent on the wire **including retransmits**
- `close()` — graceful shutdown

#### `wire_bytes_sent`

This method is central to Quelay's wire-level bandwidth enforcement.  The
`AggregateRateLimiter` (ARL) samples it each tick and computes:

    wire_delta      = wire_bytes_sent_now  - wire_bytes_sent_last_tick
    payload_delta   = application payload bytes delivered to pumps this tick
    retransmit_cost = wire_delta.saturating_sub(payload_delta)
    available_budget = available_budget.saturating_sub(retransmit_cost)

Without this deduction, QUIC retransmits under packet loss would silently
consume wire bandwidth beyond the configured cap.

Implementations that do not track wire-level retransmits (mock transports,
loopback) must return `0`.  Returning `0` disables the deduction, which is
correct for lossless transports.

**Baseline reset on reconnect**: The counter resets to zero on each new
connection.  `SessionManager` calls `AggregateRateLimiter::set_session` after
installing a new session so the ARL's sampling baseline is updated atomically.

### `QueLayTransport`

Factory trait for creating sessions.  One implementation: `quelay_quic::QuicTransport`.

- `connect(remote)` — connect and return a live session
- `listen(bind)` — bind and yield incoming sessions via an `mpsc::Receiver`

## Scheduler

`DrrScheduler` implements Deficit Round Robin across registered streams.
Each call to `schedule(budget)` returns `(Uuid, bytes)` allocations summing to
at most `budget`, weighted by priority and backlog.

Priority tiers:
- `Priority::Strict(n)` — always served before bulk streams
- `Priority::Bulk(n)` — DRR scheduling; `n` is the DRR quantum weight

## Usage

    [dependencies]
    quelay-domain = { path = "../quelay-domain" }

Implement `QueLaySession` and `QueLayStream` for your transport, then wire up
`QueLayHandler` for async event callbacks.  See `quelay-quic` for the
reference QUIC implementation.
