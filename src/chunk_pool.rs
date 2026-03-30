//! Size-classed chunk pool for outbound stream payloads.
//!
//! Buffers checked out from this pool are recycled when the `Chunk` handle
//! is dropped, via a crossbeam channel back to the owning worker thread.
//! This eliminates glibc malloc fragmentation from repeated alloc/free
//! cycles on the write path.

use std::sync::Arc;

/// Size classes for pooled chunks.
const CHUNK_CLASSES: [usize; 3] = [1024, 2048, 4096];

/// Number of size-class bins.
const NUM_BINS: usize = CHUNK_CLASSES.len();

/// Outbound stream-payload chunk pool with size-classed bins.
///
/// Buffers are bucketed by the smallest class that fits the request.
/// Payloads larger than the largest class use exact-size allocation
/// without pooling.
pub struct ChunkPool {
    bins: [Vec<Vec<u8>>; NUM_BINS],
    max_per_bin: usize,
}

impl ChunkPool {
    pub fn new(max_per_bin: usize) -> Self {
        Self {
            bins: [Vec::new(), Vec::new(), Vec::new()],
            max_per_bin,
        }
    }

    /// Create a pool with a thread-safe return channel for cross-thread recycling.
    pub fn with_return_channel(
        max_per_bin: usize,
    ) -> (Self, Arc<ChunkPoolReturn>, crossbeam_channel::Receiver<Vec<u8>>) {
        let total_capacity = max_per_bin * NUM_BINS * 2;
        let (tx, rx) = crossbeam_channel::bounded(total_capacity);
        let pool = Self::new(max_per_bin);
        (pool, Arc::new(ChunkPoolReturn(tx)), rx)
    }

    /// Find the bin index for a given length. Returns None if > largest class.
    fn bin_for(len: usize) -> Option<usize> {
        CHUNK_CLASSES.iter().position(|&class| len <= class)
    }

    /// Check out a buffer with capacity >= `len`.
    /// Returns `(buffer, reused)`.
    pub fn checkout(&mut self, len: usize) -> (Vec<u8>, bool) {
        if let Some(bin_idx) = Self::bin_for(len) {
            // Search this bin and larger bins for a reusable buffer.
            for idx in bin_idx..NUM_BINS {
                if let Some(mut buf) = self.bins[idx].pop() {
                    buf.clear();
                    buf.resize(len, 0);
                    return (buf, true);
                }
            }
            // No reusable buffer — allocate at the class size for future reuse.
            let cap = CHUNK_CLASSES[bin_idx];
            let mut buf = Vec::with_capacity(cap);
            buf.resize(len, 0);
            (buf, false)
        } else {
            // Larger than any class — exact allocation, not pooled.
            let mut buf = Vec::with_capacity(len);
            buf.resize(len, 0);
            (buf, false)
        }
    }

    /// Check out a buffer and copy `data` into it.
    pub fn checkout_copy(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let (mut buf, reused) = self.checkout(data.len());
        buf[..data.len()].copy_from_slice(data);
        (buf, reused)
    }

    /// Return a buffer to the pool. Dropped if wrong size class or bin full.
    pub fn checkin(&mut self, buf: Vec<u8>) -> bool {
        let cap = buf.capacity();
        if let Some(bin_idx) = Self::bin_for(cap) {
            // Only accept if capacity matches the class (not undersized).
            if cap >= CHUNK_CLASSES[bin_idx] && self.bins[bin_idx].len() < self.max_per_bin {
                self.bins[bin_idx].push(buf);
                return true;
            }
        }
        false // Dropped — oversized or bin full.
    }

    /// Drain returned buffers from the recycler channel back into the pool.
    pub fn drain_returned(&mut self, rx: &crossbeam_channel::Receiver<Vec<u8>>) {
        while let Ok(buf) = rx.try_recv() {
            self.checkin(buf);
        }
    }

    /// Total buffers across all bins.
    #[cfg(test)]
    pub fn total_pooled(&self) -> usize {
        self.bins.iter().map(|b| b.len()).sum()
    }
}

/// Thread-safe handle for returning chunks to a `ChunkPool` from another
/// thread (e.g., when a `Chunk` is dropped after quiche acceptance).
pub struct ChunkPoolReturn(crossbeam_channel::Sender<Vec<u8>>);

impl ChunkPoolReturn {
    pub fn send(&self, buf: Vec<u8>) {
        let _ = self.0.try_send(buf);
    }
}

/// An outbound payload chunk that auto-recycles its backing buffer to the
/// pool when dropped.
///
/// Replaces `Vec<u8>` in `WorkerCommand::StreamSend` and `PendingWrite`.
pub struct Chunk {
    data: Vec<u8>,
    offset: usize,
    pool_return: Option<Arc<ChunkPoolReturn>>,
}

impl Chunk {
    /// Create a chunk by copying from a NAPI Buffer (JS→Rust boundary).
    /// The resulting Vec will be recycled to the pool when dropped.
    #[cfg(feature = "node-api")]
    pub fn from_napi_buffer(
        buf: &napi::bindgen_prelude::Buffer,
        pool_return: &Arc<ChunkPoolReturn>,
    ) -> Self {
        Self {
            data: buf.to_vec(),
            offset: 0,
            pool_return: Some(Arc::clone(pool_return)),
        }
    }

    /// Create a chunk from an existing Vec (takes ownership, no copy).
    pub fn from_vec(data: Vec<u8>, pool_return: Option<Arc<ChunkPoolReturn>>) -> Self {
        Self {
            data,
            offset: 0,
            pool_return,
        }
    }

    /// Create a chunk by copying a slice into a pool-checked-out buffer.
    pub fn from_slice(
        data: &[u8],
        pool: &mut ChunkPool,
        pool_return: &Arc<ChunkPoolReturn>,
    ) -> Self {
        let (buf, _reused) = pool.checkout_copy(data);
        Self {
            data: buf,
            offset: 0,
            pool_return: Some(Arc::clone(pool_return)),
        }
    }

    /// Create a non-pooled chunk (for tests or one-off allocations).
    pub fn unpooled(data: Vec<u8>) -> Self {
        Self {
            data,
            offset: 0,
            pool_return: None,
        }
    }

    /// View remaining unwritten bytes.
    pub fn remaining(&self) -> &[u8] {
        &self.data[self.offset..]
    }

    /// Number of remaining bytes.
    pub fn remaining_len(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Advance the offset after a partial write.
    pub fn advance(&mut self, n: usize) {
        self.offset += n;
        debug_assert!(self.offset <= self.data.len());
    }

    /// True if all bytes have been consumed.
    pub fn is_fully_written(&self) -> bool {
        self.offset >= self.data.len()
    }

    /// Extract the backing Vec, consuming the chunk.
    /// The Vec is NOT returned to the pool — caller takes ownership.
    pub fn into_vec(mut self) -> Vec<u8> {
        self.pool_return = None; // Prevent Drop from recycling.
        std::mem::take(&mut self.data)
    }

    /// Append additional data to this chunk's unwritten region.
    /// Used when a stream is already blocked and more data arrives.
    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // If there's an offset, compact first.
        if self.offset > 0 {
            self.data.drain(..self.offset);
            self.offset = 0;
        }
        self.data.extend_from_slice(data);
    }

    /// Get the pool return handle (for cloning into sub-chunks).
    pub fn pool_return(&self) -> &Option<Arc<ChunkPoolReturn>> {
        &self.pool_return
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        if let Some(ret) = self.pool_return.take() {
            let mut buf = std::mem::take(&mut self.data);
            buf.clear();
            ret.send(buf);
        }
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chunk")
            .field("len", &self.data.len())
            .field("offset", &self.offset)
            .field("remaining", &self.remaining_len())
            .field("pooled", &self.pool_return.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_returns_correct_size() {
        let mut pool = ChunkPool::new(4);
        let (buf, reused) = pool.checkout(100);
        assert!(!reused);
        assert_eq!(buf.len(), 100);
        assert!(buf.capacity() >= 1024); // Smallest class
    }

    #[test]
    fn checkin_and_reuse() {
        let mut pool = ChunkPool::new(4);
        let (buf, _) = pool.checkout(500);
        let cap = buf.capacity();
        pool.checkin(buf);
        assert_eq!(pool.total_pooled(), 1);

        let (buf2, reused) = pool.checkout(500);
        assert!(reused);
        assert_eq!(buf2.capacity(), cap);
    }

    #[test]
    fn size_class_bucketing() {
        let mut pool = ChunkPool::new(4);

        // 1KB class
        let (buf1k, _) = pool.checkout(512);
        assert!(buf1k.capacity() >= 1024);
        pool.checkin(buf1k);

        // 2KB class
        let (buf2k, _) = pool.checkout(1500);
        assert!(buf2k.capacity() >= 2048);
        pool.checkin(buf2k);

        // 4KB class
        let (buf4k, _) = pool.checkout(3000);
        assert!(buf4k.capacity() >= 4096);
        pool.checkin(buf4k);

        assert_eq!(pool.total_pooled(), 3); // One in each bin.
    }

    #[test]
    fn oversized_not_pooled() {
        let mut pool = ChunkPool::new(4);
        let (buf, _) = pool.checkout(8000);
        assert_eq!(buf.len(), 8000);
        assert!(!pool.checkin(buf)); // Rejected — too large.
    }

    #[test]
    fn bin_full_drops() {
        let mut pool = ChunkPool::new(2); // Max 2 per bin.
        // Create 3 separate buffers.
        let bufs: Vec<_> = (0..3).map(|_| Vec::with_capacity(1024)).collect();
        for buf in bufs {
            pool.checkin(buf);
        }
        assert_eq!(pool.bins[0].len(), 2); // Capped at 2, third dropped.
    }

    #[test]
    fn chunk_lifecycle() {
        let (mut pool, ret, rx) = ChunkPool::with_return_channel(4);

        // Create chunk with pool-sized buffer (1KB class).
        let (buf, _) = pool.checkout(100);
        let chunk = Chunk::from_vec(buf, Some(Arc::clone(&ret)));
        assert_eq!(chunk.remaining_len(), 100);

        // Drop chunk — should recycle via channel back to pool.
        drop(chunk);
        pool.drain_returned(&rx);
        assert_eq!(pool.total_pooled(), 1);
    }

    #[test]
    fn chunk_advance_and_remaining() {
        let chunk_data = vec![10, 20, 30, 40, 50];
        let mut chunk = Chunk::unpooled(chunk_data);
        assert_eq!(chunk.remaining(), &[10, 20, 30, 40, 50]);

        chunk.advance(2);
        assert_eq!(chunk.remaining(), &[30, 40, 50]);
        assert_eq!(chunk.remaining_len(), 3);
        assert!(!chunk.is_fully_written());

        chunk.advance(3);
        assert!(chunk.is_fully_written());
        assert_eq!(chunk.remaining_len(), 0);
    }

    #[test]
    fn chunk_into_vec_skips_recycle() {
        let (mut pool, ret, rx) = ChunkPool::with_return_channel(4);

        let chunk = Chunk::from_vec(vec![1, 2, 3], Some(Arc::clone(&ret)));
        let vec = chunk.into_vec();
        assert_eq!(vec, vec![1, 2, 3]);

        // Nothing returned to pool since into_vec() consumed it.
        pool.drain_returned(&rx);
        assert_eq!(pool.total_pooled(), 0);
    }

    #[test]
    fn chunk_append() {
        let mut chunk = Chunk::unpooled(vec![1, 2, 3]);
        chunk.advance(1);
        assert_eq!(chunk.remaining(), &[2, 3]);

        chunk.append(&[4, 5]);
        // After append with offset, data is compacted.
        assert_eq!(chunk.remaining(), &[2, 3, 4, 5]);
        assert_eq!(chunk.offset, 0);
    }

    #[test]
    fn recycler_round_trip() {
        let (mut pool, ret, rx) = ChunkPool::with_return_channel(8);

        // Checkout from pool.
        let (buf, _) = pool.checkout(512);
        let cap = buf.capacity();

        // Wrap in chunk with recycler.
        let chunk = Chunk::from_vec(buf, Some(Arc::clone(&ret)));
        drop(chunk); // Recycle.

        pool.drain_returned(&rx);
        assert_eq!(pool.total_pooled(), 1);

        // Reuse.
        let (buf2, reused) = pool.checkout(256);
        assert!(reused);
        assert_eq!(buf2.capacity(), cap);
    }

    #[test]
    fn checkout_copy_preserves_data() {
        let mut pool = ChunkPool::new(4);
        let data = [42u8; 100];
        let (buf, _) = pool.checkout_copy(&data);
        assert_eq!(&buf[..100], &data[..]);
    }
}
