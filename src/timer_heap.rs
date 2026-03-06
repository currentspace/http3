use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Instant;

pub struct TimerHeap {
    heap: BinaryHeap<Reverse<(Instant, usize)>>,
    /// Lazily removed handles — checked on pop
    removed: std::collections::HashSet<usize>,
}

impl TimerHeap {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            removed: std::collections::HashSet::new(),
        }
    }

    pub fn schedule(&mut self, handle: usize, deadline: Instant) {
        self.removed.remove(&handle);
        self.heap.push(Reverse((deadline, handle)));
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        // Skip removed entries
        while let Some(Reverse((_, handle))) = self.heap.peek() {
            if self.removed.contains(handle) {
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
            if self.removed.contains(&handle) {
                self.heap.pop();
                continue;
            }
            if deadline > now {
                break;
            }
            self.heap.pop();
            expired.push(handle);
        }
        expired
    }

    pub fn remove_connection(&mut self, handle: usize) {
        self.removed.insert(handle);
        // Periodically clean up if too many removed entries
        if self.removed.len() > self.heap.len() / 2 + 16 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let old_heap = std::mem::take(&mut self.heap);
        for entry in old_heap.into_vec() {
            if !self.removed.contains(&entry.0.1) {
                self.heap.push(entry);
            }
        }
        self.removed.clear();
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
    fn test_empty_heap() {
        let mut heap = TimerHeap::new();
        assert_eq!(heap.next_deadline(), None);
        assert!(heap.pop_expired(Instant::now()).is_empty());
    }
}
