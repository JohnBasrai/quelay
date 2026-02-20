use quelay_domain::LinkState;
use uuid::Uuid;

use super::service::{FailReason, QueueStatus, StreamInfo};

// ---------------------------------------------------------------------------
// QueLayCallbackService
// ---------------------------------------------------------------------------

/// Asynchronous notification interface implemented by Quelay clients.
///
/// Quelay connects to the endpoint registered via `set_callback` and calls
/// these methods to deliver transfer events and system status.
///
/// The same callback fires for both outbound (sending) and inbound
/// (receiving) streams. Clients look up per-stream state by `uuid`:
///   - Sender:   connect to `port` and begin writing bytes.
///   - Receiver: connect to `port`, dispatch a receive task, begin reading.
///
/// Will be replaced by Thrift-generated code once the IDL is finalised.
pub trait QueLayCallbackService: Send + Sync {
    // ---
    /// Fired when a stream becomes active (reaches the head of the queue).
    ///
    /// `port` is the ephemeral TCP port Quelay opened for this stream.
    /// The client connects to 127.0.0.1:`port` to begin I/O.
    ///
    /// `info` is forwarded verbatim from the sender's `stream_start` call,
    /// giving the receiver full access to size and metadata without any
    /// additional round-trip.
    fn stream_started(&self, uuid: Uuid, info: StreamInfo, port: u16);

    // ---

    /// Periodic progress update while a stream is active.
    ///
    /// `percent_done` is `Some` only when `info.size_bytes` was supplied
    /// at `stream_start`; `None` for open-ended streams.
    fn stream_progress(&self, uuid: Uuid, bytes_transferred: u64, percent_done: Option<f64>);

    // ---

    /// Fired when a stream completes successfully.
    ///
    /// Sender side: fired when the client closes the write socket (EOF sent).
    /// Receiver side: fired when QUIC read returns EOF.
    /// Quelay closes the ephemeral port after this call returns.
    /// Receivers should verify `info.attrs["sha256"]` if the sender
    /// populated that field.
    fn stream_done(&self, uuid: Uuid, bytes_transferred: u64);

    // ---

    /// Fired when a stream terminates abnormally.
    ///
    /// `code` allows polyglot clients to switch on the failure type.
    /// `reason` carries a human-readable detail string for logging.
    fn stream_failed(&self, uuid: Uuid, code: FailReason, reason: String);

    // ---

    /// Fired on link state changes (Connecting, Normal, Degraded, Failed).
    fn link_status_update(&self, link_state: LinkState);

    // ---

    /// Fired on every enqueue or dequeue event.
    fn queue_status_update(&self, queue_status: QueueStatus);
}
