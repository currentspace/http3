//! Reusable buffer pool to reduce allocation pressure in the hot
//! packet-receive and send loops.

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
    pub fn checkout(&mut self) -> Vec<u8> {
        self.buffers
            .pop()
            .unwrap_or_else(|| vec![0u8; self.buf_size])
    }

    /// Return a buffer to the pool. Only keeps it if capacity is sufficient.
    pub fn checkin(&mut self, mut buf: Vec<u8>) {
        if buf.capacity() >= self.buf_size {
            buf.clear();
            buf.resize(self.buf_size, 0);
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
}
