//! Native N-API addon providing QUIC and HTTP/3 for Node.js, powered by
//! cloudflare/quiche. Exposes worker-thread servers/clients for both HTTP/3
//! and raw QUIC to the TypeScript layer via napi-rs.

mod buffer_pool;
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
mod server;
mod timer_heap;
mod transport;
mod worker;

use napi_derive::napi;

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
