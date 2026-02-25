//! QUIC transport implementation for Quelay over constrained links.
//!
//! Implements [`quelay_domain::QueLayTransport`] using the `quinn` crate.
//!
//! # Quick start
//!
//! ```ignore
//! // --- server side ---
//! use quelay_quic::{CertBundle, QuicTransport};
//!
//! let bundle      = CertBundle::generate("quelay")?;
//! let cert_der    = bundle.cert_der.clone();
//! let transport   = QuicTransport::server(bundle, "0.0.0.0:5000".parse()?)?;
//! let mut sess_rx = transport.listen("0.0.0.0:5000".parse()?).await?;
//! // share cert_der with the client...
//!
//! // --- client side ---
//! use quelay_quic::QuicTransport;
//!
//! let transport = QuicTransport::client(cert_der, "quelay".into())?;
//! let session   = transport.connect("192.168.1.2:5000".parse()?).await?;
//! ```

mod error;
mod session;
mod stream;
mod tls;
mod transport;

pub use error::QuicError;
pub use session::QuicSession;
pub use stream::{QuicRecvHalf, QuicSendHalf};
pub use tls::CertBundle;
pub use transport::QuicTransport;
