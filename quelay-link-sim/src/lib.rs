//! In-process mock transport for Quelay unit and integration testing.
//!
//! [`LinkSimTransport`] implements [`quelay_domain::QueLayTransport`] using tokio
//! channels instead of real sockets. [`LinkSimConfig`] controls injected
//! impairments:
//!
//! - Packet drop probability
//! - Packet duplication probability
//! - Bandwidth cap (token bucket)
//! - Link outage (pause for a duration, then resume)
//! - Deterministic RNG seed for reproducible runs
//!
//! # Quick start
//!
//! ```rust
//! use quelay_link_sim::{LinkSimTransport, LinkSimConfig};
//!
//! let (client, server) = LinkSimTransport::new(LinkSimConfig::outage_60s())
//!     .connected_pair();
//! ```

mod config;
mod session;
mod stream;
mod transport;

// --- public API
pub use config::LinkSimConfig;

#[allow(unused_imports)]
pub(crate) use session::LinkSimSession;

pub(crate) use stream::LinkSimStream;
pub use transport::LinkSimTransport;

//pub(crate) use stream::TokenBucket;
