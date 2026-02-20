use std::collections::HashMap;

use async_trait::async_trait;
use uuid::Uuid;

use super::error::Result;
use super::priority::Priority;
use super::transport::LinkState;

// ---------------------------------------------------------------------------
// StreamInfo
// ---------------------------------------------------------------------------

/// Application-level metadata for a stream.
///
/// Mirrors `StreamInfo` in the Thrift IDL. Quelay never reads or modifies
/// `attrs` — it is owned entirely by the client and forwarded verbatim.
///
/// Suggested `attrs` keys (all optional):
///   "filename"     — original file name, used by receiver for storage
///   "sha256"       — hex digest; receiver verifies after `stream_done`
///   "content_type" — MIME type hint
///   "source"       — originating system identifier
#[derive(Debug, Clone)]
pub struct StreamInfo {
    // ---
    /// Known size in bytes. `None` for open-ended or unknown-length streams.
    /// When present, enables `percent_done` in progress callbacks.
    pub size_bytes: Option<u64>,

    /// Open-ended application metadata. Quelay forwards this verbatim.
    pub attrs: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// StreamMeta
// ---------------------------------------------------------------------------

/// Internal metadata Quelay tracks for every active or pending stream.
#[derive(Debug, Clone)]
pub struct StreamMeta {
    // ---
    /// Stable identifier. Persists across reconnections.
    pub uuid: Uuid,

    /// Priority at which this stream was opened.
    pub priority: Priority,

    /// Application-supplied metadata, forwarded verbatim to the receiver.
    pub info: StreamInfo,
}

// ---------------------------------------------------------------------------
// TransferProgress
// ---------------------------------------------------------------------------

/// Progress snapshot for an active stream.
#[derive(Debug, Clone)]
pub struct TransferProgress {
    // ---
    pub uuid: Uuid,

    pub bytes_transferred: u64,

    /// `Some(0.0 ..= 100.0)` when `size_bytes` is known; `None` otherwise.
    pub percent_done: Option<f64>,

    /// Observed throughput in bytes per second over a recent window.
    pub throughput_bps: f64,
}

// ---------------------------------------------------------------------------
// QueueStatus
// ---------------------------------------------------------------------------

/// Snapshot of the transfer queue and active streams.
///
/// `pending` is ordered from next-to-start (index 0, highest priority /
/// longest waiting) to most-recently-enqueued (last index).
///
/// Future extension: a `cancel_stream(uuid)` call will search `pending`
/// by UUID and remove the matching entry.
#[derive(Debug, Clone)]
pub struct QueueStatus {
    // ---
    /// Streams currently transferring.
    pub active_count: i32,

    /// Maximum concurrent streams allowed (configured at startup).
    pub max_concurrent: i32,

    /// Maximum depth of the pending queue (configured at startup).
    pub max_pending: i32,

    /// UUIDs of queued streams, next-to-start first.
    pub pending: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// QueLayHandler
// ---------------------------------------------------------------------------

/// Application callback interface.
///
/// Implement this to receive inbound streams and async event notifications
/// from the Quelay session manager. All methods have default no-op
/// implementations; implementors only override what they need.
#[async_trait]
pub trait QueLayHandler: Send + Sync {
    // ---
    /// Called when a stream becomes active (reaches the head of the queue).
    ///
    /// `port` is the ephemeral TCP port the client connects to for I/O.
    /// Sender connects and writes; receiver connects and reads.
    /// Clients look up `uuid` in their own state to determine handling.
    async fn on_stream_started(&self, uuid: Uuid, info: StreamInfo, port: u16) -> Result<()> {
        let _ = (uuid, info, port);
        Ok(())
    }

    // ---

    /// Called periodically with progress for each active stream.
    async fn on_progress(&self, progress: TransferProgress) {
        let _ = progress;
    }

    // ---

    /// Called when a stream completes normally.
    async fn on_stream_done(&self, uuid: Uuid, bytes_transferred: u64) {
        let _ = (uuid, bytes_transferred);
    }

    // ---

    /// Called when a stream terminates abnormally.
    async fn on_stream_failed(&self, uuid: Uuid, reason: String) {
        let _ = (uuid, reason);
    }

    // ---

    /// Called on every link state transition.
    async fn on_link_status_update(&self, state: LinkState) {
        let _ = state;
    }

    // ---

    /// Called on every enqueue or dequeue event.
    async fn on_queue_status_update(&self, status: QueueStatus) {
        let _ = status;
    }
}
