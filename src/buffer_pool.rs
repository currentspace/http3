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
}
