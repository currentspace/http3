//! Native N-API addon providing QUIC and HTTP/3 for Node.js, powered by
//! cloudflare/quiche. Exposes worker-thread servers/clients for both HTTP/3
//! and raw QUIC to the TypeScript layer via napi-rs.

mod buffer_pool;
mod client_topology;
mod cid;
mod client;
mod config;
mod connection;
mod connection_map;
mod error;
mod event_loop;
mod h3_event;
mod quic_client;
mod quic_connection;
mod quic_server;
mod quic_worker;
mod reactor_metrics;
mod shared_client_reactor;
mod server;
mod timer_heap;
mod transport;
mod worker;

use napi_derive::napi;

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[napi]
pub fn runtime_telemetry() -> reactor_metrics::JsReactorTelemetrySnapshot {
    reactor_metrics::snapshot()
}

#[napi]
pub fn reset_runtime_telemetry() {
    reactor_metrics::reset();
}
