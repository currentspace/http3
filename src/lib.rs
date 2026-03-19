//! Native N-API addon providing QUIC and HTTP/3 for Node.js, powered by
//! cloudflare/quiche. Exposes worker-thread servers/clients for both HTTP/3
//! and raw QUIC to the TypeScript layer via napi-rs.

mod buffer_pool;
mod client_topology;
mod cid;
#[cfg(feature = "node-api")]
mod client;
mod config;
mod connection;
#[cfg(feature = "node-api")]
mod connection_map;
mod error;
mod event_loop;
mod h3_event;
pub mod profile;
#[cfg(feature = "node-api")]
mod mock_quic_profile;
#[cfg(feature = "node-api")]
mod quic_client;
mod quic_connection;
#[cfg(feature = "node-api")]
mod quic_server;
mod quic_worker;
mod reactor_metrics;
mod shared_client_reactor;
#[cfg(feature = "node-api")]
mod server;
mod timer_heap;
mod transport;
#[cfg(feature = "node-api")]
mod worker;

#[cfg(feature = "bench-internals")]
pub mod bench_exports {
    pub use crate::transport::{
        Driver, DriverWaker, PollOutcome, RxDatagram, TxDatagram, RuntimeDriverKind,
    };
    pub use crate::transport::mock::{MockDriver, MockWaker};

    #[cfg(target_os = "linux")]
    pub use crate::transport::io_uring::{IoUringDriver, IoUringWaker};
    #[cfg(target_os = "linux")]
    pub use crate::transport::poll::{PollDriver, PollWaker};
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
