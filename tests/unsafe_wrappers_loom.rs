//! Loom model for the validated provided-buffer-id boundary.
//!
//! The production io_uring driver receives buffer IDs from the kernel and must
//! reject out-of-range values before any raw pointer arithmetic. This model
//! checks that concurrent staging/draining logic only ever observes IDs that
//! passed the wrapper constructor.

use http3::unsafe_boundary::{ProvidedBufferId, QuicheRecvBuf};
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

#[test]
fn quiche_recv_buf_only_commits_initialized_bytes_under_interleaving() {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;

    builder.check(|| {
        let recv = Arc::new(Mutex::new(Some(QuicheRecvBuf::with_capacity(4))));

        let write_one = {
            let recv = Arc::clone(&recv);
            thread::spawn(move || {
                let mut guard = recv.lock().unwrap();
                let recv = guard.as_mut().expect("buffer should be present");
                assert_eq!(recv.append_initialized(&[1]), 1);
            })
        };

        let write_two = {
            let recv = Arc::clone(&recv);
            thread::spawn(move || {
                let mut guard = recv.lock().unwrap();
                let recv = guard.as_mut().expect("buffer should be present");
                assert_eq!(recv.append_initialized(&[2]), 1);
            })
        };

        write_one.join().unwrap();
        write_two.join().unwrap();

        let recv = recv.lock().unwrap().take().expect("buffer should remain");
        assert_eq!(recv.initialized_len(), 2);

        let mut committed = recv.into_initialized_vec();
        committed.sort_unstable();
        assert_eq!(committed, vec![1, 2]);
    });
}
