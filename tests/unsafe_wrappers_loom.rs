//! Loom model for the validated provided-buffer-id boundary.
//!
//! The production io_uring driver receives buffer IDs from the kernel and must
//! reject out-of-range values before any raw pointer arithmetic. This model
//! checks that concurrent staging/draining logic only ever observes IDs that
//! passed the wrapper constructor.

use http3::unsafe_boundary::ProvidedBufferId;
use loom::sync::{Arc, Mutex};
use loom::thread;

#[test]
fn provided_buffer_ids_are_validated_before_staging() {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;

    builder.check(|| {
        const RING_SIZE: u16 = 4;

        let staged = Arc::new(Mutex::new(Vec::new()));

        let stage_valid = {
            let staged = Arc::clone(&staged);
            thread::spawn(move || {
                if let Some(id) = ProvidedBufferId::new(3, RING_SIZE) {
                    staged.lock().unwrap().push(id.get());
                }
            })
        };

        let reject_invalid = {
            let staged = Arc::clone(&staged);
            thread::spawn(move || {
                if let Some(id) = ProvidedBufferId::new(4, RING_SIZE) {
                    staged.lock().unwrap().push(id.get());
                }
            })
        };

        stage_valid.join().unwrap();
        reject_invalid.join().unwrap();

        let staged = staged.lock().unwrap();
        assert!(staged.iter().all(|&id| id < RING_SIZE));
        assert_eq!(staged.iter().filter(|&&id| id == 3).count(), 1);
    });
}
