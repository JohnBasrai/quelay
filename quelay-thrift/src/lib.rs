//! Thrift C2I service stubs for the Quelay agent interface.
//!
//! These hand-written stubs mirror `quelay.thrift` exactly and will be
//! replaced by Thrift-generated code once the IDL is finalised.

// Generated code lives here. Run scripts/thrift-compile.sh to regenerate.
// Do not edit files under gen/ by hand.
#[allow(dead_code, unused_imports, unused_extern_crates, clippy::all)]
mod gen {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/gen/quelay.rs"));
}

mod mapping;

// ---

pub use mapping::progress_to_wire;

pub use gen::{
    //
    FailReason,
    LinkState,
    ProgressInfo,
    QueLayAgentSyncClient,
    QueLayAgentSyncHandler,
    QueLayAgentSyncProcessor,
    QueLayCallbackSyncClient,
    QueLayCallbackSyncHandler,
    QueLayCallbackSyncProcessor,
    QueueStatus,
    StartStreamReturn,
    StreamInfo,
    StreamStartStatus,
    TQueLayAgentSyncClient,
    TQueLayCallbackSyncClient,
    IDL_VERSION,
};

// ---

pub use ordered_float::OrderedFloat;
// so that crate consumers import only from `quelay_thrift::` and never drill
// into `thrift::protocol`, `thrift::server`, or `thrift::transport` directly.
pub use thrift::protocol::{
    // ---
    TBinaryInputProtocol,
    TBinaryInputProtocolFactory,
    TBinaryOutputProtocol,
    TBinaryOutputProtocolFactory,
};
pub use thrift::server::TServer;
pub use thrift::transport::{
    // ---
    TBufferedReadTransport,
    TBufferedReadTransportFactory,
    TBufferedWriteTransport,
    TBufferedWriteTransportFactory,
    TIoChannel,
    TTcpChannel,
};
