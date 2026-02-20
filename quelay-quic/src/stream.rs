//! [`QuicStream`] — a single bidirectional QUIC stream implementing
//! [`QueLayStream`].

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
