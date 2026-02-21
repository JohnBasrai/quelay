/*
 * Copyright (c) 2026 John Basrai
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
// -*- mode: thrift -*-
// Thrift IDL reference:
// https://diwakergupta.github.io/thrift-missing-guide/
// https://thrift.apache.org/docs/idl

namespace cpp quelay

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// IDL version — bump when any interface changes.
const string idl_version  = "2026-Feb-20"
const i8     priority_min = 0
const i8     priority_max = 127

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Observable state of the link to the remote Quelay peer.
enum LinkState {

    /// Attempting to establish or re-establish the QUIC connection.
    Connecting = 0,

    /// Link is up and operating within normal parameters.
    Normal      = 1,

    /// Link is up but experiencing loss or congestion; AIMD is backing off.
    Degraded    = 2,

    /// Link is down. Quelay is spooling streams locally until recovery.
    Failed      = 3,
}

/// Reason a stream terminated abnormally.
///
/// Polyglot clients switch on the code; `reason` in `stream_failed` carries
/// a human-readable detail string for logging and operator displays.
enum FailReason {

    /// Sender closed the write socket before signalling done.
    SenderClosed  = 0,

    /// Link went down and the stream could not be resumed.
    LinkFailed    = 1,

    /// The pending queue was flushed (e.g. shutdown or overflow).
    QueueCleared  = 2,

    /// No data was received within the configured deadline.
    Timeout       = 3,

    /// Catch-all for failures that do not map to a specific code.
    Unknown       = 99,
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// Application-level metadata for a stream.
///
/// Passed by the sender at `stream_start` and forwarded verbatim to the
/// receiver's `stream_started` callback. Quelay never reads or modifies
/// `attrs` — it is owned entirely by the client.
///
/// `size_bytes` enables percent-complete reporting in `stream_progress`.
/// Omit it for open-ended streams (sensors, live feeds, unknown-length data).
///
/// Suggested `attrs` keys (all optional):
///   "filename"     — original file name, used by receiver for storage
///   "sha256"       — hex digest; receiver verifies after `stream_done`
///   "content_type" — MIME type hint
///   "source"       — originating system identifier
struct StreamInfo {

    /// Known size in bytes. Omit if unknown or open-ended.
    1: optional i64        size_bytes,

    /// Open-ended application metadata (all values are strings).
    /// Quelay forwards this map verbatim; it never reads or modifies it.
    2: map<string, string> attrs,
}

/// Snapshot of the transfer queue and active streams.
///
/// `pending` is ordered from next-to-start (index 0, highest priority /
/// longest waiting) to most-recently-enqueued (last index).
///
/// Future extension: a `cancel_stream(uuid)` call will search `pending`
/// by UUID and remove the matching entry.
struct QueueStatus {

    /// Streams currently transferring (queue_position == 0).
    1: i32          active_count,

    /// Maximum concurrent streams allowed (configured at startup).
    2: i32          max_concurrent,

    /// Maximum depth of the pending queue (configured at startup).
    3: i32          max_pending,

    /// UUIDs of queued streams, next-to-start first.
    4: list<string> pending,
}

/// Progress snapshot delivered periodically while a stream is active.
///
/// `size_bytes` and `percent_done` are present only when the sender supplied
/// `StreamInfo.size_bytes` at `stream_start`; both are absent for open-ended
/// streams (sensors, live feeds, unknown-length data).
///
/// Easy to extend later — add throughput, ETA, etc. as optional fields
/// without breaking existing clients.
struct ProgressInfo {

    /// Bytes transferred so far.
    1: i64             bytes_transferred,

    /// Total size in bytes, if known.
    2: optional i64    size_bytes,

    /// Completion percentage (0.0 .. 100.0), if size is known.
    3: optional double percent_done,
}

/// Return value from `stream_start`.
struct StartStreamReturn {

    /// Empty on success; error description on failure.
    /// `queue_position` is -1 when err_msg is non-empty.
    1: string err_msg,

    /// Position in the pending queue.
    ///   0  — stream is active; wait for `stream_started` callback before
    ///        connecting to the ephemeral port.
    ///  >0  — stream is queued; wait for `stream_started` callback.
    ///  -1  — queue is full; request rejected (see err_msg).
    2: i32    queue_position,
}

// ---------------------------------------------------------------------------
// QueLayAgent — command/control service (clients → Quelay)
// ---------------------------------------------------------------------------

/// Command and control interface exposed by the Quelay daemon.
///
/// Clients call this service to start streams, register their callback
/// endpoint, and query link state. All transfer progress and status
/// notifications are delivered asynchronously via `QueLayCallback`.
///
/// TODO: consider Thrift `exception` types for structured error handling
///       once the polyglot client set is known.
service QueLayAgent {

    /// Returns the IDL version the server was compiled with.
    /// Clients should verify this matches their own `idl_version` constant.
    string get_version(),

    /// Start a new outbound stream.
    ///
    /// The caller must supply a UUID. The recommended pattern is to generate
    /// it at the outermost system boundary so it serves as a primary key
    /// correlating the job across all services and both ends of the link.
    ///
    /// The caller must not connect to any port until `stream_started` fires
    /// on their registered callback endpoint.
    ///
    /// Returns queue_position == -1 when the pending queue is full.
    StartStreamReturn stream_start(
        1: string     uuid,
        2: StreamInfo info,
        3: i8         priority),

    /// Register the callback endpoint for this client.
    ///
    /// Quelay connects to this address and invokes `QueLayCallback` methods
    /// for all asynchronous notifications (both send and receive events).
    /// If the endpoint is unreachable Quelay logs the error and marks the
    /// client as uncallable until a valid endpoint is registered.
    /// Returns an empty string on success, or an error description.
    string set_callback(1: string endpoint),

    /// Returns the current link state.
    ///
    /// Useful at startup to obtain link state before any callbacks have fired.
    /// Ongoing state changes are delivered via `QueLayCallback::link_status_update`.
    LinkState get_link_state(),
}

// ---------------------------------------------------------------------------
// QueLayCallback — async notification service (Quelay → clients)
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
service QueLayCallback {

    /// Liveness probe — the only blocking call in this service.
    ///
    /// Quelay sends this every 60 seconds. If the call does not return within
    /// a reasonable timeout the client is considered dead, the callback socket
    /// is closed, and `Option<client>` is set to `None` until the client
    /// re-registers via `set_callback`.
    void ping(),

    /// Fired when a stream becomes active (reaches the head of the queue).
    ///
    /// `port` is the ephemeral TCP port Quelay has opened for this stream.
    /// The client connects to 127.0.0.1:`port` to begin I/O.
    ///
    /// `info` is forwarded verbatim from the sender's `stream_start` call,
    /// giving the receiver full access to size and metadata without any
    /// additional round-trip.
    ///
    /// Fired on both sender and receiver sides; clients distinguish by
    /// whether they initiated the stream_start or not.
    oneway void stream_started(
        1: string     uuid,
        2: StreamInfo info,
        3: i32        port),

    /// Periodic progress update while a stream is active.
    ///
    /// `progress.percent_done` is present only when the sender supplied
    /// `StreamInfo.size_bytes` at `stream_start`; absent for open-ended streams.
    oneway void stream_progress(
        1: string       uuid,
        2: ProgressInfo progress),

    /// Fired when a stream completes successfully.
    ///
    /// Sender side: fired when the client closes the write socket (EOF sent).
    /// Receiver side: fired when QUIC stream read returns EOF.
    /// Quelay closes the ephemeral port after this call returns.
    /// Receivers should verify `info.attrs["sha256"]` if the sender
    /// populated that field.
    oneway void stream_done(
        1: string uuid,
        2: i64    bytes_transferred),

    /// Fired when a stream terminates abnormally.
    ///
    /// Fired on both sides of the wormhole: the receiver fires it locally
    /// when a write to the client socket fails, and also sends a WormholeMsg
    /// back across the QUIC stream so the sender agent can fire it on the
    /// sender side.
    ///
    /// `code` allows polyglot clients to switch on the failure type.
    /// `reason` carries a human-readable detail string for logging.
    oneway void stream_failed(
        1: string     uuid,
        2: FailReason code,
        3: string     reason),

    /// Fired on link state changes (Connecting, Normal, Degraded, Failed).
    ///
    /// Clients can use this to gate writes, alert operators, or feed
    /// observability tooling (Prometheus, structured logs).
    oneway void link_status_update(
        1: LinkState link_state),

    /// Fired on every enqueue or dequeue event.
    ///
    /// Provides a full snapshot of active and pending streams so clients
    /// can drive status UIs or feed metrics without polling.
    oneway void queue_status_update(
        1: QueueStatus queue_status),
}
