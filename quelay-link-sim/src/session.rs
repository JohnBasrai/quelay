use std::sync::Mutex;
use std::time::Duration;

// ---

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

// ---

use quelay_domain::{
    // ---
    LinkState,
    Priority,
    QueLayError,
    QueLaySession,
    QueLayStreamPtr,
    Result,
};

// ---

use super::LinkSimConfig;
use super::LinkSimStream;

// ---------------------------------------------------------------------------
// LinkSimSession
// ---------------------------------------------------------------------------

/// In-process mock session. Create connected pairs via
/// [`LinkSimSession::pair`] or [`super::transport::LinkSimTransport::connected_pair`].
pub struct LinkSimSession {
    // ---
    /// Sends new streams to the remote session's accept queue.
    stream_tx: mpsc::UnboundedSender<QueLayStreamPtr>,

    /// Receives streams opened by the remote session.
    stream_rx: Mutex<mpsc::UnboundedReceiver<QueLayStreamPtr>>,
    link_state_tx: watch::Sender<LinkState>,
    link_state_rx: watch::Receiver<LinkState>,
    config: LinkSimConfig,
}

// ---

impl LinkSimSession {
    // ---
    /// Create a connected pair of sessions sharing the given link config.
    pub fn pair(config: LinkSimConfig) -> (Self, Self) {
        // ---
        let (a_tx, a_rx) = mpsc::unbounded_channel();
        let (b_tx, b_rx) = mpsc::unbounded_channel();
        let (ls_tx_a, ls_rx_a) = watch::channel(LinkState::Normal);
        let (ls_tx_b, ls_rx_b) = watch::channel(LinkState::Normal);

        let session_a = LinkSimSession {
            stream_tx: a_tx,
            stream_rx: Mutex::new(b_rx),
            link_state_tx: ls_tx_a,
            link_state_rx: ls_rx_a,
            config: config.clone(),
        };
        let session_b = LinkSimSession {
            stream_tx: b_tx,
            stream_rx: Mutex::new(a_rx),
            link_state_tx: ls_tx_b,
            link_state_rx: ls_rx_b,
            config,
        };

        (session_a, session_b)
    }

    // ---

    /// Simulate a link outage for testing.
    ///
    /// Sets link state to [`LinkState::Failed`], waits for `duration`,
    /// then restores [`LinkState::Normal`]`.
    pub async fn simulate_outage(&self, duration: Duration) {
        // ---
        self.link_state_tx.send(LinkState::Failed).ok();
        tokio::time::sleep(duration).await;
        self.link_state_tx.send(LinkState::Normal).ok();
    }
}

// ---

#[async_trait]
impl QueLaySession for LinkSimSession {
    // ---
    async fn open_stream(&self, _priority: Priority) -> Result<QueLayStreamPtr> {
        // ---
        let id = Uuid::new_v4();
        let (reset_tx, _) = mpsc::unbounded_channel();
        let (local_tx, remote_rx) = mpsc::unbounded_channel();
        let (remote_tx, local_rx) = mpsc::unbounded_channel();

        let bw_cap = self.config.bw_cap_bps;

        let local: QueLayStreamPtr = Box::new(LinkSimStream::new(
            id,
            local_tx,
            local_rx,
            reset_tx.clone(),
            bw_cap,
        ));
        let remote: QueLayStreamPtr = Box::new(LinkSimStream::new(
            id, remote_tx, remote_rx, reset_tx, bw_cap,
        ));

        self.stream_tx
            .send(remote)
            .map_err(|_| QueLayError::SessionClosed)?;

        Ok(local)
    }

    // ---

    async fn accept_stream(&self) -> Result<QueLayStreamPtr> {
        // ---
        loop {
            {
                let mut guard = self.stream_rx.lock().unwrap();
                match guard.try_recv() {
                    Ok(stream) => return Ok(stream),
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        return Err(QueLayError::SessionClosed);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // ---

    fn link_state(&self) -> LinkState {
        *self.link_state_rx.borrow()
    }

    fn link_state_rx(&self) -> watch::Receiver<LinkState> {
        self.link_state_rx.clone()
    }

    async fn close(&self) -> Result<()> {
        // ---
        self.link_state_tx.send(LinkState::Failed).ok();
        Ok(())
    }
}
