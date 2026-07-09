//! Pure size-class arithmetic for [`crate::chunk_pool::ChunkPool`].
//!
//! Extracted so the checkout/checkin classification logic can be Kani-proven
//! directly (bounded, heap-free, no `Vec`/channel state) instead of only
//! being exercised transitively through the stateful pool. Audit finding
//! #28 was exactly a violation of the property proven in
//! `src/proofs/kani_harnesses.rs`: `bin_for_cap` must return the *largest*
//! class `<=` a buffer's capacity, not the smallest class `>=` it (which
//! `bin_for` uses, and which `bin_for_cap` incorrectly reused before the
//! fix) — reusing `bin_for`'s semantics silently discarded any buffer whose
//! capacity fell strictly between two class thresholds.

#![deny(unsafe_code)]

/// Size classes for pooled chunks.
///
/// The top class matches Node's default binary stream high-water mark
/// (64 KiB), so ordinary writes avoid malloc churn while still copying into
/// Rust-owned memory before crossing worker-thread boundaries.
pub(crate) const CHUNK_CLASSES: [usize; 7] = [1024, 2048, 4096, 8192, 16_384, 32_768, 65_536];

/// Number of size-class bins.
pub(crate) const NUM_BINS: usize = CHUNK_CLASSES.len();

/// Find the bin a `len`-byte allocation should be served from on checkout:
/// the smallest class that can hold `len`. Returns `None` if `len` exceeds
/// every class.
pub(crate) fn bin_for(len: usize) -> Option<usize> {
    CHUNK_CLASSES.iter().position(|&class| len <= class)
}

/// Find the bin a `cap`-capacity Vec should return to on checkin: the
/// largest class whose threshold the Vec fully covers. Vecs larger than the
/// largest class are rejected (avoids stuffing oversized allocations into
/// smaller bins, which would waste memory).
pub(crate) fn bin_for_cap(cap: usize) -> Option<usize> {
    let max_class = *CHUNK_CLASSES.last()?;
    if cap > max_class {
        return None;
    }
    CHUNK_CLASSES.iter().rposition(|&class| class <= cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_for_picks_smallest_class_geq_len() {
        assert_eq!(bin_for(0), Some(0));
        assert_eq!(bin_for(1024), Some(0));
        assert_eq!(bin_for(1025), Some(1));
        assert_eq!(bin_for(65_536), Some(6));
        assert_eq!(bin_for(65_537), None);
    }

    #[test]
    fn bin_for_cap_picks_largest_class_leq_cap() {
        assert_eq!(bin_for_cap(1500), Some(0)); // between 1024 and 2048
        assert_eq!(bin_for_cap(1024), Some(0));
        assert_eq!(bin_for_cap(1023), None); // under smallest class
        assert_eq!(bin_for_cap(65_536), Some(6));
        assert_eq!(bin_for_cap(65_537), None); // over largest class
    }

    #[test]
    fn bin_for_cap_of_a_bin_for_class_size_is_that_same_bin() {
        for (idx, &class) in CHUNK_CLASSES.iter().enumerate() {
            assert_eq!(bin_for(class), Some(idx));
            assert_eq!(bin_for_cap(class), Some(idx));
        }
    }
}
