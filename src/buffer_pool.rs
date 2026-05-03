//! Reusable buffer pool to reduce allocation pressure in the hot
//! packet-receive and send loops.

#![deny(unsafe_code)]

use std::sync::{Arc, Mutex, OnceLock};

use crate::unsafe_boundary::{InitializedPacketBuf, QuicheRecvBuf};

const DEFAULT_BUF_SIZE: usize = 65535;
const DEFAULT_POOL_SIZE: usize = 256;
const RIGHT_SIZED_CLASSES: [usize; 5] = [
    16 * 1024,
    64 * 1024,
    256 * 1024,
    1024 * 1024,
    4 * 1024 * 1024,
];
const RIGHT_SIZED_MAX_PER_CLASS: usize = 256;

static RIGHT_SIZED_POOL: OnceLock<Mutex<RightSizedPool>> = OnceLock::new();

#[derive(Debug)]
struct RightSizedPool {
    buckets: Vec<Vec<Vec<u8>>>,
}

impl RightSizedPool {
    fn new() -> Self {
        Self {
            buckets: (0..RIGHT_SIZED_CLASSES.len()).map(|_| Vec::new()).collect(),
        }
    }

    fn class_for_capacity(capacity: usize) -> Option<usize> {
        RIGHT_SIZED_CLASSES
            .iter()
            .position(|class_capacity| capacity <= *class_capacity)
    }

    fn class_for_request(len: usize) -> Option<usize> {
        RIGHT_SIZED_CLASSES
            .iter()
            .position(|class_capacity| len <= *class_capacity)
    }

    fn take(&mut self, len: usize) -> Option<Vec<u8>> {
        let start = Self::class_for_request(len)?;
        for bucket in &mut self.buckets[start..] {
            if let Some(mut buf) = bucket.pop() {
                buf.clear();
                if buf.capacity() >= len {
                    return Some(buf);
                }
            }
        }
        None
    }

    fn recycle(&mut self, mut buf: Vec<u8>) -> bool {
        let Some(index) = Self::class_for_capacity(buf.capacity()) else {
            return false;
        };
        let bucket = &mut self.buckets[index];
        if bucket.len() >= RIGHT_SIZED_MAX_PER_CLASS {
            return false;
        }
        buf.clear();
        bucket.push(buf);
        true
    }

    #[cfg(test)]
    fn clear(&mut self) {
        for bucket in &mut self.buckets {
            bucket.clear();
        }
    }
}

fn right_sized_pool() -> &'static Mutex<RightSizedPool> {
    RIGHT_SIZED_POOL.get_or_init(|| Mutex::new(RightSizedPool::new()))
}

pub(crate) fn recycle_right_sized_buffer(buf: Vec<u8>) -> bool {
    let mut pool = match right_sized_pool().lock() {
        Ok(pool) => pool,
        Err(poisoned) => poisoned.into_inner(),
    };
    pool.recycle(buf)
}

fn take_right_sized_buffer(len: usize) -> Option<Vec<u8>> {
    let mut pool = match right_sized_pool().lock() {
        Ok(pool) => pool,
        Err(poisoned) => poisoned.into_inner(),
    };
    pool.take(len)
}

#[cfg(test)]
fn clear_right_sized_pool_for_test() {
    let mut pool = match right_sized_pool().lock() {
        Ok(pool) => pool,
        Err(poisoned) => poisoned.into_inner(),
    };
    pool.clear();
}

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
    /// The returned buffer has length == buf_size and is initialized.
    pub fn checkout(&mut self) -> Vec<u8> {
        self.buffers
            .pop()
            .unwrap_or_else(|| InitializedPacketBuf::zeroed(self.buf_size).into_vec())
    }

    /// Return a buffer to the pool.
    ///
    /// Fixed packet buffers are recycled only when their initialized length is
    /// still `buf_size`. The transport layer carries an explicit payload length
    /// for sends, so recycled buffers do not need to be cleared or resized here.
    pub fn checkin(&mut self, buf: Vec<u8>) {
        if buf.len() == self.buf_size && buf.capacity() >= self.buf_size {
            self.buffers.push(buf);
        }
        // Truncated or undersized buffers are dropped. Retaining them would
        // require a resize, which is the hot-path memset this pool avoids.
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
    max_retain_capacity: usize,
}

impl AdaptiveBufferPool {
    pub fn new(max_buffers: usize, min_capacity: usize) -> Self {
        Self {
            buffers: Vec::with_capacity(max_buffers),
            max_buffers,
            min_capacity,
            max_retain_capacity: min_capacity.saturating_mul(4).max(min_capacity),
        }
    }

    fn checkout_empty_vec(&mut self, len: usize) -> (Vec<u8>, bool) {
        if let Some(index) = self.buffers.iter().rposition(|buf| buf.capacity() >= len) {
            let mut buf = self.buffers.swap_remove(index);
            buf.clear();
            return (buf, true);
        }

        if let Some(buf) = take_right_sized_buffer(len) {
            return (buf, true);
        }

        (Vec::with_capacity(len.max(self.min_capacity)), false)
    }

    /// Take an empty buffer with enough capacity for an OS receive call.
    ///
    /// The returned vector has length 0, so safe Rust cannot read stale or
    /// uninitialized bytes. Platform receive wrappers may pass its spare
    /// capacity to the kernel and set the initialized length after the syscall
    /// reports how many bytes were written.
    pub(crate) fn checkout_empty_for_os_recv(&mut self, len: usize) -> (Vec<u8>, bool) {
        self.checkout_empty_vec(len)
    }

    /// Take a buffer with at least `len` bytes of capacity.
    /// Returns `(buffer, reused_from_pool)`.
    pub fn checkout(&mut self, len: usize) -> (Vec<u8>, bool) {
        // Audit finding #23: zero-init both reuse and fresh-alloc paths so
        // a caller that reads before writing can't see uninitialized
        // memory or stale bytes from a prior connection.
        let (mut buf, reused) = self.checkout_empty_vec(len);
        buf.resize(len, 0);
        (buf, reused)
    }

    /// Take a receive buffer for quiche output APIs without zero-filling.
    ///
    /// The returned wrapper hides uninitialized capacity until quiche reports
    /// how many bytes it wrote.
    pub fn checkout_recv(&mut self, len: usize) -> (QuicheRecvBuf, bool) {
        let (buf, reused) = self.checkout_empty_vec(len);
        (QuicheRecvBuf::from_vec(buf, len), reused)
    }

    /// Copy `data` into a pooled or freshly allocated owned buffer.
    pub fn copy_from_slice(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let (mut buf, reused) = self.checkout_empty_vec(data.len());
        buf.extend_from_slice(data);
        (buf, reused)
    }

    /// Create a pool with a thread-safe return channel for cross-thread buffer recycling.
    /// Returns (pool, recycler_arc, receiver). The receiver must be drained periodically
    /// via `drain_returned()` on the pool's owning thread.
    pub fn with_recycler(
        max_buffers: usize,
        min_capacity: usize,
    ) -> (
        Self,
        Arc<BufferRecycler>,
        crossbeam_channel::Receiver<Vec<u8>>,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(max_buffers * 2);
        let pool = Self {
            buffers: Vec::with_capacity(max_buffers),
            max_buffers,
            min_capacity,
            max_retain_capacity: min_capacity.saturating_mul(4).max(min_capacity),
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
        if buf.capacity() < self.min_capacity || buf.capacity() > self.max_retain_capacity {
            return recycle_right_sized_buffer(buf);
        }
        if self.buffers.len() >= self.max_buffers {
            return recycle_right_sized_buffer(buf);
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
    /// Return a buffer to the pool. If the channel is full or the
    /// receiver has been dropped (connection cleanup, worker exit), the
    /// buffer falls back to a small right-sized global pool before being
    /// dropped. This keeps V8 finalizers from degenerating into
    /// mmap/munmap churn when a per-connection recycler cannot accept more
    /// returned buffers. Audit finding #31.
    pub fn recycle(&self, buf: Vec<u8>) {
        match self.0.try_send(buf) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(buf))
            | Err(crossbeam_channel::TrySendError::Disconnected(buf)) => {
                if !recycle_right_sized_buffer(buf) {
                    crate::reactor_metrics::record_recycler_drop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static RIGHT_SIZED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn adaptive_pool_overflow_uses_right_sized_fallback() {
        let _guard = RIGHT_SIZED_TEST_LOCK.lock().expect("test lock poisoned");
        clear_right_sized_pool_for_test();

        let mut pool = AdaptiveBufferPool::new(1, 16);
        assert!(pool.checkin(vec![0u8; 16]));
        assert!(pool.checkin(vec![0u8; 16]));

        let mut next_pool = AdaptiveBufferPool::new(1, 16);
        let (buf, reused) = next_pool.checkout(16);
        assert!(reused);
        assert_eq!(buf, vec![0u8; 16]);

        clear_right_sized_pool_for_test();
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
    fn test_checkin_truncated_large_buffer_is_dropped() {
        let mut pool = BufferPool::new(1, 1500);
        let mut buf = pool.checkout();
        buf.truncate(1200);

        pool.checkin(buf);

        assert_eq!(pool.buffers.len(), 0);
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
        let small = Vec::with_capacity(128);
        let large = Vec::with_capacity(256);
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

    #[test]
    fn adaptive_pool_checkout_recv_commits_only_written_bytes() {
        let mut pool = AdaptiveBufferPool::new(1, 64);
        let (mut buf, reused) = pool.checkout_recv(8);
        assert!(!reused);

        assert_eq!(buf.append_initialized(b"ping"), 4);

        assert_eq!(buf.initialized_len(), 4);
        assert_eq!(buf.into_initialized_vec(), b"ping");
    }

    #[test]
    fn adaptive_pool_oversized_checkin_uses_right_sized_fallback() {
        let _guard = RIGHT_SIZED_TEST_LOCK.lock().expect("test lock poisoned");
        clear_right_sized_pool_for_test();

        let mut source_pool = AdaptiveBufferPool::new(1, 16);
        let oversized = Vec::with_capacity(64 * 1024);
        assert!(source_pool.checkin(oversized));

        let mut next_pool = AdaptiveBufferPool::new(1, 64 * 1024);
        let (buf, reused) = next_pool.checkout_recv(64 * 1024);
        assert!(reused);
        assert_eq!(buf.remaining(), 64 * 1024);

        clear_right_sized_pool_for_test();
    }

    #[test]
    fn buffer_recycler_disconnected_channel_uses_right_sized_fallback() {
        let _guard = RIGHT_SIZED_TEST_LOCK.lock().expect("test lock poisoned");
        clear_right_sized_pool_for_test();

        let (_pool, recycler, rx) = AdaptiveBufferPool::with_recycler(1, 64 * 1024);
        drop(rx);
        recycler.recycle(Vec::with_capacity(64 * 1024));

        let mut next_pool = AdaptiveBufferPool::new(1, 64 * 1024);
        let (_buf, reused) = next_pool.checkout_recv(64 * 1024);
        assert!(reused);

        clear_right_sized_pool_for_test();
    }

    #[test]
    fn buffer_recycler_full_channel_uses_right_sized_fallback() {
        let _guard = RIGHT_SIZED_TEST_LOCK.lock().expect("test lock poisoned");
        clear_right_sized_pool_for_test();

        let (_pool, recycler, _rx) = AdaptiveBufferPool::with_recycler(1, 64 * 1024);
        recycler.recycle(Vec::with_capacity(64 * 1024));
        recycler.recycle(Vec::with_capacity(64 * 1024));
        recycler.recycle(Vec::with_capacity(64 * 1024));

        let mut next_pool = AdaptiveBufferPool::new(1, 64 * 1024);
        let (_buf, reused) = next_pool.checkout_recv(64 * 1024);
        assert!(reused);

        clear_right_sized_pool_for_test();
    }
}
