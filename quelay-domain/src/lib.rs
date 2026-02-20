//! Core traits, types, and DRR scheduler for the Quelay data relay.
//!
//! This crate defines the vocabulary of the system. All other crates depend
//! on `quelay-domain` and speak its types. No implementations live here.
//!
//! # Structure
//!
//! - [`error`]     — [`QueLayError`] and [`Result<T>`] alias
//! - [`priority`]  — [`Priority`] levels (C2I, BulkTransfer)
//! - [`transport`] — [`QueLayStream`], [`QueLaySession`], [`QueLayTransport`] traits
//! - [`scheduler`] — [`DrrScheduler`] (Deficit Round Robin)
//! - [`session`]   — [`StreamMeta`], [`StreamInfo`], [`QueLayHandler`] callback trait

mod error;
mod priority;
mod scheduler;
mod session;
mod transport;

// --- error
pub use error::{QueLayError, Result};

// --- priority
pub use priority::Priority;

// --- transport
pub use transport::{
    // ---
    LinkState,
    QueLaySession,
    QueLaySessionPtr,
    QueLayStream,
    QueLayStreamPtr,
    QueLayTransport,
};

// --- scheduler
pub use scheduler::DrrScheduler;

// --- session
pub use session::{
    // ---
    QueLayHandler,
    QueueStatus,
    StreamInfo,
    StreamMeta,
    TransferProgress,
};
