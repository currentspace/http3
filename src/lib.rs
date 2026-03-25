//! Native N-API addon providing QUIC and HTTP/3 for Node.js, powered by
//! cloudflare/quiche. Exposes worker-thread servers/clients for both HTTP/3
//! and raw QUIC to the TypeScript layer via napi-rs.

mod arc_buf;
mod buffer_pool;
pub mod chunk_pool;
mod cid;
#[cfg(feature = "node-api")]
mod client;
mod client_topology;
mod config;
mod connection;
mod connection_map;
mod error;
mod event_loop;
mod h3_event;
#[cfg(feature = "node-api")]
mod mock_quic_profile;
pub mod profile;
#[cfg(feature = "node-api")]
mod quic_client;
mod quic_connection;
#[cfg(feature = "node-api")]
mod quic_server;
mod quic_worker;
mod reactor_metrics;
#[cfg(feature = "node-api")]
mod server;
mod server_sharding;
mod shared_client_reactor;
mod timer_heap;
mod transport;
mod worker;

#[cfg(feature = "bench-internals")]
pub mod bench_exports {
    // ── Transport layer ─────────────────────────────────────────────
    pub use crate::timer_heap::TimerHeap;
    pub use crate::transport::mock::{MockDriver, MockWaker};
    pub use crate::transport::{
        Driver, DriverWaker, ErasedWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
    };

    #[cfg(target_os = "linux")]
    pub use crate::transport::io_uring::{IoUringDriver, IoUringWaker};
    #[cfg(target_os = "linux")]
    pub use crate::transport::poll::{PollDriver, PollWaker};

    // ── Event system ────────────────────────────────────────────────
    pub use crate::event_loop::{EventBatcher, EventBatcherStatsHandle, EventSink, MAX_BATCH_SIZE};
    pub use crate::h3_event::{
        JsH3Event, JsEventMeta, JsHeader, JsSessionMetrics,
        EVENT_DATA, EVENT_DATAGRAM, EVENT_DRAIN, EVENT_ERROR, EVENT_FINISHED, EVENT_GOAWAY,
        EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_METRICS, EVENT_NEW_SESSION,
        EVENT_NEW_STREAM, EVENT_RESET, EVENT_SESSION_CLOSE, EVENT_SESSION_TICKET,
        EVENT_SHUTDOWN_COMPLETE,
    };
    pub use crate::profile::event_sink::{
        TaggedEventBatch, channel_batcher, counting_batcher, noop_batcher,
    };

    // ── Buffer pools ────────────────────────────────────────────────
    pub use crate::buffer_pool::{AdaptiveBufferPool, BufferPool};
    pub use crate::chunk_pool::Chunk;

    // ── Configuration ───────────────────────────────────────────────
    pub use crate::cid::CidEncoding;
    pub use crate::config::{
        ClientAuthMode, Http3Config, JsQuicClientOptions, JsQuicServerOptions,
        TransportRuntimeMode, new_quic_client_config, new_quic_server_config,
    };
    pub use crate::error::Http3NativeError;

    // ── H3 connection + worker ──────────────────────────────────────
    pub use crate::connection::{H3Connection, H3ConnectionInit};
    pub use crate::connection_map::ConnectionMap;
    pub use crate::worker::{
        ClientWorkerCommand, ClientWorkerHandle, H3ServerWorker, WorkerCommand, WorkerHandle,
        spawn_h3_client_on_driver, spawn_h3_server_worker_on_driver,
    };

    // ── QUIC connection + worker ────────────────────────────────────
    pub use crate::quic_connection::QuicConnection;
    pub use crate::quic_worker::{
        QuicClientCommand, QuicClientHandle, QuicServerCommand, QuicServerConfig,
        QuicServerHandle, QuicServerWorker, spawn_dedicated_quic_client_on_driver,
        spawn_server_worker_on_driver,
    };
}

#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg_attr(feature = "node-api", napi)]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg_attr(feature = "node-api", napi)]
pub fn runtime_telemetry() -> reactor_metrics::JsReactorTelemetrySnapshot {
    reactor_metrics::snapshot()
}

#[cfg_attr(feature = "node-api", napi)]
pub fn reset_runtime_telemetry() {
    reactor_metrics::reset();
}

#[cfg_attr(feature = "node-api", napi)]
pub fn set_lifecycle_trace_enabled(enabled: bool) {
    reactor_metrics::set_lifecycle_trace_enabled(enabled);
}

#[cfg_attr(feature = "node-api", napi)]
pub fn reset_lifecycle_trace() {
    reactor_metrics::reset_lifecycle_trace();
}

#[cfg_attr(feature = "node-api", napi)]
pub fn lifecycle_trace_snapshot() -> reactor_metrics::JsLifecycleTraceSnapshot {
    reactor_metrics::lifecycle_trace_snapshot()
}
