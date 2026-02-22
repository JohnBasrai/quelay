//! Thrift service handler for the `QueLayAgent` service.
//!
//! The Thrift runtime calls these methods synchronously on a thread pool.
//! Each method that needs async work blocks on the tokio runtime handle.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use quelay_domain::{
    // ---
    LinkState,
    Priority,
    StreamInfo as DomainStreamInfo,
};

use quelay_thrift::{
    LinkState as WireLinkState,
    QueLayAgentSyncHandler,
    StartStreamReturn,
    StreamInfo as WireStreamInfo,
    // ---
    IDL_VERSION,
};

// ---

use super::{CallbackCmd, CallbackTx};

// ---------------------------------------------------------------------------
// AgentCmd
// ---------------------------------------------------------------------------

/// Commands the Thrift handler sends to the async agent loop.
#[derive(Debug)]
pub enum AgentCmd {
    // ---
    StreamStart {
        uuid: Uuid,
        info: DomainStreamInfo,
        priority: Priority,
    },

    /// Test/debug only — enable or disable the QUIC link.
    /// Must not be exposed in production builds.
    LinkEnable(bool),
}

// ---------------------------------------------------------------------------
// AgentHandler
// ---------------------------------------------------------------------------

/// Implements `QueLayAgentSyncHandler` — the generated Thrift service trait.
pub struct AgentHandler {
    // ---
    rt: Handle,
    cmd_tx: mpsc::Sender<AgentCmd>,
    link_state: Arc<Mutex<LinkState>>,
    cb_tx: CallbackTx,
}

// ---

impl AgentHandler {
    // ---
    pub fn new(
        rt: Handle,
        cmd_tx: mpsc::Sender<AgentCmd>,
        link_state: Arc<Mutex<LinkState>>,
        cb_tx: CallbackTx,
    ) -> Self {
        Self {
            rt,
            cmd_tx,
            link_state,
            cb_tx,
        }
    }
}

// ---------------------------------------------------------------------------
// QueLayAgentSyncHandler impl
// ---------------------------------------------------------------------------

impl QueLayAgentSyncHandler for AgentHandler {
    // ---
    fn handle_get_version(&self) -> thrift::Result<String> {
        // ---
        tracing::debug!("get_version");
        Ok(IDL_VERSION.to_string())
    }

    // ---

    fn handle_stream_start(
        &self,
        uuid_str: String,
        info: WireStreamInfo,
        priority: i8,
    ) -> thrift::Result<StartStreamReturn> {
        // ---
        tracing::info!(uuid = %uuid_str, priority, "stream_start");

        let uuid = Uuid::parse_str(&uuid_str).map_err(|e| {
            thrift::Error::Application(thrift::ApplicationError::new(
                thrift::ApplicationErrorKind::InvalidTransform,
                e.to_string(),
            ))
        })?;

        let attrs: HashMap<String, String> = info.attrs.unwrap_or_default().into_iter().collect();

        let domain_info = DomainStreamInfo {
            // ---
            size_bytes: info.size_bytes.map(|v| v as u64),
            attrs,
        };

        let domain_priority = Priority::from_i8(priority);

        let cmd = AgentCmd::StreamStart {
            uuid,
            info: domain_info,
            priority: domain_priority,
        };

        self.rt
            .block_on(self.cmd_tx.send(cmd))
            .ok()
            .ok_or_else(|| {
                thrift::Error::Application(thrift::ApplicationError::new(
                    thrift::ApplicationErrorKind::InternalError,
                    "agent loop has shut down".to_string(),
                ))
            })?;

        Ok(StartStreamReturn {
            err_msg: Some(String::new()),
            queue_position: Some(0),
        })
    }

    // ---

    fn handle_set_callback(&self, endpoint: String) -> thrift::Result<String> {
        // ---
        tracing::info!(%endpoint, "callback endpoint registered");

        self.rt
            .block_on(self.cb_tx.send(CallbackCmd::Register(endpoint)));
        Ok(String::new())
    }

    // ---

    fn handle_get_link_state(&self) -> thrift::Result<WireLinkState> {
        // ---

        let state = self.rt.block_on(async { *self.link_state.lock().await });
        tracing::debug!(?state, "get_link_state");

        Ok(match state {
            LinkState::Connecting => WireLinkState::CONNECTING,
            LinkState::Normal => WireLinkState::NORMAL,
            LinkState::Degraded => WireLinkState::DEGRADED,
            LinkState::Failed => WireLinkState::FAILED,
        })
    }

    // -----------------------------------------------------------------------
    // Test / debug handlers — disabled in production builds
    // -----------------------------------------------------------------------

    fn handle_link_enable(&self, enabled: bool) -> thrift::Result<()> {
        // ---
        tracing::info!(enabled, "link_enable (test/debug)");
        let cmd = AgentCmd::LinkEnable(enabled);
        self.rt.block_on(async {
            let _ = self.cmd_tx.send(cmd).await;
        });
        Ok(())
    }
}
