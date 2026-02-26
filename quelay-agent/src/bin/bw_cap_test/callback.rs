//! Callback actor — dumb conduit between agent callbacks and CIC.
//!
//! Each instance is tagged with a [`Role`] at construction time so CIC knows
//! which side (air/ground) a message originated from.  No processing occurs
//! here; every callback is forwarded verbatim to the CIC channel.

use std::sync::Mutex;

// ---

use tokio::sync::mpsc;

// ---

use quelay_thrift::{
    // ---
    FailReason,
    LinkState,
    ProgressInfo,
    QueLayCallbackSyncHandler,
    QueueStatus,
    StreamInfo,
};

// ---

use super::CicMsg;

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

/// Identifies which side of the link a callback or tuner task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    // ---
    /// Air-side agent (data source / sender).
    Sender,

    /// Ground-side agent (data sink / receiver).
    Receiver,
}

// ---------------------------------------------------------------------------
// CallbackActor
// ---------------------------------------------------------------------------

/// Thrift callback handler that forwards every event to the CIC channel.
///
/// Acts as a pure conduit: no logic, no filtering — all routing decisions
/// belong to the CIC.
pub struct CallbackActor {
    // ---
    role: Role,
    cic_tx: Mutex<mpsc::Sender<CicMsg>>,
}

// ---

impl CallbackActor {
    // ---

    /// Create a new actor tagged with `role`, forwarding to `cic_tx`.
    pub fn new(role: Role, cic_tx: mpsc::Sender<CicMsg>) -> Self {
        // ---
        Self {
            role,
            cic_tx: Mutex::new(cic_tx),
        }
    }

    fn forward(&self, msg: CicMsg) {
        // ---
        // Best-effort: if CIC has shut down we drop silently.
        let _ = self.cic_tx.lock().unwrap().try_send(msg);
    }
}

// ---

impl QueLayCallbackSyncHandler for CallbackActor {
    // ---

    fn handle_ping(&self) -> thrift::Result<()> {
        // ---
        Ok(())
    }

    fn handle_stream_started(
        &self,
        uuid: String,
        info: StreamInfo,
        port: i32,
    ) -> thrift::Result<()> {
        // ---
        self.forward(CicMsg::StreamStarted {
            role: self.role,
            uuid,
            info,
            port: port as u16,
        });
        Ok(())
    }

    fn handle_stream_progress(&self, _uuid: String, _progress: ProgressInfo) -> thrift::Result<()> {
        // ---
        Ok(())
    }

    fn handle_stream_done(&self, uuid: String, bytes_transferred: i64) -> thrift::Result<()> {
        // ---
        self.forward(CicMsg::StreamDone {
            role: self.role,
            uuid,
            bytes: bytes_transferred as u64,
        });
        Ok(())
    }

    fn handle_stream_failed(
        &self,
        uuid: String,
        _code: FailReason,
        reason: String,
    ) -> thrift::Result<()> {
        // ---
        self.forward(CicMsg::StreamFailed {
            role: self.role,
            uuid,
            reason,
        });
        Ok(())
    }

    fn handle_link_status_update(&self, state: LinkState) -> thrift::Result<()> {
        // ---
        self.forward(CicMsg::LinkStatus {
            role: self.role,
            state,
        });
        Ok(())
    }

    fn handle_queue_status_update(&self, _status: QueueStatus) -> thrift::Result<()> {
        // ---
        Ok(())
    }
}
