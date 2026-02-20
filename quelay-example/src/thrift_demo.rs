//! Thrift mapping demo — shows round-trip conversion between domain types
//! and Thrift wire types using the `quelay-thrift` mapping layer.
//!
//! This does not start a real Thrift server; it exercises the mapping layer
//! in isolation to validate that types survive a domain → wire → domain trip.

use std::collections::HashMap;

use uuid::Uuid;

use quelay_domain::{
    // ---
    LinkState as DomainLinkState,
    QueueStatus as DomainQueueStatus,
    StreamInfo as DomainStreamInfo,
    TransferProgress,
};
use quelay_thrift::progress_to_wire;
use quelay_thrift::{
    // ---
    LinkState as WireLinkState,
    QueueStatus as WireQueueStatus,
    StreamInfo as WireStreamInfo,
};

// ---

pub fn run() {
    // ---
    round_trip_link_state();
    round_trip_stream_info();
    round_trip_queue_status();
    round_trip_progress();
}

// ---------------------------------------------------------------------------

fn round_trip_link_state() {
    // ---
    let cases = [
        DomainLinkState::Connecting,
        DomainLinkState::Normal,
        DomainLinkState::Degraded,
        DomainLinkState::Failed,
    ];

    for original in cases {
        let wire: WireLinkState = original.into();
        let restored: DomainLinkState = wire.into();
        assert_eq!(original, restored);
    }

    println!("LinkState round-trip: OK");
}

// ---------------------------------------------------------------------------

fn round_trip_stream_info() {
    // ---
    let original = DomainStreamInfo {
        size_bytes: Some(1024 * 1024),
        attrs: HashMap::from([
            ("filename".into(), "telemetry-2026.bin".into()),
            ("sha256".into(), "abc123def456".into()),
            ("content_type".into(), "application/octet-stream".into()),
        ]),
    };

    let wire: WireStreamInfo = original.clone().into();
    let restored: DomainStreamInfo = wire.into();

    assert_eq!(original.size_bytes, restored.size_bytes);
    assert_eq!(original.attrs, restored.attrs);

    println!(
        "StreamInfo round-trip: OK  (size={:?}, attrs={})",
        restored.size_bytes,
        restored.attrs.len(),
    );
}

// ---------------------------------------------------------------------------

fn round_trip_queue_status() {
    // ---
    let uuids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];

    let original = DomainQueueStatus {
        active_count: 2,
        max_concurrent: 4,
        max_pending: 16,
        pending: uuids.clone(),
    };

    let wire: WireQueueStatus = original.clone().into();
    let restored: DomainQueueStatus = wire.into();

    assert_eq!(original.active_count, restored.active_count);
    assert_eq!(original.max_concurrent, restored.max_concurrent);
    assert_eq!(original.max_pending, restored.max_pending);
    assert_eq!(original.pending, restored.pending);

    println!(
        "QueueStatus round-trip: OK  (active={}, pending={})",
        restored.active_count,
        restored.pending.len(),
    );
}

// ---------------------------------------------------------------------------

fn round_trip_progress() {
    // ---
    let progress = TransferProgress {
        uuid: Uuid::new_v4(),
        bytes_transferred: 512_000,
        percent_done: Some(48.8),
        throughput_bps: 1_250_000.0,
    };

    let wire = progress_to_wire(&progress);

    assert_eq!(wire.bytes_transferred, Some(512_000_i64));
    assert!(wire.percent_done.is_some());

    let pct = wire.percent_done.unwrap().into_inner();
    assert!((pct - 48.8).abs() < 1e-9);

    println!(
        "ProgressInfo to-wire: OK  (bytes={}, pct={:.1}%)",
        wire.bytes_transferred.unwrap(),
        pct,
    );
}
