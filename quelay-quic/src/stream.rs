//! [`QuicStream`] — a single bidirectional QUIC stream implementing
//! [`QueLayStream`].
//!
//! [`QuicStream::into_split`] splits a stream into a lock-free
//! [`QuicRecvHalf`] and [`QuicSendHalf`] backed directly by the underlying
//! `quinn` halves, avoiding the `tokio::io::split` shared lock that would
//! cause a deadlock when one task calls `shutdown()` while another task holds
//! the split lock in a blocking `poll_read`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use uuid::Uuid;

use quelay_domain::{QueLayError, QueLayStream, Result};

// ---------------------------------------------------------------------------
// QuicStream
// ---------------------------------------------------------------------------

pub struct QuicStream {
    // ---
    pub(crate) id: Uuid,
    pub(crate) send: quinn::SendStream,
    pub(crate) recv: quinn::RecvStream,
    pub(crate) finished: bool,
}

impl QuicStream {
    // ---

    /// Consume this stream and split it into a lock-free receive half and
    /// send half.
    ///
    /// Unlike [`tokio::io::split`], the two halves share **no** internal lock.
    /// Each wraps its underlying `quinn` half directly, so concurrent
    /// `poll_read` on [`QuicRecvHalf`] and `poll_write` / `finish_non_blocking`
    /// on [`QuicSendHalf`] never contend.
    #[allow(dead_code)]
    pub fn into_split(self) -> (QuicRecvHalf, QuicSendHalf) {
        // ---
        let recv_half = QuicRecvHalf { recv: self.recv };
        let send_half = QuicSendHalf {
            send: self.send,
            finished: self.finished,
        };
        (recv_half, send_half)
    }
}

// ---

#[async_trait]
impl QueLayStream for QuicStream {
    // ---
    fn stream_id(&self) -> Uuid {
        self.id
    }

    // ---

    async fn finish(&mut self) -> Result<()> {
        // ---
        if self.finished {
            return Err(QueLayError::AlreadyFinished);
        }
        self.finished = true;
        // finish() returns Result<(), ClosedStream>; ClosedStream means we
        // already closed — treat as success since the goal is achieved.
        let _ = self.send.finish();
        Ok(())
    }

    // ---

    async fn reset(&mut self, code: u64) -> Result<()> {
        // ---
        self.finished = true;
        let var = quinn::VarInt::from_u64(code).unwrap_or(quinn::VarInt::MAX);
        // reset() returns Result<(), ClosedStream>; ignore ClosedStream.
        let _ = self.send.reset(var);
        Ok(())
    }
}

// ---

impl AsyncRead for QuicStream {
    // ---
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

// ---

impl AsyncWrite for QuicStream {
    // ---
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        // ---
        // quinn SendStream::poll_write returns Poll<Result<usize, WriteError>>.
        // Map WriteError → io::Error.
        match Pin::new(&mut self.send).poll_write(cx, data) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                e.to_string(),
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // ---
        if !self.finished {
            self.finished = true;
            let _ = self.send.finish();
        }
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// QuicRecvHalf
// ---------------------------------------------------------------------------

/// Read-only half of a split [`QuicStream`].
///
/// Wraps `quinn::RecvStream` directly — no shared lock with [`QuicSendHalf`].
pub struct QuicRecvHalf {
    // ---
    pub(crate) recv: quinn::RecvStream,
}

impl AsyncRead for QuicRecvHalf {
    // ---
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

// ---

impl Unpin for QuicRecvHalf {}

// ---------------------------------------------------------------------------
// QuicSendHalf
// ---------------------------------------------------------------------------

/// Write-only half of a split [`QuicStream`].
///
/// Wraps `quinn::SendStream` directly — no shared lock with [`QuicRecvHalf`].
///
/// # Finishing the stream
///
/// Call [`finish_non_blocking`](QuicSendHalf::finish_non_blocking) to send
/// a QUIC FIN without awaiting peer acknowledgment.  This is the correct way
/// to signal end-of-stream from the uplink pump task: the application-level
/// `AckMsg::Done` already provides completion confirmation, so waiting for
/// the transport-level FIN ack is unnecessary and causes a deadlock when the
/// ack-reader task is blocked in `poll_read` on the sibling [`QuicRecvHalf`].
pub struct QuicSendHalf {
    // ---
    pub(crate) send: quinn::SendStream,
    pub(crate) finished: bool,
}

impl QuicSendHalf {
    // ---

    /// Send a QUIC FIN without waiting for the peer to acknowledge it.
    ///
    /// After this call the remote side will read `Ok(None)` (EOF) on its next
    /// read, signalling that all data has been delivered.  The local side does
    /// not need to remain alive — quinn's runtime delivers the FIN
    /// asynchronously.
    ///
    /// Idempotent: subsequent calls are no-ops.
    pub fn finish_non_blocking(&mut self) {
        // ---
        if !self.finished {
            self.finished = true;
            // quinn::SendStream::finish() is non-blocking: it marks the stream
            // for FIN and returns immediately.  ClosedStream means we already
            // called finish() once — treat as success.
            let _ = self.send.finish();
        }
    }
}

// ---

impl AsyncWrite for QuicSendHalf {
    // ---
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        // ---
        match Pin::new(&mut self.send).poll_write(cx, data) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                e.to_string(),
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // ---
        // Non-blocking: just call finish() and return Ready immediately.
        // Do NOT await peer acknowledgment — that would deadlock if the
        // ack-reader task holds the peer's read lock.
        self.finish_non_blocking();
        Poll::Ready(Ok(()))
    }
}

// ---

impl Unpin for QuicSendHalf {}
