//! Zero-copy buffer for quiche's `BufFactory` / `BufSplit` traits.
//!
//! `ArcBuf` wraps an `Arc<Vec<u8>>` with offset/len, allowing quiche to
//! split and clone buffers without copying the underlying data.

use std::fmt;
use std::sync::Arc;

use quiche::BufFactory;
use quiche::BufSplit;

/// A reference-counted byte buffer that supports zero-copy splitting.
///
/// Internally holds `Arc<Vec<u8>>` plus an offset and length window,
/// so `clone()` and `split_at()` share the backing allocation.
#[derive(Clone)]
pub struct ArcBuf {
    data: Arc<Vec<u8>>,
    offset: usize,
    len: usize,
}

impl ArcBuf {
    /// Create an `ArcBuf` that takes ownership of a `Vec<u8>` without copying.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let len = v.len();
        Self {
            data: Arc::new(v),
            offset: 0,
            len,
        }
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
        &self.data[self.offset..self.offset + self.len]
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
        vec[new_offset..self.offset].copy_from_slice(prefix);
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

    fn buf_from_slice(buf: &[u8]) -> Self::Buf {
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
}
