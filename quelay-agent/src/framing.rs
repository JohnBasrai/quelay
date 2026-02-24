//! Wire framing for Quelay streams.
//!
//! ## Stream-open header (8 bytes)
//!
//! Every new or reconnect QUIC stream begins with this fixed preamble
//! followed by a variable-length JSON payload.
//!
//! ```text
//! offset
//!    0  [0x51] magic ('Q')
//!    1  [0x01] version
//!    2  [0x01/0x02] opcode
//!    3  [0x00] reserved (pad)
//!    4  ┐
//!    5  │ payload_len (u32, big-endian)
//!    6  │
//!    7  ┘
//! ------
//!    JSON payload (payload_len bytes, UTF-8, max 65535)
//! ```
//!
//! ## Version
//!
//! Wire format version.  Covers the entire Quelay stream protocol: the 8-byte
//! stream-open preamble, chunk framing, and wormhole message framing.  Bump
//! when any part of the wire format changes — a receiver that knows version
//! current version must understand all three layers to parse a stream
//! correctly.  Older fielded agents will reject unknown versions cleanly via
//! the version check in [`read_stream_open`].
//!
//! ## Opcodes (independent of version)
//!
//! - `OP_NEW_STREAM` (`0x01`): new transfer.  Payload is [`StreamHeader`].
//!   Chunk data follows immediately after the JSON payload.
//!
//! - `OP_RECONNECT` (`0x02`): stream continuation after link outage.
//!   Payload is [`ReconnectHeader`].  The sender replays chunks from
//!   [`ReconnectHeader::replay_from`] (its last-acked spool offset `A`).
//!   The receiver uses its own `bytes_written` as ground truth for duplicate
//!   detection.  If `replay_from > bytes_written` the gap is unrecoverable
//!   and the receiver fails the stream.
//!
//! ## Chunk framing (10 bytes)
//!
//! After the stream-open header, data flows as a sequence of chunks:
//!
//! ```text
//! offset
//!    0  ┐
//!    1  │
//!    2  │ stream_offset (u64, big-endian)
//!    3  │   absolute byte position of first payload byte
//!    4  │
//!    5  │
//!    6  │
//!    7  ┘
//!    8  ┐ payload_len (u16, big-endian, max 65535)
//!    9  ┘
//! ------
//!    payload (payload_len bytes)
//! ```
//!
//! ## WormholeMsg framing
//!
//! Receiver-to-sender feedback (acks, done, error) rides the QUIC stream's
//! read half using the same 8-byte stream-open header format with
//! `opcode = OP_NEW_STREAM`.  Payload is a JSON-encoded [`WormholeMsg`].
//! Payload size is clamped to [`MAX_JSON_PAYLOAD`] on read.

use std::collections::HashMap;

// ---

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

// ---

use quelay_domain::{QueLayError, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic byte — 'Q' for Quelay.  Lets the receiver detect misaligned reads.
pub const MAGIC: u8 = 0x51;

/// Wire format version.  Identifies the 8-byte preamble layout.
/// Bump only when the fixed-header structure itself changes.
pub const VERSION: u8 = 0x01;

/// Opcode: new stream.  Payload is [`StreamHeader`].
pub const OP_NEW_STREAM: u8 = 0x01;

/// Opcode: reconnect after link outage.  Payload is [`ReconnectHeader`].
pub const OP_RECONNECT: u8 = 0x02;

/// Fixed stream-open header size in bytes:
/// magic(1) + version(1) + opcode(1) + pad(1) + payload_len(4).
pub const FIXED_HEADER_LEN: usize = 8;

/// Maximum JSON payload size for stream-open and wormhole messages.
///
/// A 64 KiB cap prevents a malformed or malicious peer from causing an
/// unbounded heap allocation via a crafted `payload_len` field.
/// Legitimate headers and wormhole messages are well under 1 KiB.
pub const MAX_JSON_PAYLOAD: usize = 65_535;

/// Payload size for each data chunk written to the QUIC stream.
///
/// Drives the granularity of the spool and ack system.  Smaller values give
/// finer-grained acks; larger values reduce per-chunk framing overhead.
/// Must fit in a u16 (`<= 65535`).
///
/// TODO: wire to per-stream config.
pub const CHUNK_SIZE: usize = 16 * 1024; // 16 KiB

/// Fixed chunk header size: stream_offset(8) + payload_len(2).
pub const CHUNK_HEADER_LEN: usize = 10;

/// Send a [`WormholeMsg::Ack`] every this many bytes delivered to the client.
pub const ACK_INTERVAL: u64 = 64 * 1024; // 64 KiB

// ---------------------------------------------------------------------------
// StreamHeader
// ---------------------------------------------------------------------------

/// Application-level metadata written at the start of every new Quelay stream.
///
/// Serialized as JSON, preceded by the 8-byte stream-open preamble with
/// `opcode = OP_NEW_STREAM`.  Chunk data follows immediately after.
///
/// Quelay is stream-oriented.  Clients that transfer files should place the
/// file name, checksum, and other file-level metadata in `attrs` rather than
/// a dedicated field.  Common keys: `"filename"`, `"sha256"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    // ---
    /// Stable stream identity.  Survives reconnections.
    pub uuid: Uuid,

    /// Raw priority byte (0 = lowest … 127 = highest).
    pub priority: u8,

    /// Known stream length in bytes.  `None` for open-ended streams.
    /// When present, enables `percent_done` progress callbacks.
    pub size_bytes: Option<u64>,

    /// Open-ended application metadata forwarded verbatim to the receiver.
    /// `HashMap` enforces key uniqueness; serializes as a JSON object.
    /// Suggested keys: `"filename"`, `"sha256"`, `"content_type"`.
    pub attrs: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// ReconnectHeader
// ---------------------------------------------------------------------------

/// Header written at the start of a reconnect stream (`OP_RECONNECT`).
///
/// The sender opens a new QUIC stream after a link outage and writes this
/// before replaying buffered chunks.  The receiver looks up the existing
/// downlink task by `uuid` and delivers the fresh stream to it.
///
/// `replay_from` is the sender's last-acked spool offset (`A`).  The receiver
/// uses its own `bytes_written` as ground truth for duplicate detection.
/// If `replay_from > bytes_written` the sender has freed spool data the
/// receiver never saw — an unrecoverable gap — and the receiver fails the
/// stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectHeader {
    // ---
    /// Stable stream identity — matches the original [`StreamHeader::uuid`].
    pub uuid: Uuid,

    /// Sender's last-acked spool offset (`A`) at time of reconnect.
    /// Advisory: receiver validates against its own `bytes_written`.
    pub replay_from: u64,
}

// ---------------------------------------------------------------------------
// StreamOpen
// ---------------------------------------------------------------------------

/// Decoded result of [`read_stream_open`]: new stream or reconnect.
#[derive(Debug)]
pub enum StreamOpen {
    // ---
    /// New transfer — full metadata in the header.
    New(StreamHeader),

    /// Continuation after link outage — look up the existing stream by UUID.
    Reconnect(ReconnectHeader),
}

// ---------------------------------------------------------------------------
// write_connect_header  (OP_NEW_STREAM)
// ---------------------------------------------------------------------------

/// Serialize `header` and write the 8-byte preamble + JSON payload.
///
/// Writes `opcode = OP_NEW_STREAM`.
/// Errors if the serialized payload exceeds [`MAX_JSON_PAYLOAD`].
pub async fn write_connect_header<W>(stream: &mut W, header: &StreamHeader) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // ---
    let payload = serde_json::to_vec(header)
        .map_err(|e| QueLayError::Transport(format!("framing serialize error: {e}")))?;

    if payload.len() > MAX_JSON_PAYLOAD {
        return Err(QueLayError::Transport(format!(
            "framing payload {} exceeds max {MAX_JSON_PAYLOAD}",
            payload.len()
        )));
    }

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("stream header payload exceeds 4 GiB".into()))?;

    // Fixed header: magic + version + opcode + pad + payload_len (big-endian u32)
    let mut fixed = [0u8; FIXED_HEADER_LEN];
    fixed[0] = MAGIC;
    fixed[1] = VERSION;
    fixed[2] = OP_NEW_STREAM;
    fixed[3] = 0x00; // reserved / pad
    fixed[4..8].copy_from_slice(&payload_len.to_be_bytes());

    stream
        .write_all(&fixed)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing write fixed header: {e}")))?;

    stream
        .write_all(&payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing write payload: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// write_reconnect_header  (OP_RECONNECT)
// ---------------------------------------------------------------------------

/// Serialize `header` and write the 8-byte preamble + JSON payload.
///
/// Writes `opcode = OP_RECONNECT`.  Called by `restore_active` in the sender
/// when opening a fresh QUIC stream to continue an interrupted transfer.
pub async fn write_reconnect_header<W>(stream: &mut W, header: &ReconnectHeader) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // ---
    let payload = serde_json::to_vec(header)
        .map_err(|e| QueLayError::Transport(format!("framing serialize error: {e}")))?;

    if payload.len() > MAX_JSON_PAYLOAD {
        return Err(QueLayError::Transport(format!(
            "framing payload {} exceeds max {MAX_JSON_PAYLOAD}",
            payload.len()
        )));
    }

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("reconnect header payload exceeds 4 GiB".into()))?;

    let mut fixed = [0u8; FIXED_HEADER_LEN];
    fixed[0] = MAGIC;
    fixed[1] = VERSION;
    fixed[2] = OP_RECONNECT;
    fixed[3] = 0x00; // reserved / pad
    fixed[4..8].copy_from_slice(&payload_len.to_be_bytes());

    stream
        .write_all(&fixed)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing write fixed header: {e}")))?;

    stream
        .write_all(&payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing write payload: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// read_stream_open
// ---------------------------------------------------------------------------

/// Read the 8-byte preamble and dispatch on opcode.
///
/// Returns [`StreamOpen::New`] for `OP_NEW_STREAM` or
/// [`StreamOpen::Reconnect`] for `OP_RECONNECT`.
///
/// Errors on bad magic, unsupported version, unknown opcode, payload exceeding
/// [`MAX_JSON_PAYLOAD`], or any I/O or deserialization failure.
pub async fn read_stream_open<R>(stream: &mut R) -> Result<StreamOpen>
where
    R: AsyncRead + Unpin,
{
    // ---
    let mut fixed = [0u8; FIXED_HEADER_LEN];
    stream
        .read_exact(&mut fixed)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing read fixed header: {e}")))?;

    if fixed[0] != MAGIC {
        return Err(QueLayError::Transport(format!(
            "framing bad magic: expected 0x{MAGIC:02X}, got 0x{:02X}",
            fixed[0]
        )));
    }

    if fixed[1] != VERSION {
        return Err(QueLayError::Transport(format!(
            "framing unsupported version: expected 0x{VERSION:02X}, got 0x{:02X}",
            fixed[1]
        )));
    }

    let opcode = fixed[2];
    // fixed[3] is reserved/pad — ignored on read for forward compatibility.
    let payload_len = u32::from_be_bytes(fixed[4..8].try_into().unwrap()) as usize;

    if payload_len > MAX_JSON_PAYLOAD {
        return Err(QueLayError::Transport(format!(
            "framing payload_len {payload_len} exceeds max {MAX_JSON_PAYLOAD}"
        )));
    }

    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing read payload: {e}")))?;

    match opcode {
        OP_NEW_STREAM => {
            let header: StreamHeader = serde_json::from_slice(&payload)
                .map_err(|e| QueLayError::Transport(format!("framing deserialize error: {e}")))?;
            Ok(StreamOpen::New(header))
        }
        OP_RECONNECT => {
            let header: ReconnectHeader = serde_json::from_slice(&payload)
                .map_err(|e| QueLayError::Transport(format!("framing deserialize error: {e}")))?;
            Ok(StreamOpen::Reconnect(header))
        }
        _ => Err(QueLayError::Transport(format!(
            "framing unknown opcode: 0x{opcode:02X}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// WormholeMsg
// ---------------------------------------------------------------------------

/// Messages flowing receiver → sender on the QUIC stream's read half.
///
/// After the stream-open header and chunk data, the receiver sends these
/// framed messages back using the same 8-byte preamble format with
/// `opcode = OP_NEW_STREAM`.  The sender's pump reads them concurrently
/// via a dedicated ack-reader task.
///
/// Frequency: the receiver sends a [`WormholeMsg::Ack`] every
/// [`ACK_INTERVAL`] bytes written to its client socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WormholeMsg {
    // ---
    /// Periodic acknowledgement — receiver has written this many bytes to its
    /// client socket.  Sender advances spool `A` and frees buffer space.
    Ack { bytes_received: u64 },

    /// Receiver finished writing all bytes successfully (sent after QUIC FIN).
    Done,

    /// Receiver encountered a fatal error.
    ///
    /// `code` maps to [`quelay_thrift::FailReason`] wire values.
    Error { code: u32, reason: String },
}

// ---

/// Serialize `msg` and write it as a framed wormhole message to `stream`.
///
/// Uses the same 8-byte preamble as stream-open messages with
/// `opcode = OP_NEW_STREAM`.
pub async fn write_wormhole_msg<W>(stream: &mut W, msg: &WormholeMsg) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // ---
    let payload = serde_json::to_vec(msg)
        .map_err(|e| QueLayError::Transport(format!("wormhole serialize error: {e}")))?;

    if payload.len() > MAX_JSON_PAYLOAD {
        return Err(QueLayError::Transport(format!(
            "wormhole payload {} exceeds max {MAX_JSON_PAYLOAD}",
            payload.len()
        )));
    }

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("wormhole payload exceeds 4 GiB".into()))?;

    let mut fixed = [0u8; FIXED_HEADER_LEN];
    fixed[0] = MAGIC;
    fixed[1] = VERSION;
    fixed[2] = OP_NEW_STREAM; // wormhole msgs ride on an established stream
    fixed[3] = 0x00; // reserved / pad
    fixed[4..8].copy_from_slice(&payload_len.to_be_bytes());

    stream
        .write_all(&fixed)
        .await
        .map_err(|e| QueLayError::Transport(format!("wormhole write fixed header: {e}")))?;

    stream
        .write_all(&payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("wormhole write payload: {e}")))?;

    Ok(())
}

// ---

/// Read and deserialize a [`WormholeMsg`] from `stream`.
///
/// Errors on bad magic, unsupported version, payload exceeding
/// [`MAX_JSON_PAYLOAD`], or deserialization failure.
pub async fn read_wormhole_msg<R>(stream: &mut R) -> Result<WormholeMsg>
where
    R: AsyncRead + Unpin,
{
    // ---
    let mut fixed = [0u8; FIXED_HEADER_LEN];
    stream
        .read_exact(&mut fixed)
        .await
        .map_err(|e| QueLayError::Transport(format!("wormhole read fixed header: {e}")))?;

    if fixed[0] != MAGIC {
        return Err(QueLayError::Transport(format!(
            "wormhole bad magic: expected 0x{MAGIC:02X}, got 0x{:02X}",
            fixed[0]
        )));
    }

    if fixed[1] != VERSION {
        return Err(QueLayError::Transport(format!(
            "wormhole unsupported version: expected 0x{VERSION:02X}, got 0x{:02X}",
            fixed[1]
        )));
    }

    // fixed[2] = opcode, fixed[3] = pad — not inspected for wormhole msgs.
    let payload_len = u32::from_be_bytes(fixed[4..8].try_into().unwrap()) as usize;

    if payload_len > MAX_JSON_PAYLOAD {
        return Err(QueLayError::Transport(format!(
            "wormhole payload_len {payload_len} exceeds max {MAX_JSON_PAYLOAD}"
        )));
    }

    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("wormhole read payload: {e}")))?;

    let msg: WormholeMsg = serde_json::from_slice(&payload)
        .map_err(|e| QueLayError::Transport(format!("wormhole deserialize error: {e}")))?;

    Ok(msg)
}

// ---------------------------------------------------------------------------
// Chunk framing
// ---------------------------------------------------------------------------

/// Write one chunk to `stream`.
///
/// Frame layout (10-byte header):
///
/// ```text
/// offset
///    0  ┐
///    1  │
///    2  │ stream_offset (u64, big-endian)
///    3  │   absolute byte position of first payload byte
///    4  │
///    5  │
///    6  │
///    7  ┘
///    8  ┐ payload_len (u16, big-endian, max 65535)
///    9  ┘
/// ------
///    payload (payload_len bytes)
/// ```
///
/// `stream_offset` lets the receiver detect duplicate chunks replayed after
/// reconnect (offset < `bytes_written`) and assert in-order delivery
/// (gap = logic error).
pub async fn write_chunk<W>(stream: &mut W, stream_offset: u64, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // ---
    let payload_len = u16::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("chunk payload exceeds 65535 bytes".into()))?;

    let mut hdr = [0u8; CHUNK_HEADER_LEN];
    hdr[0..8].copy_from_slice(&stream_offset.to_be_bytes());
    hdr[8..10].copy_from_slice(&payload_len.to_be_bytes());

    stream
        .write_all(&hdr)
        .await
        .map_err(|e| QueLayError::Transport(format!("chunk write header: {e}")))?;

    stream
        .write_all(payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("chunk write payload: {e}")))?;

    Ok(())
}

// ---

/// One decoded chunk read from the QUIC stream.
#[derive(Debug)]
pub struct Chunk {
    // ---
    /// Absolute byte offset of the first payload byte in the logical stream.
    pub stream_offset: u64,

    /// Payload bytes.
    pub payload: Vec<u8>,
}

// ---

/// Read one chunk from `stream`.
///
/// Returns `None` on clean EOF (FIN from sender), `Some(Chunk)` otherwise.
/// Errors if the payload length field exceeds [`CHUNK_SIZE`] (guards against
/// heap exhaustion from a malformed peer) or on any I/O failure.
pub async fn read_chunk<R>(stream: &mut R) -> Result<Option<Chunk>>
where
    R: AsyncRead + Unpin,
{
    // ---
    let mut hdr = [0u8; CHUNK_HEADER_LEN];

    // Peek at the first byte to distinguish clean EOF from a real header.
    match stream.read(&mut hdr[..1]).await {
        Ok(0) => return Ok(None), // clean EOF / FIN
        Ok(_) => {}
        Err(e) => return Err(QueLayError::Transport(format!("chunk read header[0]: {e}"))),
    }

    // Read the remaining 9 header bytes.
    stream
        .read_exact(&mut hdr[1..])
        .await
        .map_err(|e| QueLayError::Transport(format!("chunk read header[1..]: {e}")))?;

    let stream_offset = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
    let payload_len = u16::from_be_bytes(hdr[8..10].try_into().unwrap()) as usize;

    if payload_len > CHUNK_SIZE {
        return Err(QueLayError::Transport(format!(
            "chunk payload_len {payload_len} exceeds CHUNK_SIZE {CHUNK_SIZE}"
        )));
    }

    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("chunk read payload: {e}")))?;

    Ok(Some(Chunk {
        stream_offset,
        payload,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // ---
    use std::io::Cursor;

    use tokio::io::BufReader;

    use super::*;

    // ---

    /// Round-trip a new-stream header through write_connect_header / read_stream_open.
    #[tokio::test]
    async fn round_trip_new_stream() {
        // ---
        let original = StreamHeader {
            uuid: Uuid::new_v4(),
            priority: 64,
            size_bytes: Some(1_048_576),
            attrs: [
                ("filename".into(), "telemetry.bin".into()),
                ("content_type".into(), "application/octet-stream".into()),
            ]
            .into(),
        };

        let mut buf: Vec<u8> = Vec::new();
        write_connect_header(&mut buf, &original).await.unwrap();

        // Append raw bytes to confirm only the header is consumed.
        buf.extend_from_slice(b"raw-data");

        let mut reader = BufReader::new(Cursor::new(buf));
        let open = read_stream_open(&mut reader).await.unwrap();
        let recovered = match open {
            StreamOpen::New(h) => h,
            other => panic!("expected New, got {other:?}"),
        };

        assert_eq!(recovered.uuid, original.uuid);
        assert_eq!(recovered.priority, original.priority);
        assert_eq!(recovered.size_bytes, original.size_bytes);
        assert_eq!(recovered.attrs["filename"], "telemetry.bin");
        assert_eq!(recovered.attrs["content_type"], "application/octet-stream");

        // Confirm raw data is still available after the header is consumed.
        let mut tail = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut tail)
            .await
            .unwrap();
        assert_eq!(tail, b"raw-data");
    }

    // ---

    /// Round-trip a reconnect header through write_reconnect_header / read_stream_open.
    #[tokio::test]
    async fn round_trip_reconnect() {
        // ---
        let original = ReconnectHeader {
            uuid: Uuid::new_v4(),
            replay_from: 524_288,
        };

        let mut buf: Vec<u8> = Vec::new();
        write_reconnect_header(&mut buf, &original).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let open = read_stream_open(&mut reader).await.unwrap();
        let recovered = match open {
            StreamOpen::Reconnect(h) => h,
            other => panic!("expected Reconnect, got {other:?}"),
        };

        assert_eq!(recovered.uuid, original.uuid);
        assert_eq!(recovered.replay_from, original.replay_from);
    }

    // ---

    /// Payload at the limit is accepted; one byte over is rejected before allocation.
    #[tokio::test]
    async fn payload_size_clamped() {
        // ---
        // Exactly at the limit — passes the size check (fails JSON parse, not size).
        let len = MAX_JSON_PAYLOAD as u32;
        let mut buf = vec![MAGIC, VERSION, OP_NEW_STREAM, 0x00];
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend(std::iter::repeat_n(b' ', MAX_JSON_PAYLOAD));

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_stream_open(&mut reader).await.unwrap_err();
        assert!(
            err.to_string().contains("deserialize"),
            "unexpected error: {err}"
        );

        // One byte over — rejected before allocation.
        let over = MAX_JSON_PAYLOAD as u32 + 1;
        let mut buf2 = vec![MAGIC, VERSION, OP_NEW_STREAM, 0x00];
        buf2.extend_from_slice(&over.to_be_bytes());

        let mut reader2 = BufReader::new(Cursor::new(buf2));
        let err2 = read_stream_open(&mut reader2).await.unwrap_err();
        assert!(
            err2.to_string().contains("exceeds max"),
            "unexpected error: {err2}"
        );
    }

    // ---

    #[tokio::test]
    async fn bad_magic_rejected() {
        // ---
        let buf = vec![0xFFu8, VERSION, OP_NEW_STREAM, 0x00, 0, 0, 0, 2, b'{', b'}'];
        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_stream_open(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("bad magic"));
    }

    // ---

    #[tokio::test]
    async fn unknown_opcode_rejected() {
        // ---
        let buf = vec![MAGIC, VERSION, 0xFFu8, 0x00, 0, 0, 0, 2, b'{', b'}'];
        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_stream_open(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("unknown opcode"));
    }

    // ---

    /// Round-trip a chunk through write_chunk / read_chunk.
    #[tokio::test]
    async fn round_trip_chunk() {
        // ---
        let offset = 131_072u64;
        let data = b"hello quelay chunk";

        let mut buf: Vec<u8> = Vec::new();
        write_chunk(&mut buf, offset, data).await.unwrap();

        assert_eq!(buf.len(), CHUNK_HEADER_LEN + data.len());

        let mut reader = BufReader::new(Cursor::new(buf));
        let chunk = read_chunk(&mut reader).await.unwrap().unwrap();

        assert_eq!(chunk.stream_offset, offset);
        assert_eq!(chunk.payload, data);
    }

    // ---

    /// Clean EOF on chunk read returns None.
    #[tokio::test]
    async fn chunk_eof_returns_none() {
        // ---
        let mut reader = BufReader::new(Cursor::new(vec![]));
        let result = read_chunk(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    // ---

    /// Chunk payload_len exceeding CHUNK_SIZE is rejected before allocation.
    #[tokio::test]
    async fn chunk_oversize_rejected() {
        // ---
        let bad_len = (CHUNK_SIZE + 1) as u16;
        let mut buf = vec![0u8; CHUNK_HEADER_LEN];
        buf[8..10].copy_from_slice(&bad_len.to_be_bytes());

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_chunk(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("exceeds CHUNK_SIZE"));
    }
}
