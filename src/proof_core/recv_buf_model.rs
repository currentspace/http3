//! Pure initialized receive-buffer length model.

#![deny(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecvBufModel {
    initialized_len: usize,
    target_len: usize,
}

impl RecvBufModel {
    pub(crate) fn new(target_len: usize) -> Self {
        Self {
            initialized_len: 0,
            target_len,
        }
    }

    pub(crate) fn initialized_len(self) -> usize {
        self.initialized_len
    }

    pub(crate) fn target_len(self) -> usize {
        self.target_len
    }

    pub(crate) fn is_empty(self) -> bool {
        self.initialized_len == 0
    }

    pub(crate) fn is_full(self) -> bool {
        self.initialized_len >= self.target_len
    }

    pub(crate) fn remaining(self) -> usize {
        self.target_len.saturating_sub(self.initialized_len)
    }

    pub(crate) fn append_len(self, requested_len: usize) -> usize {
        requested_len.min(self.remaining())
    }

    pub(crate) fn after_append(self, written: usize) -> Self {
        Self {
            initialized_len: self
                .initialized_len
                .saturating_add(written)
                .min(self.target_len),
            target_len: self.target_len,
        }
    }

    pub(crate) fn reported_write_is_valid(
        before: usize,
        remaining: usize,
        written: usize,
        actual_after: usize,
    ) -> bool {
        written <= remaining && actual_after >= before && actual_after - before == written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_is_capped_at_remaining_capacity() {
        let model = RecvBufModel::new(4);
        let model = model.after_append(model.append_len(10));

        assert_eq!(model.initialized_len(), 4);
        assert_eq!(model.target_len(), 4);
        assert!(model.is_full());
        assert_eq!(model.remaining(), 0);
    }
}
