mod buffer_pool;
mod cid;
mod client;
mod config;
mod connection;
mod connection_map;
mod error;
mod h3_event;
mod quic_client;
mod quic_connection;
mod quic_server;
mod quic_worker;
mod server;
mod timer_heap;
mod worker;

use napi_derive::napi;

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
