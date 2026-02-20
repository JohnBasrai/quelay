//! [`QuicSession`] — a live QUIC connection implementing [`QueLaySession`].
//!
//! Thin wrapper around a [`quinn::Connection`]. Reconnection and multi-peer
//! management live above this layer in the session manager (future crate).

use async_trait::async_trait;
use tokio::sync::watch;
use uuid::Uuid;

use quelay_domain::{
    // ---
    LinkState,
    Priority,
    QueLayError,
    QueLaySession,
    QueLayStreamPtr,
    Result,
};

use crate::error::QuicError;
use crate::stream::QuicStream;

// ---------------------------------------------------------------------------
// QuicSession
// ---------------------------------------------------------------------------

/// A live QUIC connection to a single remote peer.
///
/// One `QuicSession` per remote peer. Multiple peers → multiple sessions,
/// managed externally by the session manager layer.
pub struct QuicSession {
    // ---
    conn: quinn::Connection,
    link_state_tx: watch::Sender<LinkState>,
    link_state_rx: watch::Receiver<LinkState>,
}

// ---

impl QuicSession {
    // ---
    /// Wrap an established [`quinn::Connection`] in a `QuicSession`.
    pub fn new(conn: quinn::Connection) -> Self {
        // ---
        let (tx, rx) = watch::channel(LinkState::Normal);
        Self {
            conn,
            link_state_tx: tx,
            link_state_rx: rx,
        }
    }

    // ---

    /// Update the observable link state and notify all watchers.
    pub fn set_link_state(&self, state: LinkState) {
        // ---
        self.link_state_tx.send_replace(state);
    }
}

// ---

#[async_trait]
impl QueLaySession for QuicSession {
    // ---
    async fn open_stream(&self, _priority: Priority) -> Result<QueLayStreamPtr> {
        // ---
        let (send, recv) = self
            .conn
            .open_bi()
            .await
            .map_err(QuicError::Connection)
            .map_err(QueLayError::from)?;

        let stream = QuicStream {
            id: Uuid::new_v4(),
            send,
            recv,
            finished: false,
        };

        Ok(Box::new(stream))
    }

    // ---

    async fn accept_stream(&self) -> Result<QueLayStreamPtr> {
        // ---
        let (send, recv) = self
            .conn
            .accept_bi()
            .await
            .map_err(QuicError::Connection)
            .map_err(QueLayError::from)?;

        let stream = QuicStream {
            id: Uuid::new_v4(),
            send,
            recv,
            finished: false,
        };

        Ok(Box::new(stream))
    }

    // ---

    fn link_state(&self) -> LinkState {
        *self.link_state_rx.borrow()
    }

    // ---

    fn link_state_rx(&self) -> watch::Receiver<LinkState> {
        self.link_state_rx.clone()
    }

    // ---

    async fn close(&self) -> Result<()> {
        // ---
        self.set_link_state(LinkState::Failed);
        self.conn.close(quinn::VarInt::from_u32(0), b"closed");
        Ok(())
    }
}
