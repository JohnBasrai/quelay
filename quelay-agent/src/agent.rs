//! [`Agent`] — processes commands from the Thrift handler and delegates to
//! [`SessionManagerHandle`].
//!
//! `Agent` no longer owns a `QueLaySessionPtr` directly.  All session
//! interaction (stream open, reconnect, pending queue) is the session
//! manager's responsibility.  `Agent` is now a thin dispatcher.

use tokio::sync::mpsc;

// ---

use super::{AgentCmd, SessionCommand, SessionCommandQueue};

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

pub struct Agent {
    // ---
    cmd_rx: mpsc::Receiver<AgentCmd>,
    sm_cmd_tx: SessionCommandQueue,
}

// ---

impl Agent {
    // ---
    pub fn new(cmd_rx: mpsc::Receiver<AgentCmd>, sm_cmd_tx: SessionCommandQueue) -> Self {
        Self { cmd_rx, sm_cmd_tx }
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
                    reply_tx,
                } => {
                    // ---
                    tracing::debug!(%uuid, ?priority, "stream_start → session manager");

                    let _ = self
                        .sm_cmd_tx
                        .send(SessionCommand::StreamStart {
                            uuid,
                            info,
                            priority,
                            reply_tx,
                        })
                        .await;
                }

                AgentCmd::GetConnStats { reply_tx } => {
                    let _ = self
                        .sm_cmd_tx
                        .send(SessionCommand::GetConnStats { reply_tx })
                        .await;
                }

                #[cfg(feature = "test-hooks")]
                AgentCmd::LinkEnable(enabled) => {
                    // Forward to SessionManager via command channel
                    let _ = self
                        .sm_cmd_tx
                        .send(SessionCommand::LinkEnable(enabled))
                        .await;
                }

                // RuntimeConfig is updated in-place by AgentHandler before
                // this command is sent.  The session manager reads the new
                // value on the next stream_start; no further action needed
                // here beyond the log line.
                #[cfg(feature = "test-hooks")]
                AgentCmd::SetMaxConcurrent(n) => {
                    tracing::info!(n, "set_max_concurrent (test/debug)");
                    // n == 0 means "restore default / unlimited"; propagate as None.
                    let limit = if n == 0 { None } else { Some(n) };
                    let _ = self
                        .sm_cmd_tx
                        .send(SessionCommand::SetMaxConcurrent(limit))
                        .await;
                }
            }
        }

        tracing::debug!("agent loop exiting");
    }
}
