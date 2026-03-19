//! Event batcher and sink benchmarks.
#![allow(clippy::unwrap_used)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossbeam_channel::unbounded;

use http3::bench_exports::*;

fn make_dummy_events(count: usize) -> Vec<JsH3Event> {
    (0..count)
        .map(|i| JsH3Event {
            event_type: 4, // EVENT_DATA
            conn_handle: 0,
            stream_id: i as i64,
            headers: None,
            data: None,
            fin: Some(false),
            meta: None,
            metrics: None,
        })
        .collect()
}

fn batcher_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("batcher_flush");

    group.bench_function("noop", |b| {
        let (mut batcher, _stats) = noop_batcher();
        b.iter(|| {
            batcher.batch = make_dummy_events(MAX_BATCH_SIZE);
            batcher.flush();
        });
    });

    group.bench_function("counting", |b| {
        let (mut batcher, _stats) = counting_batcher();
        b.iter(|| {
            batcher.batch = make_dummy_events(MAX_BATCH_SIZE);
            batcher.flush();
        });
    });

    group.bench_function("channel", |b| {
        let (tx, rx) = unbounded();
        let (mut batcher, _stats) = channel_batcher("bench", tx);
        b.iter(|| {
            batcher.batch = make_dummy_events(MAX_BATCH_SIZE);
            batcher.flush();
            // Drain the receiver to prevent unbounded growth
            while rx.try_recv().is_ok() {}
        });
    });

    group.finish();
}

fn channel_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("channel_throughput");
    for batch_size in [64, 512, 2048] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &size| {
                let (tx, rx) = unbounded::<TaggedEventBatch>();
                b.iter(|| {
                    let events = make_dummy_events(size);
                    tx.send(TaggedEventBatch {
                        source: "bench".into(),
                        events,
                    })
                    .unwrap();
                    let batch = rx.recv().unwrap();
                    std::hint::black_box(batch.events.len());
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, batcher_flush, channel_throughput);
criterion_main!(benches);
