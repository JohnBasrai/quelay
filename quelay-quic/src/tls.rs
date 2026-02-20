//! TLS configuration for the QUIC transport.
//!
//! Generates self-signed certificates at startup using `rcgen`.
//! The server cert DER is shared out-of-band to the client so it
//! can pin exactly that cert in its trust store — no CA needed.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::error::QuicError;

// ---------------------------------------------------------------------------
// CertBundle
// ---------------------------------------------------------------------------

/// A self-signed TLS certificate and its private key, generated at startup.
///
/// The server creates one `CertBundle` and passes `cert_der.clone()` to
/// the client via the `QuicTransport` constructor so the client can pin it.
pub struct CertBundle {
    // ---
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivatePkcs8KeyDer<'static>,
}

impl CertBundle {
    // ---
    /// Generate a new self-signed certificate valid for `server_name`.
    ///
    /// For the 2-node demo `server_name` can be any string, e.g. `"quelay"`.
    /// The client must use the same string when connecting.
    pub fn generate(server_name: &str) -> Result<Self, QuicError> {
        // ---
        let cert = rcgen::generate_simple_self_signed(vec![server_name.to_string()])
            .map_err(|e| QuicError::Tls(e.to_string()))?;

        let cert_der = CertificateDer::from(cert.cert);
        let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

        Ok(Self { cert_der, key_der })
    }
}

// ---------------------------------------------------------------------------
// Server TLS config
// ---------------------------------------------------------------------------

/// Build a `rustls::ServerConfig` from a [`CertBundle`].
pub fn server_config(bundle: &CertBundle) -> Result<rustls::ServerConfig, QuicError> {
    // ---
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![bundle.cert_der.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(bundle.key_der.clone_key()),
        )
        .map_err(|e| QuicError::Tls(e.to_string()))?;

    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Client TLS config
// ---------------------------------------------------------------------------

/// Build a `rustls::ClientConfig` that pins a single trusted server cert.
///
/// `server_cert_der` is the DER bytes of the server's self-signed cert,
/// obtained out-of-band (e.g. passed directly in the demo, or exchanged
/// via a side channel in production).
pub fn client_config(
    server_cert_der: CertificateDer<'static>,
) -> Result<rustls::ClientConfig, QuicError> {
    // ---
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(server_cert_der)
        .map_err(|e| QuicError::Tls(e.to_string()))?;

    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(roots))
        .with_no_client_auth();

    Ok(cfg)
}
