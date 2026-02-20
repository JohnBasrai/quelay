//! QUIC transport demo — server and client on loopback exchange a real
//! payload over UDP using `QuicTransport`.
//!
//! Flow:
//!   1. Server generates a self-signed TLS cert.
//!   2. Server binds on 127.0.0.1:0 (OS picks the port).
//!   3. Client is constructed with the server's cert DER (pinned trust).
//!   4. Client connects and opens a bidirectional stream.
//!   5. Client writes payload and shuts down the send half.
//!   6. Server accepts the stream and reads to EOF.
//!   7. Both sides assert the payload matches.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use quelay_domain::{Priority, QueLaySession, QueLayTransport};
use quelay_quic::{CertBundle, QuicTransport};

// ---

pub async fn run() {
    // ---
    let payload = b"hello from quelay over QUIC";

    // --- server setup -------------------------------------------------------

    let bundle = CertBundle::generate("quelay").expect("cert generation failed");
    let cert_der = bundle.cert_der.clone();

    // Bind on an OS-assigned loopback port.
    let bind_addr = "127.0.0.1:0".parse().unwrap();
    let server_transport =
        QuicTransport::server(bundle, bind_addr).expect("server transport failed");

    // listen() returns a channel of incoming sessions. The bind addr passed
    // here is ignored — the endpoint is already bound in server().
    let mut session_rx = server_transport
        .listen(bind_addr)
        .await
        .expect("listen failed");

    // Discover the actual bound port so the client can connect to it.
    let server_addr = server_transport.local_addr().expect("local_addr failed");

    println!("server listening on {server_addr}");

    // --- server task --------------------------------------------------------

    let server = tokio::spawn(async move {
        // ---
        let session = session_rx.recv().await.expect("no incoming session");

        let mut stream = session.accept_stream().await.expect("accept_stream failed");

        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .expect("read_to_end failed");

        buf
    });

    // --- client -------------------------------------------------------------

    let client_transport =
        QuicTransport::client(cert_der, "quelay".into()).expect("client transport failed");

    let session = client_transport
        .connect(server_addr)
        .await
        .expect("connect failed");

    let mut stream = session
        .open_stream(Priority::C2I)
        .await
        .expect("open_stream failed");

    // quinn requires the opener to write before the peer can accept_bi().
    stream.write_all(payload).await.expect("write_all failed");

    stream.shutdown().await.expect("shutdown failed");

    // --- verify -------------------------------------------------------------

    let received = server.await.expect("server task panicked");

    assert_eq!(received.as_slice(), payload);
    println!(
        "sent {} bytes, received {} bytes — payload matches: {}",
        payload.len(),
        received.len(),
        received.as_slice() == payload,
    );
}
