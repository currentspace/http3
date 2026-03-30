//! Buffer pool allocation benchmarks.
#![allow(clippy::unwrap_used)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use http3::bench_exports::BufferPool;

fn pool_checkout_checkin(c: &mut Criterion) {
    c.bench_function("pool_checkout_checkin", |b| {
        let mut pool = BufferPool::new(256, 65535);
        b.iter(|| {
            let buf = pool.checkout();
            pool.checkin(buf);
        });
    });
}

fn fresh_vec_alloc(c: &mut Criterion) {
    c.bench_function("fresh_vec_alloc", |b| {
        b.iter(|| {
            let buf = vec![0u8; 65535];
            std::hint::black_box(buf);
        });
    });
}

fn pool_under_pressure(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_under_pressure");
    for depleted_pct in [0, 50, 90, 100] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{depleted_pct}pct_depleted")),
            &depleted_pct,
            |b, &pct| {
                let pool_size = 256;
                let drain_count = pool_size * pct / 100;
                b.iter_batched(
                    || {
                        let mut pool = BufferPool::new(pool_size, 65535);
                        let mut drained = Vec::new();
                        for _ in 0..drain_count {
                            drained.push(pool.checkout());
                        }
                        (pool, drained)
                    },
                    |(mut pool, drained)| {
                        let buf = pool.checkout();
                        pool.checkin(buf);
                        // Return drained buffers
                        for buf in drained {
                            pool.checkin(buf);
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn copy_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_copy");
    for size in [1350, 16384, 65535] {
        let src = vec![0xAB_u8; size];
        group.bench_with_input(BenchmarkId::new("to_vec", size), &size, |b, &_size| {
            b.iter(|| {
                let v = src.to_vec();
                std::hint::black_box(v);
            });
        });
        group.bench_with_input(
            BenchmarkId::new("copy_from_slice", size),
            &size,
            |b, &sz| {
                let mut dst = vec![0u8; sz];
                b.iter(|| {
                    dst.copy_from_slice(&src);
                    std::hint::black_box(&dst);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    pool_checkout_checkin,
    fresh_vec_alloc,
    pool_under_pressure,
    copy_patterns
);
criterion_main!(benches);
