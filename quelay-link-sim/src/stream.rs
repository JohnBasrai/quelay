use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

// ---

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use uuid::Uuid;

// ---

use quelay_domain::{QueLayError, QueLayStream, Result};

// ---------------------------------------------------------------------------
// TokenBucket
// ---------------------------------------------------------------------------

/// Simple token bucket for bandwidth capping in [`LinkSimStream`].
///
/// Tokens represent bytes. On each [`TokenBucket::try_consume`] call the
/// bucket refills based on elapsed wall time at the configured rate, then
/// consumes up to `n` tokens. Capacity is capped at one second's worth so
/// long idle periods cannot accumulate an unbounded burst allowance.
pub(crate) struct TokenBucket {
    // ---
    /// Bytes per second limit.
    rate_bps: u64,

    /// Available tokens (bytes).
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
            tokens: 0.0, // start empty — first write pays for tokens via refill
            last_refill: Instant::now(),
        }
    }

    // ---

    /// Refill from elapsed time, then consume up to `n` bytes.
    ///
    /// Returns bytes actually consumed; 0 means the bucket is empty.
    pub(crate) fn try_consume(&mut self, n: usize) -> usize {
        // ---
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        let cap = self.rate_bps as f64;
        self.tokens = (self.tokens + elapsed * cap).min(cap);

        let consumed = (self.tokens as usize).min(n);
        self.tokens -= consumed as f64;
        consumed
    }
}

// ---------------------------------------------------------------------------
// LinkSimStream
// ---------------------------------------------------------------------------

/// One end of an in-process mock stream backed by mpsc channels.
///
/// Created in connected pairs by [`super::session::LinkSimSession::open_stream`].
/// The write side sends `Vec<u8>` chunks; the read side receives them.
/// An empty chunk signals EOF (equivalent to a QUIC FIN).
///
/// When `bucket` is `Some`, writes are throttled so aggregate throughput
/// stays at or below the configured rate.
pub(crate) struct LinkSimStream {
    // ---
    pub(crate) id: Uuid,
    pub(crate) tx: mpsc::UnboundedSender<Vec<u8>>,
    pub(crate) rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Leftover bytes from a partially consumed chunk.
    pub(crate) read_buf: Vec<u8>,
    pub(crate) finished: bool,
    pub(crate) reset_tx: mpsc::UnboundedSender<u64>,
    /// `None` = unlimited. `Some` = token-bucket throttled.
    pub(crate) bucket: Option<TokenBucket>,
}

// ---

impl LinkSimStream {
    // ---
    pub(crate) fn new(
        id: Uuid,
        tx: mpsc::UnboundedSender<Vec<u8>>,
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
        reset_tx: mpsc::UnboundedSender<u64>,
        bw_cap_bps: Option<u64>,
    ) -> Self {
        // ---
        Self {
            id,
            tx,
            rx,
            read_buf: Vec::new(),
            finished: false,
            reset_tx,
            bucket: bw_cap_bps.map(TokenBucket::new),
        }
    }
}

// ---

#[async_trait]
impl QueLayStream for LinkSimStream {
    // ---
    fn stream_id(&self) -> Uuid {
        self.id
    }

    async fn finish(&mut self) -> Result<()> {
        // ---
        if self.finished {
            return Err(QueLayError::AlreadyFinished);
        }
        self.finished = true;
        let _ = self.tx.send(vec![]);
        Ok(())
    }

    async fn reset(&mut self, code: u64) -> Result<()> {
        // ---
        let _ = self.reset_tx.send(code);
        self.finished = true;
        Ok(())
    }
}

// ---

impl AsyncRead for LinkSimStream {
    // ---
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // ---
        if !self.read_buf.is_empty() {
            let n = buf.remaining().min(self.read_buf.len());
            buf.put_slice(&self.read_buf[..n]);
            self.read_buf.drain(..n);
            return Poll::Ready(Ok(()));
        }

        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) if chunk.is_empty() => Poll::Ready(Ok(())),
            Poll::Ready(Some(chunk)) => {
                let n = buf.remaining().min(chunk.len());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.read_buf.extend_from_slice(&chunk[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---

impl AsyncWrite for LinkSimStream {
    // ---
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        // ---
        let allowed = match self.bucket.as_mut() {
            None => data.len(),
            Some(bucket) => bucket.try_consume(data.len()),
        };

        if allowed == 0 {
            // Bucket empty — yield and reschedule immediately so tokens
            // can refill before the next poll.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }

        match self.tx.send(data[..allowed].to_vec()) {
            Ok(()) => Poll::Ready(Ok(allowed)),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mock stream channel closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // ---
        if !self.finished {
            self.finished = true;
            let _ = self.tx.send(vec![]);
        }
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // ---
    use std::time::Instant;

    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::{LinkSimStream, TokenBucket};

    // ---

    /// Verify the token bucket refills at the configured rate and enforces
    /// the cap across multiple consume calls.
    #[test]
    fn token_bucket_caps_throughput() {
        // ---
        let rate = 1_000_000_u64; // 1 MB/s
        let mut bucket = TokenBucket::new(rate);

        // Bucket starts empty — immediate consume should grant nothing.
        let got = bucket.try_consume(1_000_000);
        assert_eq!(got, 0, "empty bucket should grant zero bytes");

        // After 100ms, ~100 KB should have refilled.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let got = bucket.try_consume(1_000_000) as f64;
        let expected = rate as f64 * 0.1; // 100ms worth
        let delta = (got - expected).abs() / expected;
        assert!(
            delta < 0.15,
            "after 100ms refill, got {got} bytes, expected ~{expected} (delta {:.1}%)",
            delta * 100.0
        );
    }

    // ---

    /// Verify that a capped `LinkSimStream` enforces throughput end-to-end
    /// through `poll_write` / `AsyncWriteExt`.
    #[tokio::test]
    async fn mock_stream_bw_cap_enforced() {
        // ---
        let rate_bps = 2_000_000_u64; // 2 MB/s
        let payload_bytes = 500_000_usize; // 500 KB → expect ~250ms

        let (tx, _rx) = mpsc::unbounded_channel();
        let (_, dummy_rx) = mpsc::unbounded_channel();
        let (reset_tx, _) = mpsc::unbounded_channel();

        let mut stream = LinkSimStream::new(Uuid::new_v4(), tx, dummy_rx, reset_tx, Some(rate_bps));

        let data = vec![0u8; payload_bytes];
        let start = Instant::now();
        stream.write_all(&data).await.expect("write_all failed");
        let elapsed = start.elapsed().as_secs_f64();

        let actual_bps = payload_bytes as f64 / elapsed;
        let ratio = actual_bps / rate_bps as f64;
        assert!(
            ratio <= 1.15,
            "throughput {actual_bps:.0} bps exceeded cap {rate_bps} bps by more than 15%"
        );
        assert!(
            ratio >= 0.50,
            "throughput {actual_bps:.0} bps was less than 50% of cap {rate_bps} bps — too slow"
        );
    }
}
