//! Head-to-head io_uring vs poll driver benchmarks.
//! Uses real UDP socket pairs on localhost with zero quiche involvement.
#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use http3::bench_exports::*;

// ── Helpers ─────────────────────────────────────────────────────────

fn make_socket_pair() -> (UdpSocket, UdpSocket) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    a.set_nonblocking(true).unwrap();
    b.set_nonblocking(true).unwrap();
    // Increase socket buffers for burst benchmarks
    let sock_a = socket2::Socket::from(a);
    let sock_b = socket2::Socket::from(b);
    let _ = sock_a.set_send_buffer_size(2 * 1024 * 1024);
    let _ = sock_a.set_recv_buffer_size(2 * 1024 * 1024);
    let _ = sock_b.set_send_buffer_size(2 * 1024 * 1024);
    let _ = sock_b.set_recv_buffer_size(2 * 1024 * 1024);
    let a: UdpSocket = sock_a.into();
    let b: UdpSocket = sock_b.into();
    // Connect each to the other for send/recv without address
    a.connect(b.local_addr().unwrap()).unwrap();
    b.connect(a.local_addr().unwrap()).unwrap();
    (a, b)
}

fn make_packets(count: usize, to: SocketAddr) -> Vec<TxDatagram> {
    let payload = vec![0xAB_u8; 1350];
    (0..count)
        .map(|_| TxDatagram {
            data: payload.clone(),
            to,
        })
        .collect()
}

// ── Generic driver benchmarks ───────────────────────────────────────

fn bench_submit_sends<D: Driver>(c: &mut Criterion, driver_name: &str) {
    let mut group = c.benchmark_group(format!("driver_submit_sends/{driver_name}"));
    for batch_size in [1, 16, 64, 256] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &size| {
                let (sock_a, _sock_b) = make_socket_pair();
                let peer_addr = _sock_b.local_addr().unwrap();
                let (mut driver, _waker) = D::new(sock_a).unwrap();
                b.iter(|| {
                    let packets = make_packets(size, peer_addr);
                    driver.submit_sends(packets).unwrap();
                    // Drain recycled buffers to prevent unbounded growth
                    let _ = driver.drain_recycled_tx();
                });
            },
        );
    }
    group.finish();
}

fn bench_poll_rx_group<D: Driver>(c: &mut Criterion, driver_name: &str) {
    let mut group = c.benchmark_group(format!("driver_poll_rx/{driver_name}"));
    for burst_size in [1, 16, 64, 128] {
        group.bench_with_input(
            BenchmarkId::from_parameter(burst_size),
            &burst_size,
            |b, &size| {
                let (sock_a, sock_b) = make_socket_pair();
                let (mut driver, _waker) = D::new(sock_a).unwrap();
                // Initial poll to set up io_uring RX slots
                let _ = driver.poll(Some(Instant::now() + Duration::from_millis(1)));
                let payload = vec![0xCD_u8; 1350];

                b.iter(|| {
                    // Send burst
                    for _ in 0..size {
                        let _ = sock_b.send(&payload);
                    }
                    // Poll until all received
                    let mut received = 0;
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while received < size && Instant::now() < deadline {
                        let outcome = driver
                            .poll(Some(Instant::now() + Duration::from_millis(50)))
                            .unwrap();
                        received += outcome.rx.len();
                    }
                    // Don't assert — under heavy benchmarking some packets may be
                    // dropped by the kernel. The timing still captures the cost.
                });
            },
        );
    }
    group.finish();
}

fn bench_roundtrip<D: Driver>(c: &mut Criterion, driver_name: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    c.bench_function(&format!("driver_roundtrip/{driver_name}"), |b| {
        let (sock_a, sock_b) = make_socket_pair();
        let peer_addr = sock_b.local_addr().unwrap();
        let (mut driver, _waker) = D::new(sock_a).unwrap();
        let _ = driver.poll(Some(Instant::now() + Duration::from_millis(1)));

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let echo_handle = {
            let sock_b_clone = sock_b.try_clone().unwrap();
            thread::spawn(move || {
                let mut buf = [0u8; 65535];
                sock_b_clone.set_nonblocking(false).unwrap();
                sock_b_clone
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                while !stop_clone.load(Ordering::Relaxed) {
                    match sock_b_clone.recv(&mut buf) {
                        Ok(n) => { let _ = sock_b_clone.send(&buf[..n]); }
                        Err(_) => {}
                    }
                }
            })
        };

        b.iter(|| {
            let packets = make_packets(1, peer_addr);
            driver.submit_sends(packets).unwrap();
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                let outcome = driver
                    .poll(Some(Instant::now() + Duration::from_millis(50)))
                    .unwrap();
                if !outcome.rx.is_empty() {
                    break;
                }
                if Instant::now() >= deadline {
                    panic!("roundtrip timeout");
                }
            }
            let _ = driver.drain_recycled_tx();
        });

        stop.store(true, Ordering::Relaxed);
        let _ = echo_handle.join();
    });
}

fn bench_poll_idle_wake<D: Driver>(c: &mut Criterion, driver_name: &str) {
    c.bench_function(&format!("driver_poll_idle_wake/{driver_name}"), |b| {
        let (sock_a, _sock_b) = make_socket_pair();
        let (mut driver, waker) = D::new(sock_a).unwrap();
        // Initial poll
        let _ = driver.poll(Some(Instant::now() + Duration::from_millis(1)));

        b.iter(|| {
            // Schedule a wake from another thread
            let w = waker.clone();
            let t = thread::spawn(move || {
                w.wake().unwrap();
            });
            // Poll should return quickly due to waker
            let outcome = driver
                .poll(Some(Instant::now() + Duration::from_millis(500)))
                .unwrap();
            assert!(outcome.woken);
            t.join().unwrap();
        });
    });
}

// ── Criterion groups ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn io_uring_benches(c: &mut Criterion) {
    bench_submit_sends::<IoUringDriver>(c, "io_uring");
    bench_poll_rx_group::<IoUringDriver>(c, "io_uring");
    bench_roundtrip::<IoUringDriver>(c, "io_uring");
    bench_poll_idle_wake::<IoUringDriver>(c, "io_uring");
}

#[cfg(target_os = "linux")]
fn poll_benches(c: &mut Criterion) {
    bench_submit_sends::<PollDriver>(c, "poll");
    bench_poll_rx_group::<PollDriver>(c, "poll");
    bench_roundtrip::<PollDriver>(c, "poll");
    bench_poll_idle_wake::<PollDriver>(c, "poll");
}

#[cfg(not(target_os = "linux"))]
fn io_uring_benches(_c: &mut Criterion) {}

#[cfg(not(target_os = "linux"))]
fn poll_benches(_c: &mut Criterion) {}

criterion_group!(benches, io_uring_benches, poll_benches);
criterion_main!(benches);
