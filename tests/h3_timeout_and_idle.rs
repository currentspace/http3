//! Integration tests for HTTP/3 idle timeout behavior and session lifecycle
//! using `MockDriver`.  Each test spins up a mock H3 server+client pair over
//! in-memory channels and validates timeout, keep-alive, and idle-recovery
//! flows.
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

/// Config creation writes PEM bytes to temp files keyed only by PID, so
/// parallel tests within the same process can race.  Serialize behind this
/// mutex.
static CONFIG_MUTEX: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Port allocator — base 56_000 to avoid collision with quic_mock_pair (50k),
// h3_mock_pair (52k), and quic_timeout_and_idle (54k).
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(56_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ---------------------------------------------------------------------------
// Cert generation
// ---------------------------------------------------------------------------

fn generate_test_certs() -> (String, String) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair.serialize_pem())
}

// ---------------------------------------------------------------------------
// Build quiche configs for H3 (ALPN = h3) with configurable idle timeout
// ---------------------------------------------------------------------------

fn build_h3_quiche_configs_with_timeout(
    cert_pem: &str,
    key_pem: &str,
    idle_timeout_ms: u64,
) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("h3_timeout_test_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("h3_timeout_test_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

    // Server quiche config
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
    server_config.set_max_idle_timeout(idle_timeout_ms);
    server_config.set_max_recv_udp_payload_size(1472);
    server_config.set_max_send_udp_payload_size(1472);
    server_config.set_initial_max_data(100_000_000);
    server_config.set_initial_max_stream_data_bidi_local(2_000_000);
    server_config.set_initial_max_stream_data_bidi_remote(2_000_000);
    server_config.set_initial_max_stream_data_uni(2_000_000);
    server_config.set_initial_max_streams_bidi(10_000);
    server_config.set_initial_max_streams_uni(1_000);
    server_config.set_disable_active_migration(true);

    // Client quiche config
    let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    client_config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    client_config.verify_peer(false);
    client_config.set_max_idle_timeout(idle_timeout_ms);
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

// ---------------------------------------------------------------------------
// Setup helper — builds an H3 server+client pair with configurable idle
// timeout over MockDriver
// ---------------------------------------------------------------------------

struct H3Pair {
    server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_h3_pair_with_timeout(idle_timeout_ms: u64) -> H3Pair {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();
    let (server_quiche, client_quiche) =
        build_h3_quiche_configs_with_timeout(&cert_pem, &key_pem, idle_timeout_ms);

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
// Event collection helpers
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

/// Wait for the H3 handshake to complete.  Returns (server_conn_handle,
/// client_conn_handle).
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

/// Send a WorkerCommand to the server worker and wake it.
fn send_server_cmd(pair: &H3Pair, cmd: WorkerCommand) {
    pair.server.cmd_tx.send(cmd).unwrap();
    let _ = pair.server.waker.wake();
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_h3_idle_timeout_triggers_session_close() {
    // 500ms idle timeout — handshake, then do nothing.
    let pair = setup_h3_pair_with_timeout(500);
    let (_server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Do NOT drain events — just wait for SESSION_CLOSE to arrive.  The
    // worker thread's event loop will fire the quiche idle timer internally
    // and emit SESSION_CLOSE.  Use a generous timeout (3s) to give the
    // timer plenty of room beyond the 500ms idle period.
    let close_timeout = Duration::from_secs(3);
    let client_close = recv_event_matching(&pair.client_rx, close_timeout, |e| {
        e.event_type == EVENT_SESSION_CLOSE
    });
    let server_close = recv_event_matching(&pair.server_rx, close_timeout, |e| {
        e.event_type == EVENT_SESSION_CLOSE
    });

    assert!(
        client_close.is_some() || server_close.is_some(),
        "at least one side should see SESSION_CLOSE after H3 idle timeout"
    );
}

#[test]
fn test_h3_idle_timeout_reset_by_request() {
    // 2000ms idle timeout — sending a request at 400ms resets the idle
    // timer.  At 1.5s from handshake (1.1s from last activity), the
    // connection should still be alive (well within the 2s window).
    let pair = setup_h3_pair_with_timeout(2000);
    let (_server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Wait ~400ms, then send a GET request to reset the idle timer
    std::thread::sleep(Duration::from_millis(400));
    let stream_id = pair
        .client
        .send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/keepalive".into()),
            ],
            true,
        )
        .expect("send_request should succeed while connection is alive");

    // Server should see HEADERS for the request
    let got_headers = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    });
    assert!(
        got_headers.is_some(),
        "server should receive HEADERS for the keepalive request"
    );

    // At 1.5s from handshake (~1.1s from last activity), verify no
    // SESSION_CLOSE has arrived — well within the 2s idle timeout.
    std::thread::sleep(Duration::from_millis(1100));
    let premature_close = recv_event_matching(
        &pair.client_rx,
        Duration::from_millis(200),
        |e| e.event_type == EVENT_SESSION_CLOSE,
    );
    assert!(
        premature_close.is_none(),
        "H3 connection should NOT have timed out — request kept it alive"
    );
}

#[test]
fn test_h3_request_after_long_idle() {
    // 5000ms idle timeout — wait 2s idle, then send a request.
    // The connection should survive because 2s < 5s timeout.
    let pair = setup_h3_pair_with_timeout(5000);
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Wait 2s with no traffic
    std::thread::sleep(Duration::from_millis(2000));

    // Send a GET request — should still work
    let stream_id = pair
        .client
        .send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/after-idle".into()),
            ],
            true,
        )
        .expect("send_request should succeed after idle period within timeout");

    // Server should see the HEADERS
    let got_headers = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    });
    assert!(
        got_headers.is_some(),
        "server should receive HEADERS after 2s idle (within 5s timeout)"
    );

    // Server sends response headers + body
    send_server_cmd(
        &pair,
        WorkerCommand::SendResponseHeaders {
            conn_handle: server_conn,
            stream_id,
            headers: vec![(":status".into(), "200".into())],
            fin: false,
        },
    );
    send_server_cmd(
        &pair,
        WorkerCommand::StreamSend {
            conn_handle: server_conn,
            stream_id,
            chunk: Chunk::unpooled(b"survived-idle".to_vec()),
            fin: true,
        },
    );

    // Client should receive HEADERS, DATA, and FINISHED.  Collect all in
    // one pass to avoid dropping events that arrive in the same batch.
    let mut got_response_headers = false;
    let mut client_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.stream_id == stream_id as i64 {
                        if event.event_type == EVENT_HEADERS {
                            got_response_headers = true;
                        }
                        if event.event_type == EVENT_DATA {
                            if let Some(data) = event.data.as_ref() {
                                client_data.extend_from_slice(data);
                            }
                            if event.fin == Some(true) {
                                got_fin = true;
                            }
                        }
                        if event.event_type == EVENT_FINISHED {
                            got_fin = true;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        got_response_headers,
        "client should receive response HEADERS after idle period"
    );
    assert!(got_fin, "client should receive fin on response stream");
    assert_eq!(
        client_data,
        b"survived-idle",
        "response body should match after idle period"
    );
}
