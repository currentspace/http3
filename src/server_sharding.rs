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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_index_extraction() {
        // worker 0, local handle 5
        assert_eq!(worker_index(5), 0);
        // worker 1
        let handle = (1 << WORKER_SHIFT) | 7;
        assert_eq!(worker_index(handle), 1);
        // worker 3
        let handle = (3 << WORKER_SHIFT) | 100;
        assert_eq!(worker_index(handle), 3);
    }

    #[test]
    fn test_local_conn_handle() {
        let local = 0xABCu32;
        let handle = (2 << WORKER_SHIFT) | local;
        assert_eq!(local_conn_handle(handle), local);
    }

    #[test]
    fn test_handle_offset() {
        assert_eq!(handle_offset(0), 0);
        assert_eq!(handle_offset(1), 1 << WORKER_SHIFT);
        assert_eq!(handle_offset(5), 5 << WORKER_SHIFT);
    }

    #[test]
    fn test_roundtrip_handle_encoding() {
        for w in 0..8u32 {
            for local in [0, 1, 42, WORKER_MASK - 1, WORKER_MASK] {
                let global = handle_offset(w) | local;
                assert_eq!(
                    local_conn_handle(global),
                    local,
                    "roundtrip failed for worker={w} local={local}"
                );
                assert_eq!(
                    worker_index(global),
                    w as usize,
                    "worker extraction failed for worker={w} local={local}"
                );
            }
        }
    }

    #[test]
    fn test_worker_index_zero() {
        assert_eq!(handle_offset(0), 0);
        assert_eq!(worker_index(0), 0);
    }

    #[test]
    fn test_max_local_handle() {
        let local = WORKER_MASK;
        for w in 0..4u32 {
            let global = handle_offset(w) | local;
            assert_eq!(local_conn_handle(global), local);
            assert_eq!(worker_index(global), w as usize);
        }
    }
}
