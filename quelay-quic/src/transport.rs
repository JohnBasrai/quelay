//! [`QuicTransport`] — factory for [`QuicSession`]s.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use quelay_domain::{
    // ---
    QueLayError,
    QueLayTransport,
    Result,
};

use crate::session::QuicSession;
use crate::tls::{client_config, server_config, CertBundle};

// ---------------------------------------------------------------------------
// CongestionAlgo
// ---------------------------------------------------------------------------

/// Which QUIC congestion-control algorithm to install on the endpoint.
///
/// Mirrors `quelay_agent::config::CongestionAlgo` so `quelay-quic` stays
/// free of a direct dependency on the agent crate.  Callers convert with
/// `.into()` or pass the value directly.
#[derive(Debug, Clone, Default)]
pub enum CongestionAlgo {
    #[default]
    NewReno,
    Bbr,
    Cubic,
}

/// Build a `quinn::TransportConfig` with the requested congestion controller.
fn make_transport_config(algo: &CongestionAlgo) -> quinn::TransportConfig {
    // ---
    let mut tc = quinn::TransportConfig::default();

    // Satellite links have high RTT and reconnect windows that far exceed
    // quinn's 30s default idle timeout.  Set a generous timeout so a
    // temporarily stalled transfer (e.g. spool-full back-pressure or a
    // reconnect cycle) does not terminate the QUIC connection.
    tc.max_idle_timeout(Some(
        quinn::VarInt::from_u32(300_000) // 300 s in milliseconds
            .into(),
    ));

    match algo {
        CongestionAlgo::NewReno => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()));
        }
        CongestionAlgo::Bbr => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        }
        CongestionAlgo::Cubic => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default()));
        }
    }
    tc
}

// ---------------------------------------------------------------------------
// QuicTransport
// ---------------------------------------------------------------------------

pub struct QuicTransport {
    // ---
    endpoint: quinn::Endpoint,
    server_name: Option<String>,
}

// ---

impl QuicTransport {
    // ---
    /// Create a server-side transport bound to `bind_addr`.
    pub fn server(bundle: CertBundle, bind_addr: SocketAddr, algo: CongestionAlgo) -> Result<Self> {
        let tls = server_config(&bundle).map_err(QueLayError::from)?;

        let quinn_tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        let mut scfg = quinn::ServerConfig::with_crypto(Arc::new(quinn_tls));
        scfg.transport_config(Arc::new(make_transport_config(&algo)));

        let endpoint = quinn::Endpoint::server(scfg, bind_addr)
            .map_err(|e: std::io::Error| QueLayError::Transport(e.to_string()))?;

        Ok(Self {
            endpoint,
            server_name: None,
        })
    }

    // ---

    /// Create a client-side transport.
    ///
    /// `server_cert_der` — the server's self-signed cert DER obtained
    /// out-of-band. `server_name` must match the name used when the server
    /// generated its cert.
    pub fn client(
        server_cert_der: rustls_pki_types::CertificateDer<'static>,
        server_name: String,
        algo: CongestionAlgo,
    ) -> Result<Self> {
        // ---
        let tls = client_config(server_cert_der).map_err(QueLayError::from)?;

        let quinn_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        let mut ccfg = quinn::ClientConfig::new(Arc::new(quinn_tls));
        ccfg.transport_config(Arc::new(make_transport_config(&algo)));

        let bind_addr: SocketAddr = "0.0.0.0:0"
            .parse::<std::net::SocketAddr>()
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        let mut endpoint = quinn::Endpoint::client(bind_addr)
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        endpoint.set_default_client_config(ccfg);

        Ok(Self {
            endpoint,
            server_name: Some(server_name),
        })
    }

    // ---

    /// Return the local address the endpoint is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        // ---
        self.endpoint.local_addr()
    }
}

// ---

#[async_trait]
impl QueLayTransport for QuicTransport {
    // ---
    type Session = QuicSession;

    async fn connect(&self, remote: SocketAddr) -> Result<QuicSession> {
        // ---
        let server_name = self.server_name.as_deref().ok_or_else(|| {
            QueLayError::Transport("connect() called on server-side transport".into())
        })?;

        let conn = self
            .endpoint
            .connect(remote, server_name)
            .map_err(|e| QueLayError::Transport(e.to_string()))?
            .await
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        Ok(QuicSession::new(conn))
    }

    // ---

    async fn listen(&self, _bind: SocketAddr) -> Result<mpsc::Receiver<QuicSession>> {
        // ---
        let endpoint = self.endpoint.clone();
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let session = QuicSession::new(conn);
                            tx.send(session).await.ok();
                        }
                        Err(e) => {
                            tracing::warn!("incoming connection failed: {e}");
                        }
                    }
                });
            }
        });

        Ok(rx)
    }
}
