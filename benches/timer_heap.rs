//! Timer heap benchmarks.
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use http3::bench_exports::TimerHeap;

fn bench_schedule(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_schedule");
    for count in [10, 100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            let now = Instant::now();
            b.iter(|| {
                let mut heap = TimerHeap::new();
                for i in 0..n {
                    heap.schedule(i, now + Duration::from_millis(i as u64));
                }
                std::hint::black_box(&heap);
            });
        });
    }
    group.finish();
}

fn bench_pop_expired(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_pop_expired");
    for count in [10, 100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            let now = Instant::now();
            b.iter_batched(
                || {
                    let mut heap = TimerHeap::new();
                    for i in 0..n {
                        heap.schedule(i, now + Duration::from_millis(i as u64));
                    }
                    heap
                },
                |mut heap| {
                    let expired = heap.pop_expired(now + Duration::from_secs(1));
                    assert_eq!(expired.len(), n);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_churn");
    for count in [100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            let now = Instant::now();
            b.iter(|| {
                let mut heap = TimerHeap::new();
                // Schedule
                for i in 0..n {
                    heap.schedule(i, now + Duration::from_millis(i as u64));
                }
                // Remove half
                for i in (0..n).step_by(2) {
                    heap.remove_connection(i);
                }
                // Re-schedule removed ones
                for i in (0..n).step_by(2) {
                    heap.schedule(i, now + Duration::from_millis((i + n) as u64));
                }
                // Pop all expired
                let expired = heap.pop_expired(now + Duration::from_secs(10));
                std::hint::black_box(expired);
            });
        });
    }
    group.finish();
}

fn bench_reschedule_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_reschedule_churn");
    for count in [100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            let now = Instant::now();
            b.iter(|| {
                let mut heap = TimerHeap::new();
                for i in 0..n {
                    heap.schedule(i, now + Duration::from_millis(i as u64));
                }
                for round in 0..8usize {
                    for i in 0..n {
                        let jitter = (round * n + i) as u64;
                        heap.schedule(i, now + Duration::from_millis((n as u64) + jitter));
                    }
                }
                std::hint::black_box(heap.next_deadline());
                let expired = heap.pop_expired(now + Duration::from_secs(30));
                std::hint::black_box(expired);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_schedule,
    bench_pop_expired,
    bench_churn,
    bench_reschedule_churn
);
criterion_main!(benches);
