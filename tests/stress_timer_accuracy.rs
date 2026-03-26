//! Timer accuracy stress tests for TimerHeap under heavy load.
//! All tests are `#[ignore]` so they do not run in CI.
//! Run with: cargo test --test stress_timer_accuracy --features bench-internals --no-default-features -- --ignored
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::collections::HashSet;
use std::time::{Duration, Instant};

use http3::bench_exports::*;

// ===========================================================================
// Tests
// ===========================================================================

#[test]
#[ignore]
fn test_timer_heap_under_load() {
    let mut heap = TimerHeap::new();
    let now = Instant::now();

    // Schedule 10,000 handles with pseudo-random deadlines.
    // handle_id * 7 % 5000 gives offsets in 0..4999 ms range.
    let handle_count = 10_000usize;
    for handle_id in 0..handle_count {
        let offset_ms = (handle_id * 7) % 5000;
        let deadline = now + Duration::from_millis(offset_ms as u64);
        heap.schedule(handle_id, deadline);
    }

    // Pop expired handles at 100ms intervals until now + 6s.
    // All handles should have deadlines within 0..5000ms, so 6s is enough.
    let mut all_expired = Vec::new();
    let mut check_time = now;
    let end_time = now + Duration::from_secs(6);

    while check_time <= end_time {
        let expired = heap.pop_expired(check_time);
        all_expired.extend(expired);
        check_time += Duration::from_millis(100);
    }

    // Verify all 10,000 handles were expired
    assert_eq!(
        all_expired.len(),
        handle_count,
        "expected all {handle_count} handles to be expired, got {}",
        all_expired.len()
    );

    // Verify no handle was returned twice
    let unique: HashSet<usize> = all_expired.iter().copied().collect();
    assert_eq!(
        unique.len(),
        handle_count,
        "expected {handle_count} unique handles, got {} (duplicates detected)",
        unique.len()
    );

    // Verify every handle_id in 0..10_000 is present
    for handle_id in 0..handle_count {
        assert!(
            unique.contains(&handle_id),
            "handle {handle_id} was not returned by pop_expired"
        );
    }

    eprintln!(
        "timer heap: all {handle_count} handles expired correctly with no duplicates"
    );
}

#[test]
#[ignore]
fn test_timer_reschedule_under_load() {
    let mut heap = TimerHeap::new();
    let now = Instant::now();

    let handle_count = 1000usize;
    let reschedules_per_handle = 10;

    // Schedule each handle, then reschedule it 10 times (10,000 total calls).
    // Each reschedule pushes the deadline further out.
    for handle_id in 0..handle_count {
        for round in 0..=reschedules_per_handle {
            // Each reschedule moves the deadline by (round * 100)ms
            let offset_ms = (round as u64) * 100 + (handle_id as u64 % 50);
            let deadline = now + Duration::from_millis(offset_ms);
            heap.schedule(handle_id, deadline);
        }
    }

    // After all reschedules, each handle's canonical deadline is:
    // now + 10*100ms + (handle_id % 50)ms = now + 1000..1049ms
    // So at now + 10s, all handles should be expired.
    let check_time = now + Duration::from_secs(10);
    let expired = heap.pop_expired(check_time);

    // Verify we got all 1000 handles exactly once
    assert_eq!(
        expired.len(),
        handle_count,
        "expected {handle_count} expired handles, got {}",
        expired.len()
    );

    let unique: HashSet<usize> = expired.iter().copied().collect();
    assert_eq!(
        unique.len(),
        handle_count,
        "expected {handle_count} unique handles, got {} (duplicates detected)",
        unique.len()
    );

    for handle_id in 0..handle_count {
        assert!(
            unique.contains(&handle_id),
            "handle {handle_id} was not returned after reschedule"
        );
    }

    // Verify the heap is now empty
    let leftover = heap.pop_expired(check_time + Duration::from_secs(100));
    assert!(
        leftover.is_empty(),
        "heap should be empty after popping all handles, got {} leftover",
        leftover.len()
    );

    eprintln!(
        "timer reschedule: {handle_count} handles x {reschedules_per_handle} reschedules \
         all resolved correctly"
    );
}
