//! Error types for `quelay-quic`.
//!
//! [`QuicError`] maps quinn's error taxonomy onto Quelay's needs.  Quinn
//! exposes several independent error enums (one per operation type); this
//! module collapses them into a single enum whose variants are named for
//! *what happened at the Quelay level*, not for which quinn type was involved.
//!
//! ## Variant guide for the data pump
//!
//! The uplink and downlink pumps in `quelay-agent` ask two questions:
//! *"is the connection still alive?"* and *"is this a logic error?"*
//!
//! | Variant         | Connection alive? | Pump action          |
//! |-------------------------------|-----|----------------------|
//! | `WriteFailed(ConnectionLost)` | No  | rewind Q→A, full reconnect         |
//! | `ReadFailed(ConnectionLost)`  | No  | full reconnect                     |
//! | `ConnectionLost`              | No  | full reconnect (stream open failed)|
//! | `WriteFailed(Stopped)`        | Yes | abort stream, `stream_failed` cb   |
//! | `ReadFailed(Reset)`           | Yes | abort stream, `stream_failed` cb   |
//! | `ConnectFailed`               | N/A | retry with backoff                 |
//! | `Endpoint`                    | N/A | fatal, abort                       |
//! | `AlreadyFinished`             | Yes | logic error — should not occur     |

use thiserror::Error;

#[derive(Debug, Error)]
pub enum QuicError {
    // ---
    /// Failed to construct the QUIC endpoint.
    ///
    /// Covers socket bind failures, certificate/TLS configuration errors,
    /// and any other problem that prevents the endpoint from starting.
    /// Only seen at process startup — never during an active transfer.
    ///
    /// Stringified from `quinn::EndpointError`, rustls setup errors, and
    /// certificate generation failures.
    #[error("QUIC endpoint error: {0}")]
    Endpoint(String),

    /// Failed to establish a new QUIC connection.
    ///
    /// Seen only during the TLS handshake phase (initial connect or a
    /// reconnect attempt), never on an already-established connection.
    ///
    /// Stringified from `quinn::ConnectError`:
    /// - `EndpointStopping`   — endpoint is shutting down; retry after restart
    /// - `InvalidDnsName`     — bad server name string; configuration error
    /// - `Config`             — invalid transport/TLS configuration
    /// - `TooManyConnections` — unreachable in Quelay's single-connection-per-agent
    ///   architecture; would indicate an internal programming error
    ///
    /// The session manager's reconnect loop retries with exponential backoff.
    #[error("QUIC connect failed: {0}")]
    ConnectFailed(String),

    /// The QUIC connection was lost outside of a stream read or write.
    ///
    /// Returned when `open_stream()` or `accept_stream()` fail because the
    /// connection dropped between stream operations (rather than during an
    /// active read or write).  The session manager treats this identically
    /// to `WriteFailed(ConnectionLost)`: close and reconnect.
    ///
    /// Stringified from `quinn::ConnectionError`.
    #[error("QUIC connection lost: {0}")]
    ConnectionLost(String),

    /// A write to a QUIC stream failed.
    ///
    /// Wraps [`quinn::WriteError`] directly so the pump can match on the
    /// inner variant and choose the correct recovery path:
    ///
    /// - `ConnectionLost(ConnectionError)` — the QUIC connection dropped.
    ///   The primary satellite link-failure case.  Analogous to `EPIPE` on
    ///   a TCP `write(2)` call.  **The connection is gone — rewind Q→A and
    ///   enter the reconnect loop.**
    ///
    /// - `Stopped(VarInt)` — the receiver sent a `STOP_SENDING` frame
    ///   signalling it no longer wants data on this stream.  **The connection
    ///   is still alive** — only this stream is dead.  Fire `stream_failed`
    ///   on the callback and leave the connection alone.  This is the
    ///   receiving side of a future `stream_abort()` call: when either peer
    ///   calls `reset()` on their send half, `Stopped` surfaces here on the
    ///   write side.  The `VarInt` carries an application-defined abort code
    ///   that a future `stream_abort(uuid, code)` API can populate.
    ///
    /// - `ClosedStream` — write was called on an already-finished stream.
    ///   **Logic error in the pump** — `finish()` or `reset()` was already
    ///   called.  Should not occur in correct code.
    ///
    /// - `ZeroRttRejected` — unreachable for Quelay.  We never open 0-RTT
    ///   streams (`Connecting::into_0rtt()` is not used).
    #[error("QUIC write failed: {0}")]
    WriteFailed(#[from] quinn::WriteError),

    /// A read from a QUIC stream failed.
    ///
    /// Wraps [`quinn::ReadError`] directly.  The pump matches on the inner
    /// variant:
    ///
    /// - `ConnectionLost(ConnectionError)` — the QUIC connection dropped.
    ///   **The connection is gone — enter the reconnect loop.**
    ///
    /// - `Reset(VarInt)` — the sender called `reset()` on their send half,
    ///   sending a `RESET_STREAM` frame.  **The connection is still alive**
    ///   — only this stream is dead.  Fire `stream_failed` on the callback.
    ///   In normal Quelay operation this fires on the downlink when the
    ///   uplink pump aborts after its own `WriteFailed`.  The `VarInt` abort
    ///   code is the future hook for a `stream_abort(uuid, code)` API — a
    ///   client-initiated abort will set a well-known code here.
    ///
    /// - `ClosedStream` — read was called on an already-closed stream.
    ///   **Logic error in the pump.**  Should not occur.
    ///
    /// - `IllegalOrderedRead` — unreachable for Quelay.  We never mix
    ///   ordered and unordered reads on the same stream.
    ///
    /// - `ZeroRttRejected` — unreachable for Quelay.  Same reason as
    ///   `WriteFailed::ZeroRttRejected`.
    #[error("QUIC read failed: {0}")]
    ReadFailed(#[from] quinn::ReadError),

    /// [`finish()`](quelay_domain::QueLayStream::finish) was called on a
    /// stream that was already finished.
    ///
    /// Logic error in the pump — `finish()` must be called exactly once.
    #[error("stream already finished")]
    AlreadyFinished,
}

// ---------------------------------------------------------------------------
// Bridge to quelay_domain::QueLayError
// ---------------------------------------------------------------------------

impl From<QuicError> for quelay_domain::QueLayError {
    // ---
    fn from(e: QuicError) -> Self {
        quelay_domain::QueLayError::Transport(e.to_string())
    }
}
