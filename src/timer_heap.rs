//! Min-heap for QUIC connection timeout deadlines, supporting lazy removal
//! and efficient next-expiry queries.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

pub struct TimerHeap {
    heap: BinaryHeap<Reverse<(Instant, usize)>>,
    /// Canonical deadline per handle; stale heap entries are skipped on read.
    deadlines: HashMap<usize, Instant>,
}

impl TimerHeap {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            deadlines: HashMap::new(),
        }
    }

    pub fn schedule(&mut self, handle: usize, deadline: Instant) {
        self.deadlines.insert(handle, deadline);
        self.heap.push(Reverse((deadline, handle)));
        self.maybe_compact();
    }

    pub fn set_deadline(&mut self, handle: usize, deadline: Option<Instant>) {
        if let Some(deadline) = deadline {
            self.schedule(handle, deadline);
        } else {
            self.remove_connection(handle);
        }
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(&Reverse((deadline, handle))) = self.heap.peek() {
            if !self.is_current_deadline(handle, deadline) {
                self.heap.pop();
            } else {
                break;
            }
        }
        self.heap.peek().map(|Reverse((deadline, _))| *deadline)
    }

    pub fn pop_expired(&mut self, now: Instant) -> Vec<usize> {
        let mut expired = Vec::new();
        while let Some(&Reverse((deadline, handle))) = self.heap.peek() {
            if !self.is_current_deadline(handle, deadline) {
                self.heap.pop();
                continue;
            }
            if deadline > now {
                break;
            }
            self.heap.pop();
            self.deadlines.remove(&handle);
            expired.push(handle);
        }
        expired
    }

    pub fn remove_connection(&mut self, handle: usize) {
        self.deadlines.remove(&handle);
        self.maybe_compact();
    }

    fn is_current_deadline(&self, handle: usize, deadline: Instant) -> bool {
        matches!(self.deadlines.get(&handle), Some(current) if *current == deadline)
    }

    fn maybe_compact(&mut self) {
        let active = self.deadlines.len();
        if self.heap.len() > active.saturating_mul(4) + 32 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let old_heap = std::mem::take(&mut self.heap);
        for Reverse((deadline, handle)) in old_heap.into_vec() {
            if self.is_current_deadline(handle, deadline) {
                self.heap.push(Reverse((deadline, handle)));
            }
        }
    }
}

impl Default for TimerHeap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_schedule_and_pop() {
        let mut heap = TimerHeap::new();
        let now = Instant::now();

        heap.schedule(1, now + Duration::from_millis(100));
        heap.schedule(2, now + Duration::from_millis(50));
        heap.schedule(3, now + Duration::from_millis(200));

        assert_eq!(heap.next_deadline(), Some(now + Duration::from_millis(50)));

        let expired = heap.pop_expired(now + Duration::from_millis(120));
        assert_eq!(expired, vec![2, 1]);
    }

    #[test]
    fn test_remove_connection() {
        let mut heap = TimerHeap::new();
        let now = Instant::now();

        heap.schedule(1, now + Duration::from_millis(100));
        heap.schedule(2, now + Duration::from_millis(50));

        heap.remove_connection(2);

        assert_eq!(heap.next_deadline(), Some(now + Duration::from_millis(100)));
        let expired = heap.pop_expired(now + Duration::from_millis(200));
        assert_eq!(expired, vec![1]);
    }

    #[test]
    fn test_reschedule_same_handle_replaces_previous_deadline() {
        let mut heap = TimerHeap::new();
        let now = Instant::now();

        heap.schedule(7, now + Duration::from_millis(25));
        heap.schedule(7, now + Duration::from_millis(100));

        assert_eq!(heap.next_deadline(), Some(now + Duration::from_millis(100)));
        assert!(heap.pop_expired(now + Duration::from_millis(50)).is_empty());
        assert_eq!(heap.pop_expired(now + Duration::from_millis(125)), vec![7]);
    }

    #[test]
    fn test_set_deadline_none_clears_handle() {
        let mut heap = TimerHeap::new();
        let now = Instant::now();

        heap.set_deadline(5, Some(now + Duration::from_millis(40)));
        heap.set_deadline(6, Some(now + Duration::from_millis(80)));
        heap.set_deadline(5, None);

        assert_eq!(heap.next_deadline(), Some(now + Duration::from_millis(80)));
        assert_eq!(heap.pop_expired(now + Duration::from_millis(120)), vec![6]);
    }

    #[test]
    fn test_empty_heap() {
        let mut heap = TimerHeap::new();
        assert_eq!(heap.next_deadline(), None);
        assert!(heap.pop_expired(Instant::now()).is_empty());
    }
}
