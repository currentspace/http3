//! Loom model for the BufferRecycler ownership contract.
//!
//! The production recycler uses `crossbeam_channel::Sender::try_send`, which
//! is not loom-instrumented. This test models the same ownership boundary:
//! a finalized V8 buffer is accepted by the worker-side return queue, falls
//! back to a right-sized global pool when the queue is full/closed, or is
//! dropped only after that fallback is full. Each buffer is still owned exactly
//! once across possible GC-thread/worker-thread interleavings.

use loom::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use loom::thread;

#[derive(Clone)]
struct ModelRecycler {
    queue: Arc<Mutex<Vec<usize>>>,
    retained: Arc<Mutex<Vec<usize>>>,
    fallback: Arc<Mutex<Vec<usize>>>,
    dropped: Arc<Mutex<Vec<usize>>>,
    receiver_alive: Arc<AtomicBool>,
    capacity: usize,
    fallback_capacity: usize,
}

impl ModelRecycler {
    fn recycle(&self, buffer_id: usize) {
        let mut queue = self.queue.lock().unwrap();
        if self.receiver_alive.load(Ordering::Acquire) && queue.len() < self.capacity {
            queue.push(buffer_id);
        } else {
            let mut fallback = self.fallback.lock().unwrap();
            if fallback.len() < self.fallback_capacity {
                fallback.push(buffer_id);
            } else {
                drop(fallback);
                self.dropped.lock().unwrap().push(buffer_id);
            }
        }
    }

    fn drain(&self) {
        let mut queue = self.queue.lock().unwrap();
        let mut retained = self.retained.lock().unwrap();
        retained.extend(queue.drain(..));
    }

    fn close_receiver(&self) {
        self.drain();
        self.receiver_alive.store(false, Ordering::Release);
    }
}

#[test]
fn recycler_returns_or_drops_each_buffer_once() {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;

    builder.check(|| {
        let recycler = ModelRecycler {
            queue: Arc::new(Mutex::new(Vec::new())),
            retained: Arc::new(Mutex::new(Vec::new())),
            fallback: Arc::new(Mutex::new(Vec::new())),
            dropped: Arc::new(Mutex::new(Vec::new())),
            receiver_alive: Arc::new(AtomicBool::new(true)),
            capacity: 1,
            fallback_capacity: 1,
        };

        recycler.recycle(1);

        let recycle_second = {
            let recycler = recycler.clone();
            thread::spawn(move || recycler.recycle(2))
        };
        let worker = {
            let recycler = recycler.clone();
            thread::spawn(move || recycler.close_receiver())
        };

        recycle_second.join().unwrap();
        worker.join().unwrap();
        recycler.close_receiver();
        recycler.recycle(3);

        let mut seen = Vec::new();
        seen.extend(recycler.retained.lock().unwrap().iter().copied());
        seen.extend(recycler.fallback.lock().unwrap().iter().copied());
        seen.extend(recycler.dropped.lock().unwrap().iter().copied());
        seen.sort_unstable();

        assert_eq!(seen, vec![1, 2, 3]);
    });
}
