//! [`Agent`] — processes commands from the Thrift handler and delegates to
//! [`SessionManagerHandle`].
//!
//! `Agent` no longer owns a `QueLaySessionPtr` directly.  All session
//! interaction (stream open, reconnect, pending queue) is the session
//! manager's responsibility.  `Agent` is now a thin dispatcher.

use tokio::sync::mpsc;

// ---

use super::AgentCmd;
use super::SessionManagerHandle;

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

pub struct Agent {
    // ---
    cmd_rx: mpsc::Receiver<AgentCmd>,
    sm: SessionManagerHandle,
}

// ---

impl Agent {
    // ---
    pub fn new(cmd_rx: mpsc::Receiver<AgentCmd>, sm: SessionManagerHandle) -> Self {
        Self { cmd_rx, sm }
    }

    // ---

    pub async fn run(mut self) {
        // ---
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                AgentCmd::StreamStart {
                    uuid,
                    info,
                    priority,
                } => {
                    tracing::info!(%uuid, ?priority, "stream_start → session manager");
                    self.sm.stream_start(uuid, info, priority).await;
                }
            }
        }

        tracing::info!("agent loop exiting");
    }
}
