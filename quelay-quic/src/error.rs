//! Error types for `quelay-quic`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum QuicError {
    // ---
    #[error("TLS error: {0}")]
    Tls(String),

    #[error("QUIC connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),

    #[error("QUIC connect error: {0}")]
    Connect(#[from] quinn::ConnectError),

    #[error("QUIC write error: {0}")]
    Write(#[from] quinn::WriteError),

    #[error("QUIC read error: {0}")]
    Read(#[from] quinn::ReadError),

    #[error("QUIC endpoint error: {0}")]
    Endpoint(String),

    #[error("stream already finished")]
    AlreadyFinished,
}

// ---------------------------------------------------------------------------
// Bridge to quelay_domain::QueLayError
// ---------------------------------------------------------------------------

impl From<QuicError> for quelay_domain::QueLayError {
    // ---
    fn from(e: QuicError) -> Self {
        quelay_domain::QueLayError::Transport(e.to_string())
    }
}
