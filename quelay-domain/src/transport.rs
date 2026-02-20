use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use uuid::Uuid;

use super::error::Result;
use super::priority::Priority;

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

/// Convenience type alias for a heap-allocated [`QueLaySession`].
pub type QueLaySessionPtr = Box<dyn QueLaySession>;

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

    /// Close the session gracefully, finishing all open streams.
    async fn close(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// QueLayTransport
// ---------------------------------------------------------------------------

/// Factory trait for creating Quelay sessions.
///
/// Implementations: `quelay_quic::QuicTransport`, `quelay_link_sim::LinkSimTransport`.
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
