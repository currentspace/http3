//! Pure io_uring provided-buffer ring layout helpers.

#![deny(unsafe_code)]

pub(crate) fn provided_buffer_id_is_valid(bid: u16, ring_size: u16) -> bool {
    bid < ring_size
}

pub(crate) fn provided_buffer_offset(bid: u16, buf_size: usize) -> Option<usize> {
    (bid as usize).checked_mul(buf_size)
}

pub(crate) fn provided_buffer_layout_size(ring_size: u16, buf_size: usize) -> Option<usize> {
    (ring_size as usize).checked_mul(buf_size)
}

pub(crate) fn provided_buffer_range_in_layout(bid: u16, ring_size: u16, buf_size: usize) -> bool {
    if !provided_buffer_id_is_valid(bid, ring_size) {
        return false;
    }

    let Some(offset) = provided_buffer_offset(bid, buf_size) else {
        return false;
    };
    let Some(layout_size) = provided_buffer_layout_size(ring_size, buf_size) else {
        return false;
    };
    offset < layout_size && offset.saturating_add(buf_size) <= layout_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_bid_range_is_inside_layout() {
        assert!(provided_buffer_range_in_layout(3, 4, 128));
        assert!(!provided_buffer_range_in_layout(4, 4, 128));
    }
}
