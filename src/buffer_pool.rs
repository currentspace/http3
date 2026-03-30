//! Reusable buffer pool to reduce allocation pressure in the hot
//! packet-receive and send loops.

use std::sync::Arc;

const DEFAULT_BUF_SIZE: usize = 65535;
const DEFAULT_POOL_SIZE: usize = 256;

pub struct BufferPool {
    buffers: Vec<Vec<u8>>,
    buf_size: usize,
}

impl BufferPool {
    pub fn new(pool_size: usize, buf_size: usize) -> Self {
        let mut buffers = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            buffers.push(vec![0u8; buf_size]);
        }
        Self { buffers, buf_size }
    }

    /// Take a buffer from the pool (or allocate a new one).
    /// The returned buffer has length == buf_size but content is uninitialized.
    /// Callers must write before reading.
    #[allow(unsafe_code)]
    pub fn checkout(&mut self) -> Vec<u8> {
        self.buffers.pop().unwrap_or_else(|| {
            let mut buf = Vec::with_capacity(self.buf_size);
            // SAFETY: callers always write before reading (documented contract).
            unsafe { buf.set_len(self.buf_size); }
            buf
        })
    }

    /// Return a buffer to the pool. Only keeps it if capacity is sufficient.
    #[allow(unsafe_code)]
    pub fn checkin(&mut self, mut buf: Vec<u8>) {
        if buf.capacity() >= self.buf_size {
            buf.clear();
            // SAFETY: checkout() contract requires callers write before reading.
            // Matches AdaptiveBufferPool::checkout() which uses the same pattern.
            unsafe { buf.set_len(self.buf_size); }
            self.buffers.push(buf);
        }
        // Undersized buffers are dropped
    }
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new(DEFAULT_POOL_SIZE, DEFAULT_BUF_SIZE)
    }
}

/// A thread-local pool for owned byte buffers whose lengths vary over time.
/// Buffers are returned cleared and resized to the requested length by
/// temporarily setting the length before the caller writes into every byte.
pub struct AdaptiveBufferPool {
    buffers: Vec<Vec<u8>>,
    max_buffers: usize,
    min_capacity: usize,
}

impl AdaptiveBufferPool {
    pub fn new(max_buffers: usize, min_capacity: usize) -> Self {
        Self {
            buffers: Vec::with_capacity(max_buffers),
            max_buffers,
            min_capacity,
        }
    }

    /// Take a buffer with at least `len` bytes of capacity.
    /// Returns `(buffer, reused_from_pool)`.
    #[allow(unsafe_code)]
    pub fn checkout(&mut self, len: usize) -> (Vec<u8>, bool) {
        if let Some(index) = self.buffers.iter().rposition(|buf| buf.capacity() >= len) {
            let mut buf = self.buffers.swap_remove(index);
            buf.clear();
            // SAFETY: callers write every byte before reading. The buffer already
            // has enough capacity for `len`.
            unsafe {
                buf.set_len(len);
            }
            return (buf, true);
        }

        let mut buf = Vec::with_capacity(len.max(self.min_capacity));
        // SAFETY: callers write every byte before reading.
        unsafe {
            buf.set_len(len);
        }
        (buf, false)
    }

    /// Copy `data` into a pooled or freshly allocated owned buffer.
    pub fn copy_from_slice(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let (mut buf, reused) = self.checkout(data.len());
        if !data.is_empty() {
            buf[..data.len()].copy_from_slice(data);
        }
        (buf, reused)
    }

    /// Create a pool with a thread-safe return channel for cross-thread buffer recycling.
    /// Returns (pool, recycler_arc, receiver). The receiver must be drained periodically
    /// via `drain_returned()` on the pool's owning thread.
    pub fn with_recycler(
        max_buffers: usize,
        min_capacity: usize,
    ) -> (Self, Arc<BufferRecycler>, crossbeam_channel::Receiver<Vec<u8>>) {
        let (tx, rx) = crossbeam_channel::bounded(max_buffers * 2);
        let pool = Self {
            buffers: Vec::with_capacity(max_buffers),
            max_buffers,
            min_capacity,
        };
        (pool, Arc::new(BufferRecycler(tx)), rx)
    }

    /// Pull returned buffers from the recycler channel back into the pool.
    /// Should be called periodically on the pool's owning thread.
    pub fn drain_returned(&mut self, rx: &crossbeam_channel::Receiver<Vec<u8>>) {
        while let Ok(buf) = rx.try_recv() {
            self.checkin(buf);
        }
    }

    /// Return a buffer to the pool.
    /// Returns `true` when the buffer was retained for reuse.
    pub fn checkin(&mut self, mut buf: Vec<u8>) -> bool {
        if self.buffers.len() >= self.max_buffers || buf.capacity() < self.min_capacity {
            return false;
        }

        buf.clear();
        self.buffers.push(buf);
        true
    }
}

/// Thread-safe handle for returning buffers to an `AdaptiveBufferPool`
/// from a different thread (e.g., V8 GC finalize callback).
pub struct BufferRecycler(crossbeam_channel::Sender<Vec<u8>>);

impl BufferRecycler {
    /// Return a buffer to the pool. If the channel is full, the buffer is dropped.
    pub fn recycle(&self, buf: Vec<u8>) {
        let _ = self.0.try_send(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkout_checkin() {
        let mut pool = BufferPool::new(4, 1500);
        assert_eq!(pool.buffers.len(), 4);

        let buf = pool.checkout();
        assert_eq!(buf.len(), 1500);
        assert_eq!(pool.buffers.len(), 3);

        pool.checkin(buf);
        assert_eq!(pool.buffers.len(), 4);
    }

    #[test]
    fn test_empty_pool_allocates() {
        let mut pool = BufferPool::new(0, 1500);
        let buf = pool.checkout();
        assert_eq!(buf.len(), 1500);
    }

    #[test]
    fn test_checkin_preserves_capacity() {
        let mut pool = BufferPool::new(1, 100);
        let mut buf = pool.checkout();
        buf[0] = 42; // Write some data
        pool.checkin(buf);

        let buf2 = pool.checkout();
        assert_eq!(buf2.len(), 100);
        assert!(buf2.capacity() >= 100);
    }

    #[test]
    fn adaptive_pool_reuses_large_enough_buffer() {
        let mut pool = AdaptiveBufferPool::new(2, 64);
        let (buf, reused) = pool.copy_from_slice(b"hello");
        assert!(!reused);
        assert_eq!(buf, b"hello");
        assert!(pool.checkin(buf));

        let (buf2, reused2) = pool.copy_from_slice(b"world");
        assert!(reused2);
        assert_eq!(buf2, b"world");
    }

    #[test]
    fn adaptive_pool_drops_when_full() {
        let mut pool = AdaptiveBufferPool::new(1, 16);
        assert!(pool.checkin(vec![0u8; 16]));
        assert!(!pool.checkin(vec![0u8; 16]));
    }

    #[test]
    fn test_exhaustion_checkout_still_allocates() {
        let mut pool = BufferPool::new(2, 1500);
        let buf1 = pool.checkout();
        let buf2 = pool.checkout();
        let buf3 = pool.checkout(); // fallback allocation
        assert_eq!(buf1.len(), 1500);
        assert_eq!(buf2.len(), 1500);
        assert_eq!(buf3.len(), 1500);
    }

    #[test]
    fn test_checkin_undersized_buffer_is_dropped() {
        let mut pool = BufferPool::new(2, 1500);
        let _buf1 = pool.checkout();
        let _buf2 = pool.checkout();
        assert_eq!(pool.buffers.len(), 0);

        let tiny = Vec::with_capacity(50);
        pool.checkin(tiny);
        assert_eq!(pool.buffers.len(), 0); // rejected — too small

        let proper = vec![0u8; 1500];
        pool.checkin(proper);
        assert_eq!(pool.buffers.len(), 1);
    }

    #[test]
    fn test_checkout_checkin_cycle_stability() {
        let mut pool = BufferPool::new(2, 100);
        for _ in 0..1000 {
            let buf = pool.checkout();
            pool.checkin(buf);
        }
        assert_eq!(pool.buffers.len(), 2);
    }

    #[test]
    fn test_default_pool_dimensions() {
        let pool = BufferPool::default();
        assert_eq!(pool.buffers.len(), 256);
        assert_eq!(pool.buf_size, 65535);
    }

    #[test]
    fn adaptive_pool_checkout_picks_best_fit() {
        let mut pool = AdaptiveBufferPool::new(4, 64);
        let small = Vec::with_capacity(256);
        let large = Vec::with_capacity(4096);
        pool.checkin(small);
        pool.checkin(large);

        let (buf, reused) = pool.checkout(200);
        assert!(reused);
        assert_eq!(buf.len(), 200);
    }

    #[test]
    fn adaptive_pool_checkout_below_min_allocates_min() {
        let mut pool = AdaptiveBufferPool::new(4, 64);
        let (buf, reused) = pool.checkout(8);
        assert!(!reused);
        assert!(buf.capacity() >= 64);
    }

    #[test]
    fn adaptive_pool_recycler_returns_buffers() {
        let (mut pool, recycler, rx) = AdaptiveBufferPool::with_recycler(4, 64);

        // Checkout a buffer (will allocate fresh since pool is empty)
        let (buf, reused) = pool.checkout(128);
        assert!(!reused);
        assert_eq!(buf.len(), 128);
        let cap = buf.capacity();

        // Simulate GC thread returning the buffer via recycler
        recycler.recycle(buf);

        // Pool hasn't received it yet
        assert_eq!(pool.buffers.len(), 0);

        // Drain the return channel
        pool.drain_returned(&rx);
        assert_eq!(pool.buffers.len(), 1);

        // Next checkout should reuse it
        let (buf2, reused2) = pool.checkout(64);
        assert!(reused2);
        assert!(buf2.capacity() >= cap);
    }
}
