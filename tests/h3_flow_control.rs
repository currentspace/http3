//! Integration tests for HTTP/3 flow control behavior using `MockDriver`.
//! These tests verify that large request/response bodies eventually arrive
//! through small flow control windows at the H3 layer.
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
// Port allocator
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(55_000);

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
// Build quiche configs for H3 with custom flow control
// ---------------------------------------------------------------------------

fn build_h3_quiche_configs_with_flow_control(
    cert_pem: &str,
    key_pem: &str,
    initial_max_stream_data: u64,
    initial_max_data: u64,
) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("h3_fc_test_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("h3_fc_test_key_{id:?}.pem"));
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
    server_config.set_max_idle_timeout(30_000);
    server_config.set_max_recv_udp_payload_size(1472);
    server_config.set_max_send_udp_payload_size(1472);
    server_config.set_initial_max_data(initial_max_data);
    server_config.set_initial_max_stream_data_bidi_local(initial_max_stream_data);
    server_config.set_initial_max_stream_data_bidi_remote(initial_max_stream_data);
    server_config.set_initial_max_stream_data_uni(initial_max_stream_data);
    server_config.set_initial_max_streams_bidi(10_000);
    server_config.set_initial_max_streams_uni(1_000);
    server_config.set_disable_active_migration(true);

    // Client quiche config
    let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    client_config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    client_config.verify_peer(false);
    client_config.set_max_idle_timeout(30_000);
    client_config.set_max_recv_udp_payload_size(1472);
    client_config.set_max_send_udp_payload_size(1472);
    client_config.set_initial_max_data(initial_max_data);
    client_config.set_initial_max_stream_data_bidi_local(initial_max_stream_data);
    client_config.set_initial_max_stream_data_bidi_remote(initial_max_stream_data);
    client_config.set_initial_max_stream_data_uni(initial_max_stream_data);
    client_config.set_initial_max_streams_bidi(10_000);
    client_config.set_initial_max_streams_uni(1_000);

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    (server_config, client_config)
}

// ---------------------------------------------------------------------------
// Setup helper — builds an H3 server+client pair with custom flow control
// ---------------------------------------------------------------------------

struct H3Pair {
    server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_h3_pair_with_flow_control(
    initial_max_stream_data: u64,
    initial_max_data: u64,
) -> H3Pair {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();
    let (server_quiche, client_quiche) =
        build_h3_quiche_configs_with_flow_control(&cert_pem, &key_pem, initial_max_stream_data, initial_max_data);

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
fn test_h3_large_response_with_small_window() {
    // Small stream window (4096). Server sends a 32KB response body.
    // Flow control should throttle delivery but all data should arrive.
    let pair = setup_h3_pair_with_flow_control(4096, 100_000_000);
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Client sends GET
    let stream_id = pair
        .client
        .send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/large-response".into()),
            ],
            true,
        )
        .expect("send_request should succeed");

    // Wait for server to see the request
    let _ = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    })
    .expect("server should receive HEADERS");

    // Server sends 200 + 32KB body
    let body_len = 32 * 1024;
    let body = vec![0xCD_u8; body_len];
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
            data: body.clone(),
            fin: true,
        },
    );

    // Client collects all data events for that stream with extended timeout
    let mut got_headers = false;
    let mut client_data = Vec::new();
    let mut got_fin = false;
    let extended_timeout = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + extended_timeout;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.stream_id == stream_id as i64 {
                        if event.event_type == EVENT_HEADERS {
                            got_headers = true;
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

    assert!(got_headers, "client should receive response HEADERS");
    assert!(
        got_fin,
        "client should receive fin on response stream (got {} of {} bytes)",
        client_data.len(),
        body_len
    );
    assert_eq!(
        client_data.len(),
        body_len,
        "full 32KB response body should arrive through the small flow control window (got {} bytes)",
        client_data.len()
    );
    assert!(
        client_data.iter().all(|&b| b == 0xCD),
        "response body content mismatch"
    );
}

#[test]
fn test_h3_large_post_with_small_window() {
    // Small stream window (4096). Client sends POST with 32KB body.
    // Flow control should throttle delivery but all data should arrive.
    let pair = setup_h3_pair_with_flow_control(4096, 100_000_000);
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    // Client sends POST with body (fin=false for headers, then send body)
    let stream_id = pair
        .client
        .send_request(
            vec![
                (":method".into(), "POST".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/upload".into()),
            ],
            false,
        )
        .expect("send_request should succeed");

    // Client sends the 32KB POST body
    let body_len = 32 * 1024;
    let post_body = vec![0xEF_u8; body_len];
    assert!(pair.client.stream_send(stream_id, post_body.clone(), true));

    // Collect all server events for this stream: HEADERS, DATA, FINISHED
    let mut got_headers = false;
    let mut server_data = Vec::new();
    let mut got_fin = false;
    let extended_timeout = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + extended_timeout;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.stream_id == stream_id as i64 {
                        if event.event_type == EVENT_HEADERS {
                            got_headers = true;
                        }
                        if event.event_type == EVENT_DATA {
                            if let Some(data) = event.data.as_ref() {
                                server_data.extend_from_slice(data);
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

    assert!(got_headers, "server should receive HEADERS for POST");
    assert!(
        got_fin,
        "server should receive fin on POST stream (got {} of {} bytes)",
        server_data.len(),
        body_len
    );
    assert_eq!(
        server_data.len(),
        body_len,
        "full 32KB POST body should arrive through the small flow control window (got {} bytes)",
        server_data.len()
    );
    assert!(
        server_data.iter().all(|&b| b == 0xEF),
        "POST body content mismatch"
    );

    // Server sends back a 200 response to complete the exchange
    send_server_cmd(
        &pair,
        WorkerCommand::SendResponseHeaders {
            conn_handle: server_conn,
            stream_id,
            headers: vec![(":status".into(), "200".into())],
            fin: true,
        },
    );

    let client_headers = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS && e.stream_id == stream_id as i64
    })
    .expect("client should receive response headers for POST");
    let resp_hdrs = client_headers.headers.as_ref().unwrap();
    let status = resp_hdrs.iter().find(|h| h.name == ":status").unwrap();
    assert_eq!(status.value, "200");
}
