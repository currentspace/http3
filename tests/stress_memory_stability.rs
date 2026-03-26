//! Memory stability stress tests for buffer pools.
//! All tests are `#[ignore]` so they do not run in CI.
//! Run with: cargo test --test stress_memory_stability --features bench-internals --no-default-features -- --ignored
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::time::Instant;

use http3::bench_exports::*;

// ===========================================================================
// Tests
// ===========================================================================

#[test]
#[ignore]
fn test_buffer_pool_stable_under_churn() {
    let mut pool = BufferPool::new(16, 1500);
    let start = Instant::now();

    // Perform 100,000 checkout/checkin cycles
    for _ in 0..100_000 {
        let buf = pool.checkout();
        assert_eq!(buf.len(), 1500, "checkout should return buffer of buf_size");
        pool.checkin(buf);
    }

    let elapsed = start.elapsed();
    eprintln!(
        "BufferPool 100K checkout/checkin cycles completed in {:.3}s",
        elapsed.as_secs_f64()
    );

    // Verify pool is still functional: checkout 16 buffers and check them
    // all back in.  If the pool is stable, all 16 should come from the pool
    // without extra allocations.
    let mut buffers = Vec::new();
    for _ in 0..16 {
        let buf = pool.checkout();
        assert_eq!(buf.len(), 1500);
        buffers.push(buf);
    }
    // The 17th checkout should still work (fallback allocation)
    let extra = pool.checkout();
    assert_eq!(extra.len(), 1500);

    // Return all buffers
    for buf in buffers {
        pool.checkin(buf);
    }
    pool.checkin(extra);

    // Verify the pool is still functional after all churn
    let final_buf = pool.checkout();
    assert_eq!(
        final_buf.len(),
        1500,
        "pool should remain functional after 100K cycles"
    );
    pool.checkin(final_buf);
}

#[test]
#[ignore]
fn test_adaptive_pool_stable_under_churn() {
    let mut pool = AdaptiveBufferPool::new(32, 64);
    let start = Instant::now();

    // Track how many buffers we've checked in vs how many the pool retained
    let mut total_checkins: u64 = 0;
    let mut retained_count: u64 = 0;

    // Perform 100,000 checkout/checkin cycles with random-ish sizes
    for i in 0u64..100_000 {
        // Pseudo-random size between 8 and 8192 using a simple hash
        let size = ((i.wrapping_mul(2654435761) >> 16) % 8185 + 8) as usize;

        let (buf, _reused) = pool.checkout(size);
        assert_eq!(buf.len(), size, "checkout should return buffer of requested size");
        assert!(
            buf.capacity() >= size,
            "buffer capacity should be >= requested size"
        );

        let was_retained = pool.checkin(buf);
        total_checkins += 1;
        if was_retained {
            retained_count += 1;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "AdaptiveBufferPool 100K cycles: {retained_count}/{total_checkins} retained, {:.3}s",
        elapsed.as_secs_f64()
    );

    // Verify pool doesn't grow unbounded: checkout up to max_buffers+1 and
    // verify the pool handles it gracefully.
    let mut buffers = Vec::new();
    for _ in 0..33 {
        let (buf, _reused) = pool.checkout(128);
        assert_eq!(buf.len(), 128);
        buffers.push(buf);
    }

    // Return them all — only max_buffers (32) should be retained
    let mut final_retained = 0;
    for buf in buffers {
        if pool.checkin(buf) {
            final_retained += 1;
        }
    }
    assert!(
        final_retained <= 32,
        "pool should not retain more than max_buffers (32), got {final_retained}"
    );

    // Pool should still be functional
    let (final_buf, _) = pool.checkout(256);
    assert_eq!(final_buf.len(), 256);
    pool.checkin(final_buf);
}
