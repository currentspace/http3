mod buffer_pool;
mod cid;
mod client;
mod config;
mod connection;
mod connection_map;
mod error;
mod h3_event;
mod server;
mod timer_heap;
mod worker;

use napi_derive::napi;

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
