//! Zero-copy buffer for quiche's `BufFactory` / `BufSplit` traits.
//!
//! `ArcBuf` wraps an `Arc<Vec<u8>>` with offset/len, allowing quiche to
//! split and clone buffers without copying the underlying data.

#![deny(unsafe_code)]

use std::fmt;
use std::sync::Arc;

use quiche::BufFactory;
use quiche::BufSplit;

use crate::chunk_pool::{Chunk, ChunkPoolReturn};

struct ArcBufInner {
    data: Vec<u8>,
    pool_return: Option<Arc<ChunkPoolReturn>>,
}

impl Drop for ArcBufInner {
    fn drop(&mut self) {
        if let Some(ret) = self.pool_return.take() {
            let mut data = std::mem::take(&mut self.data);
            data.clear();
            ret.send(data);
        }
    }
}

/// A reference-counted byte buffer that supports zero-copy splitting.
///
/// Internally holds `Arc<Vec<u8>>` plus an offset and length window,
/// so `clone()` and `split_at()` share the backing allocation.
#[derive(Clone)]
pub struct ArcBuf {
    data: Arc<ArcBufInner>,
    offset: usize,
    len: usize,
}

impl ArcBuf {
    /// Create an `ArcBuf` that takes ownership of a `Vec<u8>` without copying.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let len = v.len();
        Self {
            data: Arc::new(ArcBufInner {
                data: v,
                pool_return: None,
            }),
            offset: 0,
            len,
        }
    }

    /// Create an `ArcBuf` from a pooled outbound chunk without copying.
    ///
    /// The chunk's backing allocation is returned to the chunk pool when the
    /// last cloned/split `ArcBuf` handle is dropped.
    pub fn from_chunk(chunk: Chunk) -> Self {
        let (data, offset, pool_return) = chunk.into_raw_parts();
        let len = data.len().saturating_sub(offset);
        Self {
            data: Arc::new(ArcBufInner { data, pool_return }),
            offset,
            len,
        }
    }

    pub fn remaining_len(&self) -> usize {
        self.len
    }

    /// Consume the buffer into an owned Vec when this handle owns the full
    /// allocation uniquely; otherwise copy just the visible window.
    pub fn into_vec(self) -> Vec<u8> {
        let offset = self.offset;
        let len = self.len;

        if offset != 0 || len != self.data.data.len() {
            return self.as_ref().to_vec();
        }

        match Arc::try_unwrap(self.data) {
            Ok(mut inner) => {
                inner.pool_return = None;
                std::mem::take(&mut inner.data)
            }
            Err(data) => data.data[offset..offset + len].to_vec(),
        }
    }
}

impl From<Vec<u8>> for ArcBuf {
    fn from(value: Vec<u8>) -> Self {
        Self::from_vec(value)
    }
}

impl fmt::Debug for ArcBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArcBuf")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .field("arc_strong_count", &Arc::strong_count(&self.data))
            .finish()
    }
}

impl AsRef<[u8]> for ArcBuf {
    fn as_ref(&self) -> &[u8] {
        &self.data.data[self.offset..self.offset + self.len]
    }
}

impl BufSplit for ArcBuf {
    fn split_at(&mut self, at: usize) -> Self {
        assert!(
            at <= self.len,
            "split_at index ({at}) must be <= len ({})",
            self.len
        );
        let tail = Self {
            data: Arc::clone(&self.data),
            offset: self.offset + at,
            len: self.len - at,
        };
        self.len = at;
        tail
    }

    fn try_add_prefix(&mut self, prefix: &[u8]) -> bool {
        // We can prepend in-place only if there is room before our offset
        // AND we are the sole owner of the backing Vec (Arc strong count == 1).
        let prefix_len = prefix.len();
        if prefix_len == 0 {
            return true;
        }
        if self.offset < prefix_len {
            return false;
        }
        let Some(vec) = Arc::get_mut(&mut self.data) else {
            return false;
        };
        let new_offset = self.offset - prefix_len;
        vec.data[new_offset..self.offset].copy_from_slice(prefix);
        self.offset = new_offset;
        self.len += prefix_len;
        true
    }
}

/// Factory that produces `ArcBuf` instances for quiche's generic connection.
#[derive(Debug, Clone, Default)]
pub struct ArcBufFactory;

impl BufFactory for ArcBufFactory {
    type Buf = ArcBuf;
    /// quiche 0.28 introduced a separate datagram-buffer associated type;
    /// use `ArcBuf` here as well so DATAGRAM sends can retain pooled N-API
    /// ingress buffers until quiche has consumed them.
    type DgramBuf = ArcBuf;

    fn buf_from_slice(buf: &[u8]) -> Self::Buf {
        ArcBuf::from_vec(buf.to_vec())
    }

    fn dgram_buf_from_slice(buf: &[u8]) -> Self::DgramBuf {
        ArcBuf::from_vec(buf.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_vec_roundtrip() {
        let data = vec![1, 2, 3, 4, 5];
        let buf = ArcBuf::from_vec(data.clone());
        assert_eq!(buf.as_ref(), &data[..]);
    }

    #[test]
    fn into_vec_avoids_copy_for_unique_full_window() {
        let data = vec![1, 2, 3, 4];
        let buf = ArcBuf::from_vec(data.clone());
        assert_eq!(buf.into_vec(), data);
    }

    #[test]
    fn into_vec_copies_visible_window_for_split_buffer() {
        let mut buf = ArcBuf::from_vec(vec![1, 2, 3, 4]);
        let tail = buf.split_at(2);
        assert_eq!(tail.into_vec(), vec![3, 4]);
    }

    #[test]
    fn split_at_divides_correctly() {
        let mut buf = ArcBuf::from_vec(vec![10, 20, 30, 40, 50]);
        let tail = buf.split_at(2);
        assert_eq!(buf.as_ref(), &[10, 20]);
        assert_eq!(tail.as_ref(), &[30, 40, 50]);
    }

    #[test]
    fn split_at_zero() {
        let mut buf = ArcBuf::from_vec(vec![1, 2, 3]);
        let tail = buf.split_at(0);
        assert!(buf.as_ref().is_empty());
        assert_eq!(tail.as_ref(), &[1, 2, 3]);
    }

    #[test]
    fn split_at_full() {
        let mut buf = ArcBuf::from_vec(vec![1, 2, 3]);
        let tail = buf.split_at(3);
        assert_eq!(buf.as_ref(), &[1, 2, 3]);
        assert!(tail.as_ref().is_empty());
    }

    #[test]
    fn clone_shares_backing() {
        let buf = ArcBuf::from_vec(vec![1, 2, 3]);
        let buf2 = buf.clone();
        assert_eq!(buf.as_ref(), buf2.as_ref());
        assert_eq!(Arc::strong_count(&buf.data), 2);
    }

    #[test]
    fn buf_from_slice_copies() {
        let slice = &[7, 8, 9];
        let buf = ArcBufFactory::buf_from_slice(slice);
        assert_eq!(buf.as_ref(), slice);
    }

    #[test]
    fn debug_output() {
        let buf = ArcBuf::from_vec(vec![1, 2, 3]);
        let s = format!("{buf:?}");
        assert!(s.contains("ArcBuf"));
        assert!(s.contains("len: 3"));
    }

    #[test]
    fn test_try_add_prefix_sole_owner_succeeds() {
        let mut buf = ArcBuf::from_vec(vec![0, 0, 1, 2, 3]);
        let mut tail = buf.split_at(2);
        // buf is the head [0, 0], tail is [1, 2, 3]
        drop(buf); // drop head so tail is sole owner
        assert!(tail.try_add_prefix(&[0xFF]));
        assert_eq!(tail.as_ref(), &[0xFF, 1, 2, 3]);
    }

    #[test]
    fn test_try_add_prefix_shared_owner_fails() {
        let mut buf = ArcBuf::from_vec(vec![0, 0, 1, 2, 3]);
        let mut tail = buf.split_at(2);
        // Keep both head (buf) and tail alive
        assert!(!tail.try_add_prefix(&[0xFF]));
        // Ensure buf is still alive (prevents optimizer from dropping early)
        let _ = buf.as_ref();
    }

    #[test]
    fn test_try_add_prefix_empty_prefix_succeeds() {
        let buf = ArcBuf::from_vec(vec![1, 2, 3]);
        let mut original = buf.clone(); // refcount is now 2
        assert!(original.try_add_prefix(&[]));
        assert_eq!(original.as_ref(), &[1, 2, 3]);
        let _ = buf.as_ref();
    }

    #[test]
    #[should_panic(expected = "split_at index")]
    fn test_split_at_out_of_bounds_panics() {
        let mut buf = ArcBuf::from_vec(vec![1, 2, 3]);
        buf.split_at(4);
    }
}
