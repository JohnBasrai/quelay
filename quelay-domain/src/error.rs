use thiserror::Error;

// ---

#[derive(Debug, Error)]
pub enum QueLayError {
    // ---
    #[error("transport error: {0}")]
    Transport(String),

    #[error("stream reset by remote (code {code})")]
    StreamReset { code: u64 },

    #[error("stream already finished")]
    AlreadyFinished,

    #[error("session closed")]
    SessionClosed,

    #[error("stream not found: {0}")]
    StreamNotFound(uuid::Uuid),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ---

pub type Result<T> = std::result::Result<T, QueLayError>;
