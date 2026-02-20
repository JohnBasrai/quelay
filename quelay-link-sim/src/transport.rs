use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::sync::mpsc;

use quelay_domain::{QueLayTransport, Result};

use super::config::LinkSimConfig;
use super::session::LinkSimSession;

// ---------------------------------------------------------------------------
// LinkSimTransport
// ---------------------------------------------------------------------------

/// In-process mock transport. Does not use real sockets.
///
/// For simple unit tests, prefer [`LinkSimTransport::connected_pair`] over
/// `connect` / `listen`, which are provided only for interface compliance.
pub struct LinkSimTransport {
    // ---
    config: LinkSimConfig,
}

// ---

impl LinkSimTransport {
    // ---
    pub fn new(config: LinkSimConfig) -> Self {
        Self { config }
    }

    // ---

    /// Create a directly-connected session pair without going through
    /// `connect` / `listen`. The preferred entry point for unit tests.
    pub fn connected_pair(&self) -> (LinkSimSession, LinkSimSession) {
        LinkSimSession::pair(self.config.clone())
    }
}

// ---

#[async_trait]
impl QueLayTransport for LinkSimTransport {
    // ---
    type Session = LinkSimSession;

    async fn connect(&self, _remote: SocketAddr) -> Result<Self::Session> {
        let (a, _b) = LinkSimSession::pair(self.config.clone());
        Ok(a)
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<mpsc::Receiver<Self::Session>> {
        let (_tx, rx) = mpsc::channel(8);
        Ok(rx)
    }
}
