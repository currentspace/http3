//! Sustained throughput stress tests using MockDriver pairs (in-memory, no UDP).
//! All tests are `#[ignore]` so they do not run in CI.
//! Run with: cargo test --test stress_sustained_throughput --features bench-internals --no-default-features -- --ignored
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};

use http3::bench_exports::*;

/// Config creation writes PEM bytes to temp files keyed only by PID, so
/// parallel tests within the same process can race.  Serialize behind this
/// mutex.
static CONFIG_MUTEX: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Port allocator
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(62_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ---------------------------------------------------------------------------
// Cert generation
// ---------------------------------------------------------------------------

fn generate_test_certs() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes())
}

fn generate_test_certs_string() -> (String, String) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair.serialize_pem())
}

// ---------------------------------------------------------------------------
// QUIC MockDriver pair setup
// ---------------------------------------------------------------------------

struct QuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

fn setup_quic_pair() -> QuicPair {
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
        keepalive_interval_ms: None,
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
        keepalive_interval_ms: None,
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
        keepalive_interval: None,
    };

    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair(client_addr, server_addr);

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _server_sink_stats) = channel_batcher("server", server_event_tx);
    let (client_batcher, _client_sink_stats) = channel_batcher("client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (_client_cmd_tx, client_cmd_rx) = unbounded();

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
        None,
        client_driver,
        client_waker,
        client_addr,
        _client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    )
    .unwrap();

    QuicPair {
        server,
        client,
        server_rx: server_event_rx,
        client_rx: client_event_rx,
    }
}

// ---------------------------------------------------------------------------
// H3 MockDriver pair setup
// ---------------------------------------------------------------------------

struct H3Pair {
    server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

fn setup_h3_pair() -> H3Pair {
    let (cert_pem, key_pem) = generate_test_certs_string();
    let (client_addr, server_addr) = next_pair_addrs();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("stress_h3_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("stress_h3_key_{id:?}.pem"));
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();

    let (server_quiche, client_quiche) = {
        let _lock = CONFIG_MUTEX.lock().unwrap();

        let mut server_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        server_config
            .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
            .unwrap();
        server_config
            .load_priv_key_from_pem_file(key_path.to_str().unwrap())
            .unwrap();
        server_config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .unwrap();
        server_config.set_max_idle_timeout(30_000);
        server_config.set_max_recv_udp_payload_size(1472);
        server_config.set_max_send_udp_payload_size(1472);
        server_config.set_initial_max_data(100_000_000);
        server_config.set_initial_max_stream_data_bidi_local(2_000_000);
        server_config.set_initial_max_stream_data_bidi_remote(2_000_000);
        server_config.set_initial_max_stream_data_uni(2_000_000);
        server_config.set_initial_max_streams_bidi(10_000);
        server_config.set_initial_max_streams_uni(1_000);
        server_config.set_disable_active_migration(true);

        let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        client_config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .unwrap();
        client_config.verify_peer(false);
        client_config.set_max_idle_timeout(30_000);
        client_config.set_max_recv_udp_payload_size(1472);
        client_config.set_max_send_udp_payload_size(1472);
        client_config.set_initial_max_data(100_000_000);
        client_config.set_initial_max_stream_data_bidi_local(2_000_000);
        client_config.set_initial_max_stream_data_bidi_remote(2_000_000);
        client_config.set_initial_max_stream_data_uni(2_000_000);
        client_config.set_initial_max_streams_bidi(10_000);
        client_config.set_initial_max_streams_uni(1_000);

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);

        (server_config, client_config)
    };

    let http3_config = Http3Config {
        qlog_dir: None,
        qlog_level: None,
        qpack_max_table_capacity: None,
        qpack_blocked_streams: None,
        max_connections: 128,
        disable_retry: true,
        reuse_port: false,
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
        keepalive_interval: None,
    };

    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair(client_addr, server_addr);

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _server_sink_stats) = channel_batcher("h3-server", server_event_tx);
    let (client_batcher, _client_sink_stats) = channel_batcher("h3-client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (client_cmd_tx, client_cmd_rx) = unbounded();

    let server_worker = spawn_h3_server_worker_on_driver(
        server_quiche,
        http3_config,
        0,
        server_driver,
        server_waker,
        server_addr,
        server_cmd_tx,
        server_cmd_rx,
        server_batcher,
    );

    let client = spawn_h3_client_on_driver(
        client_quiche,
        server_addr,
        "localhost".to_string(),
        None,
        None,
        None,
        None,
        client_driver,
        client_waker,
        client_addr,
        client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    );

    H3Pair {
        server: server_worker,
        client,
        server_rx: server_event_rx,
        client_rx: client_event_rx,
    }
}

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

fn recv_event_matching(
    rx: &Receiver<TaggedEventBatch>,
    timeout: Duration,
    mut predicate: impl FnMut(&JsH3Event) -> bool,
) -> Option<JsH3Event> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
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

fn wait_for_quic_handshake(pair: &QuicPair) -> (u32, u32) {
    let server_new = recv_event_matching(&pair.server_rx, Duration::from_secs(5), |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server should receive NEW_SESSION");

    let client_hs = recv_event_matching(&pair.client_rx, Duration::from_secs(5), |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client should receive HANDSHAKE_COMPLETE");

    (server_new.conn_handle, client_hs.conn_handle)
}

fn wait_for_h3_handshake(pair: &H3Pair) -> (u32, u32) {
    let server_new = recv_event_matching(&pair.server_rx, Duration::from_secs(5), |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server should receive NEW_SESSION");

    let client_hs = recv_event_matching(&pair.client_rx, Duration::from_secs(5), |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client should receive HANDSHAKE_COMPLETE");

    (server_new.conn_handle, client_hs.conn_handle)
}

fn send_h3_server_cmd(pair: &H3Pair, cmd: WorkerCommand) {
    pair.server.cmd_tx.send(cmd).unwrap();
    let _ = pair.server.waker.wake();
}

// ===========================================================================
// Tests
// ===========================================================================

const TEST_DURATION: Duration = Duration::from_secs(300); // 5 minutes
const SHORT_RECV_TIMEOUT: Duration = Duration::from_millis(100);

#[test]
#[ignore]
fn test_sustained_quic_throughput_5_minutes() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_quic_handshake(&pair);

    let payload = vec![0xAA_u8; 4096]; // 4KB
    let start = Instant::now();
    let mut total_streams: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut next_stream_id: u64 = 0;

    while start.elapsed() < TEST_DURATION {
        let stream_id = next_stream_id;
        next_stream_id += 4; // QUIC bidi client-initiated: 0, 4, 8, ...

        // Client sends 4KB on a new stream with fin
        pair.client
            .stream_send(stream_id, payload.clone(), true);

        // Drain server events with short timeout, looking for data on this stream
        let mut got_server_data = false;
        let stream_deadline = Instant::now() + Duration::from_secs(5);
        while !got_server_data && Instant::now() < stream_deadline {
            match pair.server_rx.recv_timeout(SHORT_RECV_TIMEOUT) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.stream_id == stream_id as i64
                            && (event.event_type == EVENT_FINISHED
                                || (event.event_type == EVENT_DATA
                                    && event.fin == Some(true))
                                || (event.event_type == EVENT_NEW_STREAM
                                    && event.fin == Some(true)))
                        {
                            got_server_data = true;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if !got_server_data {
            // Stream didn't complete in time, skip but keep going
            continue;
        }

        // Server echoes back
        pair.server.send_command(QuicServerCommand::StreamSend {
            conn_handle: server_conn,
            stream_id,
            chunk: Chunk::unpooled(payload.clone()),
            fin: true,
        });

        // Drain client events for the echo
        let mut got_client_echo = false;
        let echo_deadline = Instant::now() + Duration::from_secs(5);
        while !got_client_echo && Instant::now() < echo_deadline {
            match pair.client_rx.recv_timeout(SHORT_RECV_TIMEOUT) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.stream_id == stream_id as i64
                            && (event.event_type == EVENT_FINISHED
                                || (event.event_type == EVENT_DATA
                                    && event.fin == Some(true)))
                        {
                            got_client_echo = true;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if got_client_echo {
            total_streams += 1;
            total_bytes += (payload.len() as u64) * 2; // send + echo
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "sustained QUIC throughput: {total_streams} streams, {total_bytes} bytes in {:.1}s",
        elapsed.as_secs_f64()
    );

    assert!(
        total_streams > 100,
        "expected > 100 completed streams, got {total_streams}"
    );
    assert!(total_bytes > 0, "expected > 0 total bytes transferred");
}

#[test]
#[ignore]
fn test_sustained_h3_throughput_5_minutes() {
    let pair = setup_h3_pair();
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    let start = Instant::now();
    let mut total_requests: u64 = 0;

    while start.elapsed() < TEST_DURATION {
        // Client sends GET request
        let stream_id = match pair.client.send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), format!("/req/{total_requests}")),
            ],
            true,
        ) {
            Ok(sid) => sid,
            Err(_) => {
                // Stream limit reached or connection issue, skip
                continue;
            }
        };

        // Wait for server to see HEADERS
        let mut got_headers = false;
        let req_deadline = Instant::now() + Duration::from_secs(5);
        while !got_headers && Instant::now() < req_deadline {
            match pair.server_rx.recv_timeout(SHORT_RECV_TIMEOUT) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.event_type == EVENT_HEADERS
                            && event.stream_id == stream_id as i64
                        {
                            got_headers = true;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if !got_headers {
            continue;
        }

        // Server sends 200 response
        send_h3_server_cmd(
            &pair,
            WorkerCommand::SendResponseHeaders {
                conn_handle: server_conn,
                stream_id,
                headers: vec![(":status".into(), "200".into())],
                fin: false,
            },
        );
        send_h3_server_cmd(
            &pair,
            WorkerCommand::StreamSend {
                conn_handle: server_conn,
                stream_id,
                chunk: Chunk::unpooled(b"OK".to_vec()),
                fin: true,
            },
        );

        // Wait for client to receive the response
        let mut got_response = false;
        let resp_deadline = Instant::now() + Duration::from_secs(5);
        while !got_response && Instant::now() < resp_deadline {
            match pair.client_rx.recv_timeout(SHORT_RECV_TIMEOUT) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.stream_id == stream_id as i64
                            && (event.event_type == EVENT_FINISHED
                                || (event.event_type == EVENT_DATA
                                    && event.fin == Some(true)))
                        {
                            got_response = true;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if got_response {
            total_requests += 1;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "sustained H3 throughput: {total_requests} requests in {:.1}s",
        elapsed.as_secs_f64()
    );

    assert!(
        total_requests > 50,
        "expected > 50 completed H3 requests, got {total_requests}"
    );
}
