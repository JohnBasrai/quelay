//! LinkSim transport demo — sender and receiver exchange a payload over an
//! in-process simulated link, exercising the domain/transport layer end to end.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use quelay_domain::{LinkState, Priority, QueLaySession};
use quelay_link_sim::{LinkSimConfig, LinkSimTransport};

// ---

pub async fn run() {
    // ---
    let transport = LinkSimTransport::new(LinkSimConfig::default());
    let (sender_session, receiver_session) = transport.connected_pair();

    // Check initial link state on both ends.
    assert_eq!(sender_session.link_state(), LinkState::Normal);
    assert_eq!(receiver_session.link_state(), LinkState::Normal);
    println!("link state: {:?}", sender_session.link_state());

    let payload = b"hello from quelay sender";

    // Spawn receiver task first so it is ready before the sender opens.
    let receiver = tokio::spawn(async move {
        // ---
        let mut stream = receiver_session
            .accept_stream()
            .await
            .expect("accept_stream failed");

        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .expect("read_to_end failed");

        buf
    });

    // Sender opens a stream, writes the payload, and closes.
    let mut stream = sender_session
        .open_stream(Priority::C2I)
        .await
        .expect("open_stream failed");

    stream.write_all(payload).await.expect("write_all failed");

    stream.shutdown().await.expect("shutdown failed");

    let received = receiver.await.expect("receiver task panicked");

    assert_eq!(received.as_slice(), payload);
    println!(
        "sent {} bytes, received {} bytes — payload matches: {}",
        payload.len(),
        received.len(),
        received.as_slice() == payload,
    );
}
