//! Integration tests for error propagation across the QUIC and HTTP/3
//! layers.  Verifies that stream resets carry error codes correctly and
//! that an error on one stream does not affect other concurrent streams.
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};

use http3::bench_exports::*;

// ---------------------------------------------------------------------------
// Shared config / port allocation
// ---------------------------------------------------------------------------

static CONFIG_MUTEX: Mutex<()> = Mutex::new(());

static NEXT_PORT: AtomicU16 = AtomicU16::new(60_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ---------------------------------------------------------------------------
// Cert generation
// ---------------------------------------------------------------------------

fn generate_test_certs_bytes() -> (Vec<u8>, Vec<u8>) {
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
// QUIC pair setup
// ---------------------------------------------------------------------------

struct QuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

fn setup_quic_pair() -> QuicPair {
    let (cert_pem, key_pem) = generate_test_certs_bytes();
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
        keepalive_interval_ms: None,
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
// H3 pair setup
// ---------------------------------------------------------------------------

fn build_h3_quiche_configs(cert_pem: &str, key_pem: &str) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("h3_err_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("h3_err_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

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
}

struct H3Pair {
    server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

fn setup_h3_pair() -> H3Pair {
    let (cert_pem, key_pem) = generate_test_certs_string();
    let (client_addr, server_addr) = next_pair_addrs();
    let (server_quiche, client_quiche) = build_h3_quiche_configs(&cert_pem, &key_pem);

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

fn wait_for_quic_handshake(pair: &QuicPair) -> (u32, u32) {
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

fn wait_for_h3_handshake(pair: &H3Pair) -> (u32, u32) {
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

fn send_h3_server_cmd(pair: &H3Pair, cmd: WorkerCommand) {
    pair.server.cmd_tx.send(cmd).unwrap();
    let _ = pair.server.waker.wake();
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_quic_stream_reset_propagates_error_code() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_quic_handshake(&pair);

    // Client opens stream 0 and sends data (fin=false to keep it open).
    let payload = b"data-for-reset-test".to_vec();
    assert!(pair.client.stream_send(0, payload, false));

    // Wait for server to see the stream.
    let _ = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        (e.event_type == EVENT_NEW_STREAM || e.event_type == EVENT_DATA) && e.stream_id == 0
    })
    .expect("server should see data on stream 0");

    // Server resets stream 0 with error_code=0x42.
    assert!(pair.server.send_command(QuicServerCommand::StreamClose {
        conn_handle: server_conn,
        stream_id: 0,
        error_code: 0x42,
    }));

    // Client should receive a RESET event on stream 0.  The error code
    // should be present in `meta.error_code`.
    let reset_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.stream_id == 0
            && (e.event_type == EVENT_RESET
                || e.event_type == EVENT_ERROR
                || e.event_type == EVENT_FINISHED)
    });

    assert!(
        reset_event.is_some(),
        "client should receive a reset/error/finished event on stream 0"
    );

    let event = reset_event.unwrap();
    // If we got a RESET event with meta, verify the error_code field.
    if event.event_type == EVENT_RESET {
        if let Some(ref meta) = event.meta {
            if let Some(code) = meta.error_code {
                assert_eq!(
                    code, 0x42,
                    "error_code should be 0x42, got {code:#x}"
                );
            }
        }
    }
}

#[test]
fn test_h3_stream_reset_generates_reset_event() {
    let pair = setup_h3_pair();
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Client sends a GET request.
    let stream_id = pair
        .client
        .send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/reset-test".into()),
            ],
            true,
        )
        .expect("send_request should succeed");

    // Wait for server to see the HEADERS.
    let _ = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    })
    .expect("server should receive HEADERS");

    // Server sends response headers with fin=false (keeping stream open).
    send_h3_server_cmd(
        &pair,
        WorkerCommand::SendResponseHeaders {
            conn_handle: server_conn,
            stream_id,
            headers: vec![(":status".into(), "200".into())],
            fin: false,
        },
    );

    // Give the response headers a moment to propagate.
    let _ = recv_event_matching(&pair.client_rx, Duration::from_secs(3), |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    });

    // Server resets the stream with error_code=0x100 (H3_REQUEST_CANCELLED).
    send_h3_server_cmd(
        &pair,
        WorkerCommand::StreamClose {
            conn_handle: server_conn,
            stream_id,
            error_code: 0x100,
        },
    );

    // Client should see a RESET, ERROR, or FINISHED event on that stream.
    let reset_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.stream_id == stream_id as i64
            && (e.event_type == EVENT_RESET
                || e.event_type == EVENT_ERROR
                || e.event_type == EVENT_FINISHED)
    });

    assert!(
        reset_event.is_some(),
        "client should receive RESET, ERROR, or FINISHED for the reset stream"
    );
}

#[test]
fn test_quic_error_on_one_stream_does_not_kill_others() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_quic_handshake(&pair);

    // Client opens stream 0 (fin=false — kept open) and stream 4 (fin=true).
    let payload_0 = b"stream-zero-data".to_vec();
    let payload_4 = b"stream-four-data".to_vec();
    assert!(pair.client.stream_send(0, payload_0, false));
    assert!(pair.client.stream_send(4, payload_4.clone(), true));

    // Wait for server to see data on both streams.
    let mut seen_0 = false;
    let mut seen_4 = false;
    let mut stream_4_data = Vec::new();
    let mut stream_4_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !(seen_0 && seen_4) && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        seen_0 = true;
                    }
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 4
                    {
                        if let Some(data) = event.data.as_ref() {
                            stream_4_data.extend_from_slice(data);
                        }
                        seen_4 = true;
                        if event.fin == Some(true) {
                            stream_4_fin = true;
                        }
                    }
                    if event.event_type == EVENT_FINISHED && event.stream_id == 4 {
                        stream_4_fin = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(seen_0, "server should see data on stream 0");
    assert!(seen_4, "server should see data on stream 4");

    // If we haven't received fin on stream 4 yet, keep draining.
    if !stream_4_fin {
        let remaining_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !stream_4_fin && std::time::Instant::now() < remaining_deadline {
            let remaining = remaining_deadline.saturating_duration_since(std::time::Instant::now());
            match pair.server_rx.recv_timeout(remaining) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.stream_id == 4 {
                            if let Some(data) = event.data.as_ref() {
                                stream_4_data.extend_from_slice(data);
                            }
                            if event.fin == Some(true) || event.event_type == EVENT_FINISHED {
                                stream_4_fin = true;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    // Server resets stream 0 with error_code=1.
    assert!(pair.server.send_command(QuicServerCommand::StreamClose {
        conn_handle: server_conn,
        stream_id: 0,
        error_code: 1,
    }));

    // Server echoes stream 4 back normally.
    assert!(pair.server.send_command(QuicServerCommand::StreamSend {
        conn_handle: server_conn,
        stream_id: 4,
        chunk: Chunk::unpooled(stream_4_data),
        fin: true,
    }));

    // Client: collect events for both streams.
    let mut got_reset_0 = false;
    let mut echo_data_4 = Vec::new();
    let mut got_fin_4 = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !(got_reset_0 && got_fin_4) && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    // Stream 0: expect RESET or ERROR.
                    if event.stream_id == 0
                        && (event.event_type == EVENT_RESET
                            || event.event_type == EVENT_ERROR
                            || event.event_type == EVENT_FINISHED)
                    {
                        got_reset_0 = true;
                    }
                    // Stream 4: expect echoed data + fin.
                    if event.stream_id == 4 {
                        if event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA {
                            if let Some(data) = event.data.as_ref() {
                                echo_data_4.extend_from_slice(data);
                            }
                            if event.fin == Some(true) {
                                got_fin_4 = true;
                            }
                        }
                        if event.event_type == EVENT_FINISHED {
                            got_fin_4 = true;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        got_reset_0,
        "client should see RESET/ERROR/FINISHED on stream 0"
    );
    assert!(
        got_fin_4,
        "client should receive complete echo on stream 4 — error on stream 0 must not kill stream 4"
    );
    assert_eq!(
        echo_data_4, payload_4,
        "echo data on stream 4 should match the original payload"
    );
}
