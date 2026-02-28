//! Thrift service handler for the `QueLayAgent` service.
//!
//! The Thrift runtime calls these methods synchronously on a thread pool.
//! Each method that needs async work blocks on the tokio runtime handle.
//!
//! # Runtime configuration
//!
//! [`RuntimeConfig`] holds values that can be changed live via C2I calls
//! (`set_max_concurrent`, `set_chunk_size_bytes`).  It is wrapped in
//! `Arc<std::sync::Mutex<RuntimeConfig>>` so it can be shared between the
//! Thrift handler (which writes it) and the session manager / active-stream
//! tasks (which read it when spawning new streams).
//!
//! The startup defaults come from [`Config`] and are set in `main.rs` before
//! the handler is constructed.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use uuid::Uuid;

use quelay_domain::{
    // ---
    LinkState,
    Priority,
    StreamInfo as DomainStreamInfo,
};

use quelay_thrift::{
    // ---
    LinkState as WireLinkState,
    QueLayAgentSyncHandler,
    StartStreamReturn,
    StreamInfo as WireStreamInfo,
    IDL_VERSION,
};

// ---

use super::{CallbackCmd, CallbackTx};
use crate::config::{DEFAULT_CHUNK_SIZE_BYTES, DEFAULT_MAX_CONCURRENT};

// ---------------------------------------------------------------------------
// RuntimeConfig
// ---------------------------------------------------------------------------

/// Mutable agent configuration that can be updated via C2I at runtime.
///
/// Wrap in `Arc<std::sync::Mutex<RuntimeConfig>>` and share between
/// [`AgentHandler`] (writer) and the session manager / active-stream tasks
/// (readers).
///
/// All fields that the test/debug C2I calls modify live here.  Production
/// configuration that is set once at startup and never changed stays in
/// [`Config`].
///
/// # Lock discipline
///
/// This mutex is held only briefly for reads/writes of primitive values.
/// Never hold it across an `await` point.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    // ---
    /// Configured uplink BW cap in Mbit/s (0 = uncapped).
    ///
    /// Read-only at runtime — `get_bandwidth_cap_mbps` reports this value.
    /// Changing BW cap live is not supported; restart the agent with a new
    /// `--bw-cap-mbps` to change it.
    pub bw_cap_mbps: u64,

    /// Chunk payload size in bytes for new uplink streams.
    ///
    /// Updated by `set_chunk_size_bytes`.  Only affects streams started
    /// *after* the call; in-flight streams are not affected.
    pub chunk_size_bytes: usize,

    /// Maximum concurrent active streams (0 = unlimited).
    ///
    /// Updated by `set_max_concurrent`.  Evaluated by the scheduler when
    /// deciding whether to start the next queued stream.
    pub max_concurrent: usize,
}

// ---

impl RuntimeConfig {
    // ---
    pub fn new(bw_cap_mbps: u64, chunk_size_bytes: usize, max_concurrent: usize) -> Self {
        Self {
            bw_cap_mbps,
            chunk_size_bytes,
            max_concurrent,
        }
    }
}

/// Shared handle to the live runtime configuration.
pub type RuntimeConfigHandle = Arc<std::sync::Mutex<RuntimeConfig>>;

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

    /// Test/debug only — update the max-concurrent limit in the scheduler.
    /// Must not be exposed in production builds.
    SetMaxConcurrent(usize),
}

// ---------------------------------------------------------------------------
// AgentHandler
// ---------------------------------------------------------------------------

/// Implements `QueLayAgentSyncHandler` — the generated Thrift service trait.
pub struct AgentHandler {
    // ---
    rt: Handle,
    cmd_tx: mpsc::Sender<AgentCmd>,
    link_state: Arc<AsyncMutex<LinkState>>,
    cb_tx: CallbackTx,
    /// Live runtime configuration — shared with the session manager.
    runtime_cfg: RuntimeConfigHandle,
}

// ---

impl AgentHandler {
    // ---
    pub fn new(
        rt: Handle,
        cmd_tx: mpsc::Sender<AgentCmd>,
        link_state: Arc<AsyncMutex<LinkState>>,
        cb_tx: CallbackTx,
        runtime_cfg: RuntimeConfigHandle,
    ) -> Self {
        Self {
            rt,
            cmd_tx,
            link_state,
            cb_tx,
            runtime_cfg,
        }
    }

    /// Locks `runtime_cfg` and runs `f`, mapping lock poisoning to a
    /// Thrift `InternalError` instead of panicking.
    ///
    /// Use this in RPC handlers to keep poison handling consistent and
    /// avoid unwrap/expect on the config mutex.
    fn lock_runtime_cfg(&self) -> thrift::Result<std::sync::MutexGuard<'_, RuntimeConfig>> {
        // ---
        self.runtime_cfg.lock().map_err(|_| {
            thrift::Error::Application(thrift::ApplicationError::new(
                thrift::ApplicationErrorKind::InternalError,
                "runtime_cfg lock poisoned",
            ))
        })
    }
    fn send_cmd(&self, cmd: AgentCmd) -> thrift::Result<()> {
        // ---
        self.rt.block_on(async {
            self.cmd_tx.send(cmd).await.map_err(|_| {
                thrift::Error::Application(thrift::ApplicationError::new(
                    thrift::ApplicationErrorKind::InternalError,
                    "agent loop has shut down",
                ))
            })
        })
    }

    fn send_callback_cmd(&self, cmd: CallbackCmd) -> thrift::Result<()> {
        // ---
        let delivered = self.rt.block_on(async { self.cb_tx.send(cmd).await });

        if delivered {
            Ok(())
        } else {
            Err(thrift::Error::Application(thrift::ApplicationError::new(
                thrift::ApplicationErrorKind::InternalError,
                "callback channel closed",
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// QueLayAgentSyncHandler impl
// ---------------------------------------------------------------------------

impl QueLayAgentSyncHandler for AgentHandler {
    // ---

    fn handle_get_version(&self) -> thrift::Result<String> {
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

        tracing::debug!(uuid = %uuid_str, priority, "THFT: stream_start");

        let uuid = Uuid::parse_str(&uuid_str).map_err(|e| {
            thrift::Error::Application(thrift::ApplicationError::new(
                thrift::ApplicationErrorKind::InvalidTransform,
                format!("thrift_srv: Invalid UUID:{uuid_str}:{e}"),
            ))
        })?;

        let attrs: HashMap<String, String> = info.attrs.unwrap_or_default().into_iter().collect();

        let domain_info = DomainStreamInfo {
            size_bytes: info.size_bytes.map(|v| v as u64),
            attrs,
        };

        let domain_priority = Priority::from_i8(priority);

        self.send_cmd(AgentCmd::StreamStart {
            uuid,
            info: domain_info,
            priority: domain_priority,
        })?;

        Ok(StartStreamReturn {
            err_msg: Some(String::new()),
            queue_position: Some(0),
            pending_queue: Some(Vec::new()),
        })
    }

    // ---

    fn handle_set_callback(&self, endpoint: String) -> thrift::Result<String> {
        // ---

        tracing::debug!(%endpoint, "callback endpoint registered");

        self.send_callback_cmd(CallbackCmd::Register(endpoint))?;

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

    // ---

    fn handle_get_bandwidth_cap_mbps(&self) -> thrift::Result<i32> {
        // ---

        let guard = self.lock_runtime_cfg()?;
        let cap = guard.bw_cap_mbps;

        tracing::debug!(cap, "get_bandwidth_cap_mbps");

        Ok(cap as i32)
    }

    // -----------------------------------------------------------------------
    // Test / debug handlers — disabled in production builds
    // -----------------------------------------------------------------------

    fn handle_link_enable(&self, enabled: bool) -> thrift::Result<()> {
        // ---

        tracing::info!(enabled, "link_enable (test/debug)");
        self.send_cmd(AgentCmd::LinkEnable(enabled))
    }

    // ---

    fn handle_set_max_concurrent(&self, n: i32) -> thrift::Result<()> {
        // ---

        let n = n as usize;
        tracing::info!(n, "set_max_concurrent (test/debug)");

        {
            let mut cfg = self.lock_runtime_cfg()?;
            cfg.max_concurrent = if n == 0 { DEFAULT_MAX_CONCURRENT } else { n };
        }

        self.send_cmd(AgentCmd::SetMaxConcurrent(n))
    }

    // ---

    fn handle_set_chunk_size_bytes(&self, n: i32) -> thrift::Result<()> {
        // ---

        let requested = n as usize;
        tracing::info!(requested, "set_chunk_size_bytes (test/debug)");

        let effective = if requested == 0 {
            DEFAULT_CHUNK_SIZE_BYTES
        } else {
            requested
        };

        if effective > 65_535 {
            return Err(thrift::Error::Application(thrift::ApplicationError::new(
                thrift::ApplicationErrorKind::InvalidTransform,
                format!("chunk_size_bytes {effective} exceeds u16 max (65535)"),
            )));
        }

        {
            let mut cfg = self.lock_runtime_cfg()?;
            cfg.chunk_size_bytes = effective;
        }

        Ok(())
    }
}
