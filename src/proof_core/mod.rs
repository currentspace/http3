//! Pure proof-facing models shared by runtime code, fuzz targets, and formal
//! harnesses.

#![deny(unsafe_code)]

pub(crate) mod admission;
pub(crate) mod buffer_pool_model;
pub(crate) mod chunk_pool_model;
pub(crate) mod cid_model;
pub(crate) mod cmsg_cursor;
pub(crate) mod pending_write_model;
pub(crate) mod recv_buf_model;
pub(crate) mod retry_token_model;
pub(crate) mod ring_layout;
#[cfg(any(test, kani))]
pub(crate) mod stream_tracking;
