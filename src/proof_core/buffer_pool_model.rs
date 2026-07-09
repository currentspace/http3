//! Pure size-class arithmetic for `crate::buffer_pool`'s `RightSizedPool`.
//!
//! Same shape as [`crate::proof_core::chunk_pool_model`], and the same bug
//! class applied here: `class_for_capacity` (used by `recycle`, i.e.
//! checkin) previously reused `class_for_request`'s "smallest class `>=`"
//! semantics, so a buffer whose capacity fell strictly between two class
//! thresholds was filed into a bucket whose declared capacity it did not
//! actually meet. A later `take` for a length in that gap would pop the
//! buffer, find `buf.capacity() < len`, and silently drop it instead of
//! returning it to any bucket — the buffer was reused as pool key material
//! but never as an actual reusable allocation, needlessly increasing
//! allocator churn (this is a lost-efficiency bug, not a memory-safety
//! one: `take`'s explicit `buf.capacity() >= len` check means an
//! undersized buffer is never actually handed out to a caller expecting
//! more capacity than it has).
//!
//! `class_for_capacity` now mirrors `chunk_pool_model::bin_for_cap`:
//! largest class `<=` capacity, matching audit finding #28's fix.

#![deny(unsafe_code)]

pub(crate) const RIGHT_SIZED_CLASSES: [usize; 5] = [
    16 * 1024,
    64 * 1024,
    256 * 1024,
    1024 * 1024,
    4 * 1024 * 1024,
];

/// Find the bucket a `len`-byte request should start searching from: the
/// smallest class that can hold `len`. `take` scans this bucket and every
/// larger one, so starting here (rather than at the exact-matching class
/// alone) still finds any bigger buffer that happens to be sitting
/// unclaimed in a higher bucket.
pub(crate) fn class_for_request(len: usize) -> Option<usize> {
    RIGHT_SIZED_CLASSES
        .iter()
        .position(|class_capacity| len <= *class_capacity)
}

/// Find the bucket a `capacity`-capacity Vec should return to on checkin:
/// the largest class whose threshold the Vec fully covers. Vecs larger
/// than the largest class are rejected (avoids stuffing an oversized
/// allocation into a smaller bucket, which would waste memory the next
/// time it's handed out).
pub(crate) fn class_for_capacity(capacity: usize) -> Option<usize> {
    let max_class = *RIGHT_SIZED_CLASSES.last()?;
    if capacity > max_class {
        return None;
    }
    RIGHT_SIZED_CLASSES
        .iter()
        .rposition(|class_capacity| *class_capacity <= capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_for_request_picks_smallest_class_geq_len() {
        assert_eq!(class_for_request(0), Some(0));
        assert_eq!(class_for_request(16 * 1024), Some(0));
        assert_eq!(class_for_request(16 * 1024 + 1), Some(1));
        assert_eq!(class_for_request(4 * 1024 * 1024), Some(4));
        assert_eq!(class_for_request(4 * 1024 * 1024 + 1), None);
    }

    #[test]
    fn class_for_capacity_picks_largest_class_leq_capacity() {
        // Between the 16 KiB and 64 KiB classes: must map back into the
        // 16 KiB bucket, not the 64 KiB one (this is the case that was
        // silently mis-bucketed before this module existed).
        assert_eq!(class_for_capacity(20 * 1024), Some(0));
        assert_eq!(class_for_capacity(16 * 1024), Some(0));
        assert_eq!(class_for_capacity(16 * 1024 - 1), None); // under smallest class
        assert_eq!(class_for_capacity(4 * 1024 * 1024), Some(4));
        assert_eq!(class_for_capacity(4 * 1024 * 1024 + 1), None); // over largest class
    }

    #[test]
    fn class_for_capacity_of_a_class_size_is_that_same_class() {
        for (idx, &class) in RIGHT_SIZED_CLASSES.iter().enumerate() {
            assert_eq!(class_for_request(class), Some(idx));
            assert_eq!(class_for_capacity(class), Some(idx));
        }
    }

    /// The property that matters operationally: any buffer `recycle`
    /// accepts into bucket `i` must be found by a `take` whose `len` maps
    /// its search-start to bucket `i`, WITH capacity `>= len` (i.e. the
    /// buffer is actually usable, not just co-located). This is the
    /// contract `RightSizedPool::take`'s explicit capacity check relies
    /// on as a backstop — this test confirms the classification itself
    /// doesn't need that backstop to hide a real mis-bucketing.
    #[test]
    fn recycled_bucket_capacity_always_meets_its_own_class_threshold() {
        for cap in 0..=(4 * 1024 * 1024 + 10) {
            if let Some(idx) = class_for_capacity(cap) {
                assert!(
                    RIGHT_SIZED_CLASSES[idx] <= cap,
                    "class {idx} threshold {} exceeds actual capacity {cap}",
                    RIGHT_SIZED_CLASSES[idx]
                );
            }
        }
    }
}
