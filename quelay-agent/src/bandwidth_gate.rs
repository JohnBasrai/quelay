//! Bandwidth gate — rate-limiting wrapper around a [`QueLayStream`].
//!
//! [`BandwidthGate`] sits between the data pump and the QUIC transport.
//! It serves two operational purposes:
//!
//! 1. **Rate limiting** — a token-bucket enforces an operator-configured
//!    bytes-per-second ceiling.  Setting the cap to `None` disables rate
//!    limiting entirely (loopback CI, uncapped production links).
//!
//! 2. **Link-down injection** — an [`Arc<AtomicBool>`] `link_enabled` flag
//!    is checked on every write just before bytes are handed to the
//!    transport.  When `false`, the write returns
//!    [`io::ErrorKind::ConnectionAborted`], which the uplink pump treats
//!    identically to a real [`quinn::WriteError::ConnectionLost`]: rewind
//!    Q→A and enter the reconnect loop.
//!
//! ## Where it lives in the stack
//!
//! ```text
//! ┌─────────────────────┐
//! │  uplink data pump   │  (quelay-agent/active_stream.rs)
//! └────────┬────────────┘
//!          │ AsyncWrite
//! ┌────────▼────────────┐
//! │   BandwidthGate     │  ← token bucket + link_enabled check
//! └────────┬────────────┘
//!          │ AsyncWrite
//! ┌────────▼────────────┐
//! │   QuicStream        │  (quelay-quic)
//! └─────────────────────┘
//! ```
//!
//! ## Link-down injection in tests
//!
//! `SessionManager::link_enable(false)` sets the shared `link_enabled` flag
//! to `false`.  The next `poll_write` on any active `BandwidthGate` returns
//! an error, driving the pump into the spool-and-reconnect path — the same
//! code path that fires on a real satellite link outage.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

// ---

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use uuid::Uuid;

// ---

use quelay_domain::{QueLayStream, Result};

// ---------------------------------------------------------------------------
// TokenBucket
// ---------------------------------------------------------------------------

/// Token-bucket rate limiter.
///
/// Tokens represent bytes.  On each [`TokenBucket::try_consume`] call the
/// bucket refills based on elapsed wall time at the configured rate, then
/// consumes up to `n` tokens.  Capacity is capped at one second's worth so
/// long idle periods cannot accumulate an unbounded burst allowance.
///
/// This is an internal implementation detail of [`BandwidthGate`]; it is
/// exposed as `pub(crate)` only so the unit tests can exercise it directly.
pub(crate) struct TokenBucket {
    // ---
    /// Bytes per second ceiling.
    rate_bps: u64,

    /// Available tokens (bytes), fractional to avoid rounding drift.
    tokens: f64,

    /// Last refill timestamp.
    last_refill: Instant,
}

// ---

impl TokenBucket {
    // ---
    pub(crate) fn new(rate_bps: u64) -> Self {
        // ---
        Self {
            rate_bps,
            tokens: 0.0, // start empty — first write pays via refill
            last_refill: Instant::now(),
        }
    }

    // ---

    /// Refill from elapsed time, then consume up to `n` bytes.
    ///
    /// Returns the number of bytes actually consumed; `0` means the bucket
    /// is empty and the caller should yield and retry.
    pub(crate) fn try_consume(&mut self, n: usize) -> usize {
        // ---
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        // Refill, but cap at one second's worth to prevent burst accumulation.
        let cap = self.rate_bps as f64;
        self.tokens = (self.tokens + elapsed * cap).min(cap);

        let consumed = (self.tokens as usize).min(n);
        self.tokens -= consumed as f64;
        consumed
    }
}

// ---------------------------------------------------------------------------
// BandwidthGate
// ---------------------------------------------------------------------------

/// Rate-limiting and link-down-injection wrapper around any `AsyncRead +
/// AsyncWrite` stream.
///
/// Constructed by [`BandwidthGate::new`].  Reads pass through to the inner
/// stream without modification.  Writes are subject to:
///
/// 1. **`link_enabled` check** — if `false`, returns
///    `ErrorKind::ConnectionAborted` immediately (no bytes written).
/// 2. **Token-bucket rate limit** — if `bucket` is `Some` and the bucket is
///    empty, returns `Poll::Pending` after scheduling an immediate re-wake so
///    tokens can refill before the next poll.
pub struct BandwidthGate<S> {
    // ---
    inner: S,
    bucket: Option<TokenBucket>,

    /// Shared with [`SessionManager`].  Set to `false` by `link_enable(false)`
    /// to simulate a link outage.  The pump sees `ConnectionAborted` and
    /// enters the spool-and-reconnect path.
    link_enabled: Arc<AtomicBool>,
}

// ---

impl<S> BandwidthGate<S> {
    // ---
    /// Wrap `inner` with optional rate limiting and link-down injection.
    ///
    /// - `rate_bps`: `None` = uncapped; `Some(n)` = cap at `n` bytes/second.
    /// - `link_enabled`: shared flag; set to `false` to inject a link-down
    ///   error on the next write.
    pub fn new(inner: S, rate_bps: Option<u64>, link_enabled: Arc<AtomicBool>) -> Self {
        // ---
        Self {
            inner,
            bucket: rate_bps.map(TokenBucket::new),
            link_enabled,
        }
    }
}

// ---

impl<S: AsyncRead + Unpin> AsyncRead for BandwidthGate<S> {
    // ---
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Reads are not rate-limited and ignore link_enabled — we want to
        // drain any in-flight bytes from the peer even after a link-down
        // event so the session can close cleanly.
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

// ---

impl<S: AsyncWrite + Unpin> AsyncWrite for BandwidthGate<S> {
    // ---
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        // ---

        // 1. Link-down check — before touching the token bucket.
        if !self.link_enabled.load(Ordering::Relaxed) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "link disabled",
            )));
        }

        // 2. Token-bucket rate limit.
        let allowed = match self.bucket.as_mut() {
            None => data.len(),
            Some(bucket) => bucket.try_consume(data.len()),
        };

        if allowed == 0 {
            // Bucket empty — reschedule immediately so tokens can refill
            // before the next poll.  This yields the task without blocking
            // the executor thread.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }

        // 3. Forward to inner stream.
        Pin::new(&mut self.inner).poll_write(cx, &data[..allowed])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// QueLayStream delegation
// ---------------------------------------------------------------------------

/// Delegate [`QueLayStream`] to the inner stream so `BandwidthGate<S>` can
/// be boxed as a `QueLayStreamPtr` and handed directly to `spawn_uplink`.
#[async_trait]
impl<S: QueLayStream + Send> QueLayStream for BandwidthGate<S> {
    // ---
    fn stream_id(&self) -> Uuid {
        self.inner.stream_id()
    }

    async fn finish(&mut self) -> Result<()> {
        self.inner.finish().await
    }

    async fn reset(&mut self, code: u64) -> Result<()> {
        self.inner.reset(code).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // ---
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::io::AsyncWriteExt;

    use super::{BandwidthGate, TokenBucket};

    // ---

    /// Bucket starts empty; after a measured sleep, refill should be
    /// proportional to elapsed time within a 15% tolerance.
    #[test]
    fn token_bucket_starts_empty_and_refills() {
        // ---
        let rate = 1_000_000_u64; // 1 MB/s
        let mut bucket = TokenBucket::new(rate);

        // Empty bucket grants nothing.
        assert_eq!(
            bucket.try_consume(1_000_000),
            0,
            "empty bucket should grant zero bytes"
        );

        // After 100 ms, ~100 KB should be available.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let got = bucket.try_consume(1_000_000) as f64;
        let expected = rate as f64 * 0.1;
        let delta = (got - expected).abs() / expected;
        assert!(
            delta < 0.15,
            "after 100ms: got {got:.0} bytes, expected ~{expected:.0} (delta {:.1}%)",
            delta * 100.0
        );
    }

    // ---

    /// Burst accumulation is capped at one second's worth so a long idle
    /// period cannot produce an unbounded burst.
    #[test]
    fn token_bucket_caps_burst_at_one_second() {
        // ---
        let rate = 500_000_u64; // 500 KB/s
        let mut bucket = TokenBucket::new(rate);

        // Sleep well over one second.
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // Should not be able to consume more than one second's worth.
        let got = bucket.try_consume(usize::MAX);
        assert!(
            got <= rate as usize,
            "burst should be capped at {rate} bytes, got {got}"
        );
    }

    // ---

    /// End-to-end: `BandwidthGate` wrapping a `Vec<u8>` enforces throughput.
    ///
    /// 500 KB at 2 MB/s should take ~250ms.  We assert the gate does not
    /// allow more than 115% of the configured rate (cap enforced) and does
    /// not drop below 50% (not pathologically slow).
    #[tokio::test]
    async fn bandwidth_gate_caps_throughput() {
        // ---
        let rate_bps = 2_000_000_u64; // 2 MB/s
        let payload_bytes = 500_000_usize; // 500 KB → ~250ms

        let buf: Vec<u8> = Vec::new();
        let link_enabled = Arc::new(AtomicBool::new(true));
        let mut gate = BandwidthGate::new(buf, Some(rate_bps), link_enabled);

        let data = vec![0xABu8; payload_bytes];
        let start = Instant::now();
        gate.write_all(&data).await.expect("write_all failed");
        let elapsed = start.elapsed().as_secs_f64();

        let actual_bps = payload_bytes as f64 / elapsed;
        let ratio = actual_bps / rate_bps as f64;
        assert!(
            ratio <= 1.15,
            "throughput {actual_bps:.0} bps exceeded cap {rate_bps} bps by >{:.0}%",
            (ratio - 1.0) * 100.0
        );
        assert!(
            ratio >= 0.50,
            "throughput {actual_bps:.0} bps was less than 50% of cap {rate_bps} bps"
        );
    }

    // ---

    /// When `link_enabled` is `false`, `poll_write` returns `ConnectionAborted`
    /// immediately without writing any bytes.
    #[tokio::test]
    async fn link_disabled_returns_error() {
        // ---
        let link_enabled = Arc::new(AtomicBool::new(false));
        let buf: Vec<u8> = Vec::new();
        let mut gate = BandwidthGate::new(buf, None, link_enabled);

        let result = gate.write_all(&[0u8; 1024]).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
    }

    // ---

    /// Mid-transfer link disable: write some bytes successfully, then flip
    /// `link_enabled` to `false` and verify subsequent writes fail.
    #[tokio::test]
    async fn link_disabled_mid_transfer() {
        // ---
        let link_enabled = Arc::new(AtomicBool::new(true));
        let buf: Vec<u8> = Vec::new();
        let mut gate = BandwidthGate::new(buf, None, Arc::clone(&link_enabled));

        // First write succeeds.
        gate.write_all(&[0u8; 1024])
            .await
            .expect("first write failed");

        // Flip the flag — simulates link_enable(false) from SessionManager.
        link_enabled.store(false, Ordering::Relaxed);

        // Subsequent write fails.
        let result = gate.write_all(&[0u8; 1024]).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
    }
}
