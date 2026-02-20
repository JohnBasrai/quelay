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
    pub fn server(bundle: CertBundle, bind_addr: SocketAddr) -> Result<Self> {
        let tls = server_config(&bundle).map_err(QueLayError::from)?;

        let quinn_tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        let scfg = quinn::ServerConfig::with_crypto(Arc::new(quinn_tls));

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
    ) -> Result<Self> {
        // ---
        let tls = client_config(server_cert_der).map_err(QueLayError::from)?;

        let quinn_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| QueLayError::Transport(e.to_string()))?;

        let ccfg = quinn::ClientConfig::new(Arc::new(quinn_tls));

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e: std::io::Error| QueLayError::Transport(e.to_string()))?;

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
