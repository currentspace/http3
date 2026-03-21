//! Shared primitives for multi-worker server sharding via SO_REUSEPORT.
//!
//! Both the raw QUIC server (`quic_worker.rs`) and the H3 server
//! (`worker.rs`) use the same pattern: N worker threads each own a socket
//! bound to the same port via SO_REUSEPORT.  The kernel distributes
//! incoming packets by 4-tuple hash.  Connection handles encode the
//! worker index in the upper bits so commands can be routed to the
//! correct worker.

/// Bits reserved for the worker index in connection handles.
/// `worker_index = conn_handle >> WORKER_SHIFT`
/// `local_handle = conn_handle & WORKER_MASK`
pub(crate) const WORKER_SHIFT: u32 = 20;
pub(crate) const WORKER_MASK: u32 = (1 << WORKER_SHIFT) - 1;

/// Extract the worker index from a global connection handle.
#[inline]
pub(crate) fn worker_index(conn_handle: u32) -> usize {
    (conn_handle >> WORKER_SHIFT) as usize
}

/// Strip the worker bits to get the local slab handle.
#[inline]
pub(crate) fn local_conn_handle(conn_handle: u32) -> u32 {
    conn_handle & WORKER_MASK
}

/// Compute the handle offset for a worker index (added to all emitted
/// event conn_handles for global uniqueness).
#[inline]
pub(crate) fn handle_offset(worker_index: u32) -> u32 {
    worker_index << WORKER_SHIFT
}
