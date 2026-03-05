use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use uuid::Uuid;

use super::Priority;
use super::Result;

// ---------------------------------------------------------------------------
// LinkState
// ---------------------------------------------------------------------------

/// Observable state of the underlying link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    // ---
    /// Attempting to establish or re-establish the connection.
    Connecting,

    /// Link is up and operating within normal parameters.
    Normal,

    /// Link is up but experiencing loss or degradation; AIMD is backing off.
    Degraded,

    /// Link is down. Quelay is spooling data locally until recovery.
    Failed,
}

// ---------------------------------------------------------------------------
// QueLayStream
// ---------------------------------------------------------------------------

/// A single logical stream over the transport.
///
/// Implements [`AsyncRead`] + [`AsyncWrite`] so all higher layers
/// (scheduler, spooler, progress tracker) are transport-agnostic.
///
/// A stream's [`Uuid`] is stable across link reconnections, allowing
/// the session layer to resume from the last acknowledged byte.
///
/// `#[async_trait]` is required here so that `finish` and `reset` are
/// dyn-compatible, allowing `QueLayStreamPtr = Box<dyn QueLayStream>` to compile.
#[async_trait]
pub trait QueLayStream: AsyncRead + AsyncWrite + Send + Unpin {
    // ---
    /// Stable identifier for this stream. Survives reconnection.
    fn stream_id(&self) -> Uuid;

    /// Signal end-of-write to the remote side (FIN).
    ///
    /// The read half remains open; either side may call `finish()`
    /// independently. Returns [`QueLayError::AlreadyFinished`] if called
    /// more than once.
    async fn finish(&mut self) -> Result<()>;

    /// Abort the stream immediately with an application error code.
    ///
    /// All handles on both sides return an error after this call.
    /// This is the transport primitive behind `terminate(stream_id)`.
    async fn reset(&mut self, code: u64) -> Result<()>;
}

// ---

/// Convenience type alias for a heap-allocated [`QueLayStream`].
pub type QueLayStreamPtr = Box<dyn QueLayStream>;

// Blanket impl so `Box<dyn QueLayStream>` can itself be used as a
// `QueLayStream`.  This allows `BandwidthGate<QueLayStreamPtr>` to satisfy
// the `S: QueLayStream` bound in its own `QueLayStream` impl, enabling
// the session manager to box it as a new `QueLayStreamPtr`.
#[async_trait]
impl QueLayStream for Box<dyn QueLayStream> {
    // ---
    fn stream_id(&self) -> Uuid {
        (**self).stream_id()
    }

    async fn finish(&mut self) -> Result<()> {
        (**self).finish().await
    }

    async fn reset(&mut self, code: u64) -> Result<()> {
        (**self).reset(code).await
    }
}

// ---------------------------------------------------------------------------
// ConnStats
// ---------------------------------------------------------------------------

/// Cumulative QUIC connection statistics for one session.
///
/// All counters reset to zero when a new session is established (reconnect).
/// Take a snapshot before and after a transfer and subtract to get
/// per-transfer deltas.
///
/// Transport implementations that do not track these counters (e.g. mock or
/// loopback transports) should return a zeroed `ConnStats`.  Zero values
/// disable the corresponding reporting in callers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnStats {
    // ---
    /// Total packets sent, including retransmits.
    pub sent_packets: u64,

    /// Packets declared lost by QUIC loss detection.
    pub lost_packets: u64,

    /// Number of congestion events (ECN CE marks or loss-based triggers).
    pub congestion_events: u64,
}

// ---

/// Convenience type alias for a heap-allocated [`QueLaySession`].
///
/// `Arc` (rather than `Box`) allows the accept loop and reconnect loop to
/// hold concurrent references to the live session without cloning the
/// underlying connection object.
pub type QueLaySessionPtr = Arc<dyn QueLaySession>;

// ---------------------------------------------------------------------------
// QueLaySession
// ---------------------------------------------------------------------------

/// A logical session between two Quelay endpoints.
///
/// Survives link outages: when the underlying QUIC connection drops,
/// the session layer reconnects transparently and maps in-flight streams
/// back to their UUIDs via the spool.
#[async_trait]
pub trait QueLaySession: Send + Sync {
    // ---
    /// Open a new outbound stream.
    ///
    /// Priority is recorded by the DRR scheduler above the transport;
    /// the transport trait itself does not interpret it.
    async fn open_stream(&self, priority: Priority) -> Result<QueLayStreamPtr>;

    /// Block until an inbound stream arrives, or return
    /// [`QueLayError::SessionClosed`] if the session has ended.
    async fn accept_stream(&self) -> Result<QueLayStreamPtr>;

    /// Current snapshot of link state.
    fn link_state(&self) -> LinkState;

    /// Subscribe to link state changes.
    ///
    /// Use [`watch::Receiver::changed()`] to await each transition.
    fn link_state_rx(&self) -> watch::Receiver<LinkState>;

    /// Cumulative UDP bytes transmitted on the wire since this session was
    /// established, **including retransmits**.
    ///
    /// The [`AggregateRateLimiter`] samples this each tick and deducts the
    /// retransmit overhead (wire delta minus payload delta) from its carry-over
    /// budget, enforcing a true wire-level bandwidth cap rather than a
    /// payload-only cap.
    ///
    /// This counter resets to zero each time a new session is established
    /// (reconnect).  Callers must reset their sampling baseline via
    /// [`AggregateRateLimiter::reset_wire_baseline`] immediately after
    /// installing a new session.
    ///
    /// Transport implementations that do not track wire-level retransmits
    /// (e.g. mock or loopback transports used in unit tests) should return `0`.
    /// Returning `0` disables the retransmit-overhead deduction, which is
    /// correct for lossless transports.
    fn wire_bytes_sent(&self) -> u64;

    /// Snapshot of cumulative QUIC connection statistics.
    ///
    /// All counters reset to zero on each new session (reconnect).
    /// Callers wanting per-transfer deltas should snapshot before and after
    /// the transfer and subtract.
    ///
    /// Transport implementations that do not track these counters should
    /// return `ConnStats::default()`.
    fn conn_stats(&self) -> ConnStats;

    /// Close the session gracefully, finishing all open streams.
    async fn close(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// QueLayTransport
// ---------------------------------------------------------------------------

/// Factory trait for creating Quelay sessions.
///
/// Implementations: `quelay_quic::QuicTransport`.
#[async_trait]
pub trait QueLayTransport: Send + Sync {
    // ---
    type Session: QueLaySession + 'static;

    /// Connect to a remote Quelay endpoint and return a live session.
    async fn connect(&self, remote: SocketAddr) -> Result<Self::Session>;

    /// Bind and listen for incoming sessions.
    ///
    /// Returns a receiver that yields one [`QueLaySession`] per incoming
    /// connection. Use `tokio::sync::mpsc::Receiver::recv()` to iterate.
    async fn listen(&self, bind: SocketAddr) -> Result<tokio::sync::mpsc::Receiver<Self::Session>>;
}
