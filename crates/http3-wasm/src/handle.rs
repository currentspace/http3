//! Generic handle table (u32 slab indices; 0 is never a valid handle).
//!
//! wasm32-wasip1 is single-threaded, so a plain `Vec<Option<T>>` behind a
//! `thread_local!` `RefCell` (see `h3.rs` / `quic.rs`) is sufficient — no
//! atomics or locking needed.

pub(crate) struct Slots<T> {
    slots: Vec<Option<T>>,
}

impl<T> Slots<T> {
    pub(crate) const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Insert a value, returning its 1-indexed handle (0 is reserved).
    /// Reuses the first free slot so long-lived processes don't grow the
    /// table unboundedly across many short sessions.
    pub(crate) fn insert(&mut self, value: T) -> u32 {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(value);
                return u32_from_index(i);
            }
        }
        self.slots.push(Some(value));
        u32_from_index(self.slots.len() - 1)
    }

    pub(crate) fn get_mut(&mut self, handle: u32) -> Option<&mut T> {
        index_from_handle(handle).and_then(|i| self.slots.get_mut(i).and_then(Option::as_mut))
    }

    pub(crate) fn get(&self, handle: u32) -> Option<&T> {
        index_from_handle(handle).and_then(|i| self.slots.get(i).and_then(Option::as_ref))
    }

    pub(crate) fn remove(&mut self, handle: u32) -> Option<T> {
        index_from_handle(handle).and_then(|i| self.slots.get_mut(i).and_then(Option::take))
    }
}

fn u32_from_index(i: usize) -> u32 {
    // Sessions are bounded by realistic per-process connection counts; a
    // 4-billion-entry table would already have exhausted memory long
    // before this saturates.
    u32::try_from(i + 1).unwrap_or(u32::MAX)
}

fn index_from_handle(handle: u32) -> Option<usize> {
    if handle == 0 {
        return None;
    }
    Some((handle - 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::Slots;

    #[test]
    fn zero_is_never_a_valid_handle() {
        let slots: Slots<u32> = Slots::new();
        assert!(slots.get(0).is_none());
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let mut slots: Slots<&str> = Slots::new();
        let h1 = slots.insert("a");
        let h2 = slots.insert("b");
        assert_ne!(h1, 0);
        assert_ne!(h2, 0);
        assert_ne!(h1, h2);
        assert_eq!(slots.get(h1), Some(&"a"));
        assert_eq!(slots.get(h2), Some(&"b"));
        assert_eq!(slots.remove(h1), Some("a"));
        assert_eq!(slots.get(h1), None);
        assert_eq!(slots.get(h2), Some(&"b"));
    }

    #[test]
    fn reuses_freed_slots() {
        let mut slots: Slots<u32> = Slots::new();
        let h1 = slots.insert(1);
        slots.remove(h1);
        let h2 = slots.insert(2);
        assert_eq!(h1, h2, "freed slot should be reused rather than growing the table");
    }

    #[test]
    fn invalid_handle_returns_none_not_panic() {
        let mut slots: Slots<u32> = Slots::new();
        assert!(slots.get_mut(9999).is_none());
        assert!(slots.remove(9999).is_none());
    }
}
