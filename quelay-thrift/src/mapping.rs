//! Mapping between generated Thrift wire types and `quelay-domain` types.
//!
//! This module is the only place in the codebase that touches both
//! `gen::*` and `quelay_domain::*`. Everything above the Thrift boundary
//! works exclusively with domain types; everything below works exclusively
//! with generated types.
//!
//! Conversion direction conventions:
//!   `gen → domain`  happens on the receive path  (inbound Thrift call)
//!   `domain → gen`  happens on the send path     (outbound Thrift call)

use std::collections::HashMap;

use uuid::Uuid;

use quelay_domain::{
    // ---
    LinkState as DomainLinkState,
    QueueStatus as DomainQueueStatus,
    StreamInfo as DomainStreamInfo,
    TransferProgress,
};

use crate::gen::{
    // ---
    LinkState as WireLinkState,
    ProgressInfo as WireProgressInfo,
    QueueStatus as WireQueueStatus,
    StreamInfo as WireStreamInfo,
};

// ---------------------------------------------------------------------------
// LinkState
// ---------------------------------------------------------------------------

impl From<WireLinkState> for DomainLinkState {
    // ---
    fn from(w: WireLinkState) -> Self {
        // ---
        match w {
            WireLinkState::CONNECTING => DomainLinkState::Connecting,
            WireLinkState::NORMAL => DomainLinkState::Normal,
            WireLinkState::DEGRADED => DomainLinkState::Degraded,
            WireLinkState::FAILED => DomainLinkState::Failed,
            _ => DomainLinkState::Failed, // unknown value → safe default
        }
    }
}

impl From<DomainLinkState> for WireLinkState {
    // ---
    fn from(d: DomainLinkState) -> Self {
        // ---
        match d {
            DomainLinkState::Connecting => WireLinkState::CONNECTING,
            DomainLinkState::Normal => WireLinkState::NORMAL,
            DomainLinkState::Degraded => WireLinkState::DEGRADED,
            DomainLinkState::Failed => WireLinkState::FAILED,
        }
    }
}

// ---------------------------------------------------------------------------
// StreamInfo
// ---------------------------------------------------------------------------

impl From<WireStreamInfo> for DomainStreamInfo {
    // ---
    fn from(w: WireStreamInfo) -> Self {
        // ---
        DomainStreamInfo {
            size_bytes: w.size_bytes.map(|v| v as u64),
            // attrs is Option<BTreeMap> in generated code; domain uses HashMap.
            // None on the wire means empty map (not an error).
            attrs: w
                .attrs
                .unwrap_or_default()
                .into_iter()
                .collect::<HashMap<_, _>>(),
        }
    }
}

impl From<DomainStreamInfo> for WireStreamInfo {
    // ---
    fn from(d: DomainStreamInfo) -> Self {
        // ---
        WireStreamInfo {
            size_bytes: d.size_bytes.map(|v| v as i64),
            attrs: Some(d.attrs.into_iter().collect()),
        }
    }
}

// ---------------------------------------------------------------------------
// ProgressInfo  (wire) → TransferProgress (domain)
//
// Note: TransferProgress includes `uuid` and `throughput_bps` which are not
// on the wire — callers must supply them separately when building the domain
// type. We therefore provide a helper rather than a bare From impl.
// ---------------------------------------------------------------------------

/// Build a [`TransferProgress`] from a wire [`WireProgressInfo`] plus the
/// fields that live outside the struct on the wire (`uuid`, `throughput_bps`).
// Used by the callback push path — not yet wired.
#[allow(dead_code)]
pub fn progress_from_wire(
    uuid: Uuid,
    w: WireProgressInfo,
    throughput_bps: f64,
) -> TransferProgress {
    // ---
    TransferProgress {
        uuid,
        bytes_transferred: w.bytes_transferred.unwrap_or(0) as u64,
        percent_done: w.percent_done.map(|f| f.into_inner()),
        throughput_bps,
    }
}

/// Build a [`WireProgressInfo`] from a [`TransferProgress`].
///
/// `throughput_bps` is dropped — it is not in the IDL.
pub fn progress_to_wire(d: &TransferProgress) -> WireProgressInfo {
    // ---
    WireProgressInfo {
        bytes_transferred: Some(d.bytes_transferred as i64),
        size_bytes: None, // not tracked in TransferProgress; caller may set
        percent_done: d.percent_done.map(thrift::OrderedFloat::from),
    }
}

// ---------------------------------------------------------------------------
// QueueStatus
// ---------------------------------------------------------------------------

impl From<WireQueueStatus> for DomainQueueStatus {
    // ---
    fn from(w: WireQueueStatus) -> Self {
        // ---
        DomainQueueStatus {
            active_count: w.active_count.unwrap_or(0),
            max_concurrent: w.max_concurrent.unwrap_or(0),
            max_pending: w.max_pending.unwrap_or(0),
            pending: w
                .pending
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| Uuid::parse_str(&s).ok())
                .collect(),
        }
    }
}

impl From<DomainQueueStatus> for WireQueueStatus {
    // ---
    fn from(d: DomainQueueStatus) -> Self {
        // ---
        WireQueueStatus {
            active_count: Some(d.active_count),
            max_concurrent: Some(d.max_concurrent),
            max_pending: Some(d.max_pending),
            pending: Some(d.pending.iter().map(Uuid::to_string).collect()),
        }
    }
}

// ---------------------------------------------------------------------------
// Display for wire types
// ---------------------------------------------------------------------------

impl std::fmt::Display for WireLinkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ---
        let s = match *self {
            WireLinkState::CONNECTING => "Connecting",
            WireLinkState::NORMAL => "Normal",
            WireLinkState::DEGRADED => "Degraded",
            WireLinkState::FAILED => "Failed",
            _ => "Unknown",
        };
        f.write_str(s)
    }
}
