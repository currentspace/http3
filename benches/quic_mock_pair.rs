//! Criterion benchmarks for QUIC mock pair: handshake, echo throughput,
//! and connection churn through the full event loop with MockDriver.
#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use crossbeam_channel::{Receiver, unbounded};

use http3::bench_exports::*;

// ── Shared state ────────────────────────────────────────────────────

static CONFIG_MUTEX: Mutex<()> = Mutex::new(());
static NEXT_PORT: AtomicU16 = AtomicU16::new(40_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ── Cert generation ─────────────────────────────────────────────────

fn generate_test_certs() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes())
}

// ── Pair helper ─────────────────────────────────────────────────────

struct BenchQuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_quic_pair() -> BenchQuicPair {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();

    let server_options = JsQuicServerOptions {
        key: key_pem.clone(),
        cert: cert_pem.clone(),
        ca: None,
        client_auth: None,
        alpn: None,
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(100_000_000),
        initial_max_stream_data_bidi_local: Some(2_000_000),
        initial_max_streams_bidi: Some(10_000),
        disable_active_migration: Some(true),
        enable_datagrams: Some(false),
        max_connections: Some(128),
        disable_retry: Some(true),
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        keylog: None,
    };
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: None,
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(100_000_000),
        initial_max_stream_data_bidi_local: Some(2_000_000),
        initial_max_streams_bidi: Some(10_000),
        session_ticket: None,
        allow_0rtt: None,
        enable_datagrams: Some(false),
        keylog: None,
        qlog_dir: None,
        qlog_level: None,
    };

    let (server_quiche, client_quiche) = {
        let _lock = CONFIG_MUTEX.lock().unwrap();
        let s = new_quic_server_config(&server_options).unwrap();
        let c = new_quic_client_config(&client_options).unwrap();
        (s, c)
    };

    let server_config = QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: 128,
        disable_retry: true,
        client_auth: ClientAuthMode::None,
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
    };

    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair(client_addr, server_addr);

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _server_sink_stats) = channel_batcher("server", server_event_tx);
    let (client_batcher, _client_sink_stats) = channel_batcher("client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (client_cmd_tx, client_cmd_rx) = unbounded();

    let server_worker = spawn_server_worker_on_driver(
        server_quiche,
        server_config,
        0,
        server_driver,
        server_waker,
        server_addr,
        server_cmd_tx,
        server_cmd_rx,
        server_batcher,
    );
    let server = QuicServerHandle::from_workers(vec![server_worker], server_addr);

    let client = spawn_dedicated_quic_client_on_driver(
        client_quiche,
        server_addr,
        "localhost".to_string(),
        None,
        None,
        None,
        client_driver,
        client_waker,
        client_addr,
        client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    )
    .unwrap();

    BenchQuicPair {
        server,
        client,
        server_rx: server_event_rx,
        client_rx: client_event_rx,
    }
}

// ── Event helpers ───────────────────────────────────────────────────

fn recv_event_matching(
    rx: &Receiver<TaggedEventBatch>,
    timeout: Duration,
    mut predicate: impl FnMut(&JsH3Event) -> bool,
) -> Option<JsH3Event> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if predicate(&event) {
                        return Some(event);
                    }
                }
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_handshake(pair: &BenchQuicPair) -> (u32, u32) {
    let server_new = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server should receive NEW_SESSION");

    let client_hs = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client should receive HANDSHAKE_COMPLETE");

    (server_new.conn_handle, client_hs.conn_handle)
}

// ── Benchmarks ──────────────────────────────────────────────────────

fn quic_mock_handshake(c: &mut Criterion) {
    c.bench_function("quic_mock_handshake", |b| {
        b.iter(|| {
            let pair = setup_quic_pair();
            let (_server_conn, _client_conn) = wait_for_handshake(&pair);
        });
    });
}

fn quic_mock_echo_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("quic_mock_echo_throughput");
    for stream_count in [1, 5, 10] {
        group.bench_with_input(
            BenchmarkId::from_parameter(stream_count),
            &stream_count,
            |b, &count| {
                let payload = vec![0xAA_u8; 4096]; // 4KB per stream
                b.iter(|| {
                    let pair = setup_quic_pair();
                    let (server_conn, _client_conn) = wait_for_handshake(&pair);

                    // Client sends 4KB on each stream
                    for i in 0..count {
                        let stream_id = i as u64 * 4;
                        pair.client.stream_send(stream_id, payload.clone(), true);
                    }

                    // Server collects and echoes each stream
                    let mut streams_done = std::collections::HashSet::new();
                    let mut pending: std::collections::HashMap<i64, Vec<u8>> =
                        std::collections::HashMap::new();
                    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
                    while streams_done.len() < count
                        && std::time::Instant::now() < deadline
                    {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        match pair.server_rx.recv_timeout(remaining) {
                            Ok(batch) => {
                                for event in batch.events {
                                    let sid = event.stream_id;
                                    if event.event_type == EVENT_NEW_STREAM
                                        || event.event_type == EVENT_DATA
                                    {
                                        if let Some(d) = event.data.as_ref() {
                                            pending.entry(sid).or_default().extend_from_slice(d);
                                        }
                                        if event.fin == Some(true) {
                                            streams_done.insert(sid);
                                        }
                                    }
                                    if event.event_type == EVENT_FINISHED {
                                        streams_done.insert(sid);
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }

                    // Echo back
                    for i in 0..count {
                        let stream_id = i as u64 * 4;
                        let data = pending
                            .remove(&(stream_id as i64))
                            .unwrap_or_default();
                        pair.server.send_command(QuicServerCommand::StreamSend {
                            conn_handle: server_conn,
                            stream_id,
                            chunk: Chunk::unpooled(data),
                            fin: true,
                        });
                    }

                    // Client drains all echoed streams
                    let mut client_done = std::collections::HashSet::new();
                    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
                    while client_done.len() < count
                        && std::time::Instant::now() < deadline
                    {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        match pair.client_rx.recv_timeout(remaining) {
                            Ok(batch) => {
                                for event in batch.events {
                                    if (event.event_type == EVENT_NEW_STREAM
                                        || event.event_type == EVENT_DATA
                                        || event.event_type == EVENT_FINISHED)
                                        && (event.fin == Some(true)
                                            || event.event_type == EVENT_FINISHED)
                                    {
                                        client_done.insert(event.stream_id);
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

fn quic_mock_connection_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("quic_mock_connection_churn");
    for conn_count in [5, 10] {
        group.bench_with_input(
            BenchmarkId::from_parameter(conn_count),
            &conn_count,
            |b, &count| {
                b.iter(|| {
                    for _ in 0..count {
                        let mut pair = setup_quic_pair();
                        let (_server_conn, _client_conn) = wait_for_handshake(&pair);
                        pair.client.shutdown();
                        pair.server.shutdown();
                    }
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    quic_mock_handshake,
    quic_mock_echo_throughput,
    quic_mock_connection_churn
);
criterion_main!(benches);
