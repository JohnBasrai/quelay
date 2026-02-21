//! Quelay agent daemon.
//!
//! Bridges local Thrift C2I clients to a remote Quelay peer over QUIC.
//!
//! Usage:
//!   quelay-agent server --bind 0.0.0.0:5000
//!   quelay-agent client --peer 192.168.1.2:5000 --cert /tmp/quelay-server.der

use std::fs;
use std::sync::Arc;

// ---

use clap::Parser;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

// ---

use quelay_domain::{LinkState, QueLayTransport};
use quelay_quic::{CertBundle, QuicTransport};
use quelay_thrift::{
    QueLayAgentSyncProcessor, TBinaryInputProtocolFactory, TBinaryOutputProtocolFactory,
    TBufferedReadTransportFactory, TBufferedWriteTransportFactory, TServer, IDL_VERSION,
};

// ---

mod active_stream;
mod agent;
mod callback;
mod config;
mod framing;
mod session_manager;
mod thrift_srv;

// ---

use agent::Agent;
use callback::{spawn_ping_timer, CallbackAgent};
use config::{Config, Mode};
use session_manager::{SessionManager, TransportConfig};
use thrift_srv::AgentHandler;

// Gateway re-exports — siblings import via super::Symbol per EMBP §2.3
pub use active_stream::ActiveStream;
pub use callback::{CallbackCmd, CallbackTx};
pub use framing::{
    // ---
    read_header,
    read_wormhole_msg,
    write_header,
    write_wormhole_msg,
    StreamHeader,
    WormholeMsg,
};
pub use session_manager::SessionManagerHandle;
pub use thrift_srv::AgentCmd;

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ---

    let cfg = Config::parse();

    let no_color = std::env::var("EMACS").is_ok()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CARGO_TERM_COLOR").as_deref() == Ok("never")
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());

    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(!no_color)
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        idl_version = IDL_VERSION,
        "quelay-agent starting",
    );

    // Ensure spool directory exists.
    fs::create_dir_all(&cfg.spool_dir)?;
    info!(spool_dir = %cfg.spool_dir.display(), "spool directory ready");

    let link_state = Arc::new(Mutex::new(LinkState::Connecting));
    let (cmd_tx, cmd_rx) = mpsc::channel(64);

    // Spawn the callback agent thread and the 60-second ping timer.
    let cb_tx = CallbackAgent::spawn();
    spawn_ping_timer(cb_tx.clone(), std::time::Duration::from_secs(60));

    // Establish the initial QUIC session and build the transport config
    // the session manager needs for reconnection.
    let (initial_session, transport_cfg) = match &cfg.mode {
        Mode::Server { bind } => {
            info!("server mode, binding QUIC on {bind}");

            let bundle = CertBundle::generate("quelay")?;
            let cert_der = bundle.cert_der.clone();

            let cert_path = std::env::current_dir()?.join("quelay-server.der");
            fs::write(&cert_path, cert_der.as_ref())?;
            info!("server cert written to {}", cert_path.display());

            let transport = QuicTransport::server(bundle, *bind)?;
            let mut sess_rx = transport.listen(*bind).await?;

            info!("waiting for client connection...");
            let session = sess_rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("no incoming QUIC session"))?;
            info!("client connected");

            // Pass sess_rx into TransportConfig so the session manager can
            // call recv() again on reconnect — no second listen() needed.
            let tcfg = TransportConfig::Server { sess_rx };
            (Arc::new(session) as quelay_domain::QueLaySessionPtr, tcfg)
        }

        Mode::Client {
            peer,
            server_name,
            cert,
        } => {
            info!("client mode, connecting to peer {peer}");

            let cert_bytes = fs::read(cert)?;
            let cert_der = rustls_pki_types::CertificateDer::from(cert_bytes);

            let transport = QuicTransport::client(cert_der.clone(), server_name.clone())?;
            let session = transport.connect(*peer).await?;
            info!("connected to {peer}");

            let tcfg = TransportConfig::Client {
                peer: *peer,
                server_name: server_name.clone(),
                cert_der,
            };
            (Arc::new(session) as quelay_domain::QueLaySessionPtr, tcfg)
        }
    };

    // Boot the session manager.
    let sm = Arc::new(SessionManager::new(
        initial_session,
        transport_cfg,
        link_state.clone(),
        cfg.spool_dir.clone(),
        cb_tx.clone(),
    ));
    let sm_handle = SessionManagerHandle::new(sm.clone());

    // Reconnection loop runs independently.
    tokio::spawn(sm.run());

    // Agent dispatches Thrift commands to the session manager.
    let agent = Agent::new(cmd_rx, sm_handle);
    tokio::spawn(agent.run());

    // Thrift C2I server on a blocking thread pool.
    let rt = tokio::runtime::Handle::current();
    let handler = AgentHandler::new(rt, cmd_tx, link_state, cb_tx);
    let processor = QueLayAgentSyncProcessor::new(handler);
    let agent_endpoint = cfg.agent_endpoint.to_string();

    info!("Thrift C2I listening on {agent_endpoint}");

    tokio::task::spawn_blocking(move || {
        let mut server = TServer::new(
            TBufferedReadTransportFactory::new(),
            TBinaryInputProtocolFactory::new(),
            TBufferedWriteTransportFactory::new(),
            TBinaryOutputProtocolFactory::new(),
            processor,
            10,
        );
        if let Err(e) = server.listen(&agent_endpoint) {
            tracing::error!("Thrift server error: {e}");
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("shutting down");

    Ok(())
}
