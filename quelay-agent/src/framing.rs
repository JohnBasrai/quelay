//! Wire framing for Quelay stream headers.
//!
//! Every QUIC stream opened by the sender begins with a fixed-size binary
//! header followed by a variable-length JSON payload, followed immediately
//! by the raw file/stream data.
//!
//! ```text
//! +-------+-------+-------------------+-----------------------------+
//! | magic | ver   | payload_len (u32) | payload (payload_len bytes) |
//! | 0x51  | 0x01  | big-endian        | UTF-8 JSON                  |
//! +-------+-------+-------------------+-----------------------------+
//!   1 byte  1 byte      4 bytes          variable
//!                  ← fixed 6 bytes →
//! ```
//!
//! After the payload the raw stream data begins with no further framing;
//! stream close (FIN) signals EOF.

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

/// Wire format version.  Bump when the fixed header layout changes.
pub const VERSION: u8 = 0x01;

/// Fixed header size in bytes: magic(1) + ver(1) + payload_len(4).
pub const FIXED_HEADER_LEN: usize = 6;

// ---------------------------------------------------------------------------
// StreamHeader
// ---------------------------------------------------------------------------

/// Application-level metadata written at the start of every Quelay stream.
///
/// Serialized as JSON and preceded by the fixed 6-byte binary header.
/// Raw stream data follows immediately after the JSON payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    // ---
    /// Stable stream identity.  Survives reconnections; used for UUID remapping.
    pub uuid: Uuid,

    /// Raw Thrift priority byte (0 ..= 127).
    pub priority: u8,

    /// Original file name; used by the receiver when writing to disk.
    pub file_name: String,

    /// Known size in bytes.  `None` for open-ended or unknown-length streams.
    /// When present, enables `percent_done` progress callbacks.
    pub size_bytes: Option<u64>,

    /// Open-ended application metadata forwarded verbatim.
    /// `HashMap` enforces key uniqueness; serializes as a JSON object.
    pub attrs: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// write_header
// ---------------------------------------------------------------------------

/// Serialize `header` and write the fixed preamble + JSON payload to `stream`.
///
/// Errors if serialization fails or the write to `stream` fails.
pub async fn write_header<W>(stream: &mut W, header: &StreamHeader) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(header)
        .map_err(|e| QueLayError::Transport(format!("framing serialize error: {e}")))?;

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("stream header payload exceeds 4 GiB".into()))?;

    // Fixed header: magic + version + payload_len (big-endian u32)
    let mut fixed = [0u8; FIXED_HEADER_LEN];
    fixed[0] = MAGIC;
    fixed[1] = VERSION;
    fixed[2..6].copy_from_slice(&payload_len.to_be_bytes());

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
// read_header
// ---------------------------------------------------------------------------

/// Read and deserialize a [`StreamHeader`] from `stream`.
///
/// Returns an error if the magic byte or version are wrong, or if the
/// payload cannot be deserialized.
///
/// Called by the inbound stream accept path, which is wired up in the
/// data-path iteration.  Suppressed until then to keep CI clean.
#[allow(dead_code)]
pub async fn read_header<R>(stream: &mut R) -> Result<StreamHeader>
where
    R: AsyncRead + Unpin,
{
    // Read fixed header.
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
            "framing unsupported version: expected {VERSION}, got {}",
            fixed[1]
        )));
    }

    let payload_len = u32::from_be_bytes(fixed[2..6].try_into().unwrap()) as usize;

    // Read variable payload.
    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| QueLayError::Transport(format!("framing read payload: {e}")))?;

    let header: StreamHeader = serde_json::from_slice(&payload)
        .map_err(|e| QueLayError::Transport(format!("framing deserialize error: {e}")))?;

    Ok(header)
}

// ---------------------------------------------------------------------------
// WormholeMsg
// ---------------------------------------------------------------------------

/// Messages flowing receiver → sender on the QUIC stream's read half.
///
/// After the sender writes the [`StreamHeader`] and raw bytes, the receiver
/// sends these framed messages back on the same QUIC stream (using the same
/// 6-byte fixed header + JSON payload format).
///
/// The sender's pump loop reads these while piping data forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WormholeMsg {
    /// Periodic acknowledgement — receiver has written this many bytes to disk.
    Ack { bytes_received: u64 },

    /// Receiver finished writing successfully.
    Done,

    /// Receiver encountered a fatal error.
    ///
    /// `code` maps to [`quelay_thrift::FailReason`] wire values.
    Error { code: u32, reason: String },
}

// ---

/// Serialize `msg` and write it as a framed message to `stream`.
///
/// Uses the same 6-byte fixed header as [`write_header`].
pub async fn write_wormhole_msg<W>(stream: &mut W, msg: &WormholeMsg) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(msg)
        .map_err(|e| QueLayError::Transport(format!("wormhole serialize error: {e}")))?;

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("wormhole payload exceeds 4 GiB".into()))?;

    let mut fixed = [0u8; FIXED_HEADER_LEN];
    fixed[0] = MAGIC;
    fixed[1] = VERSION;
    fixed[2..6].copy_from_slice(&payload_len.to_be_bytes());

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
pub async fn read_wormhole_msg<R>(stream: &mut R) -> Result<WormholeMsg>
where
    R: AsyncRead + Unpin,
{
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
            "wormhole unsupported version: expected {VERSION}, got {}",
            fixed[1]
        )));
    }

    let payload_len = u32::from_be_bytes(fixed[2..6].try_into().unwrap()) as usize;

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

/// Payload size for each data chunk written to the QUIC stream.
///
/// This is the granularity of the spool and ack system.  Smaller values give
/// finer-grained acks; larger values reduce per-chunk framing overhead.
///
/// TODO: wire to config.
pub const CHUNK_SIZE: usize = 16 * 1024; // 16 KiB

/// Fixed chunk header: u64 stream offset (8 bytes) + u32 payload length (4 bytes).
pub const CHUNK_HEADER_LEN: usize = 12;

// ---

/// Write one chunk to `stream`.
///
/// Frame layout:
/// ```text
/// +-------------------+-------------------+------------------+
/// | stream_offset(u64)| payload_len (u32) | payload bytes    |
/// | big-endian        | big-endian        | (payload_len B)  |
/// +-------------------+-------------------+------------------+
/// |     8 bytes              4 bytes      |  variable
/// |←————— CHUNK_HEADER_LEN = 12 bytes ———→|
/// ```
///
/// `stream_offset` is the absolute byte position of the first payload byte
/// in the logical stream.  The receiver uses this to detect duplicate chunks
/// (offset already delivered) and to assert ordering (gap = logic error).
pub async fn write_chunk<W>(stream: &mut W, stream_offset: u64, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| QueLayError::Transport("chunk payload exceeds 4 GiB".into()))?;

    let mut hdr = [0u8; CHUNK_HEADER_LEN];
    hdr[0..8].copy_from_slice(&stream_offset.to_be_bytes());
    hdr[8..12].copy_from_slice(&payload_len.to_be_bytes());

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
pub struct Chunk {
    /// Absolute byte offset of the first payload byte in the logical stream.
    pub stream_offset: u64,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

// ---

/// Read one chunk from `stream`.
///
/// Returns `None` on clean EOF (zero-length read of the header), which
/// signals that the sender has closed the QUIC write half.
pub async fn read_chunk<R>(stream: &mut R) -> Result<Option<Chunk>>
where
    R: AsyncRead + Unpin,
{
    let mut hdr = [0u8; CHUNK_HEADER_LEN];

    // Peek at the first byte to distinguish clean EOF from a real header.
    match stream.read(&mut hdr[..1]).await {
        Ok(0) => return Ok(None), // clean EOF
        Ok(_) => {}
        Err(e) => return Err(QueLayError::Transport(format!("chunk read header[0]: {e}"))),
    }

    // Read the remaining 11 header bytes.
    stream
        .read_exact(&mut hdr[1..])
        .await
        .map_err(|e| QueLayError::Transport(format!("chunk read header[1..]: {e}")))?;

    let stream_offset = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
    let payload_len = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;

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

    /// Round-trip: write a header into a buffer, read it back, verify fields.
    #[tokio::test]
    async fn round_trip() {
        // ---
        let original = StreamHeader {
            uuid: Uuid::new_v4(),
            priority: 64,
            file_name: "telemetry.bin".into(),
            size_bytes: Some(1_048_576),
            attrs: [("content_type".into(), "application/octet-stream".into())].into(),
        };

        // Write into an in-memory buffer.
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, &original).await.unwrap();

        // Append raw "data" to confirm only the header is consumed.
        buf.extend_from_slice(b"raw-data");

        // Read back.
        let mut reader = BufReader::new(Cursor::new(buf));
        let recovered = read_header(&mut reader).await.unwrap();

        assert_eq!(recovered.uuid, original.uuid);
        assert_eq!(recovered.priority, original.priority);
        assert_eq!(recovered.file_name, original.file_name);
        assert_eq!(recovered.size_bytes, original.size_bytes);
        assert_eq!(recovered.attrs["content_type"], "application/octet-stream");

        // Confirm raw data is still available after header read.
        let mut tail = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut tail)
            .await
            .unwrap();
        assert_eq!(tail, b"raw-data");
    }

    // ---

    #[tokio::test]
    async fn bad_magic_rejected() {
        // ---
        let buf = vec![0xFFu8, VERSION, 0, 0, 0, 2, b'{', b'}'];
        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_header(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("bad magic"));
    }

    // ---

    #[tokio::test]
    async fn bad_version_rejected() {
        // ---
        let buf = vec![MAGIC, 0xFFu8, 0, 0, 0, 2, b'{', b'}'];
        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_header(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("unsupported version"));
    }
}
