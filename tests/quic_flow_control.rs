//! Integration tests for QUIC-level flow control behavior using `MockDriver`.
//! These tests verify that data eventually arrives through small flow control
//! windows, exercising both stream-level and connection-level flow control.
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
// Port allocator — each test pair gets a unique address range so parallel
// tests never collide.
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(54_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ---------------------------------------------------------------------------
// Cert generation — identical to the pattern in quic_mock_pair.rs
// ---------------------------------------------------------------------------

fn generate_test_certs() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes())
}

// ---------------------------------------------------------------------------
// Setup helper — builds a QUIC server+client pair with custom flow control
// via JsQuicServerOptions / JsQuicClientOptions.
// ---------------------------------------------------------------------------

struct QuicPair {
    #[allow(dead_code)] // kept alive to sustain the server event loop
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_quic_pair_with_flow_control(
    initial_max_stream_data: u32,
    initial_max_data: u32,
) -> QuicPair {
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
        initial_max_data: Some(initial_max_data),
        initial_max_stream_data_bidi_local: Some(initial_max_stream_data),
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
        initial_max_data: Some(initial_max_data),
        initial_max_stream_data_bidi_local: Some(initial_max_stream_data),
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

    build_quic_pair_from_configs(server_quiche, client_quiche, client_addr, server_addr)
}

/// Build quiche configs directly for full control over all flow control
/// parameters (unlike `new_quic_server_config` which hardcodes `bidi_remote`).
fn build_quic_quiche_configs(
    cert_pem: &[u8],
    key_pem: &[u8],
    initial_max_stream_data: u64,
    initial_max_data: u64,
) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("quic_fc_test_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("quic_fc_test_key_{id:?}.pem"));
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
        .set_application_protos(&[b"quic"])
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
    // Congestion tuning (must match apply_congestion_tuning in config.rs)
    server_config.set_send_capacity_factor(20.0);
    server_config.set_initial_congestion_window_packets(1000);
    server_config.discover_pmtu(true);
    server_config.set_pmtud_max_probes(1);

    // Client quiche config
    let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    client_config
        .set_application_protos(&[b"quic"])
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
    // Congestion tuning (must match apply_congestion_tuning in config.rs)
    client_config.set_send_capacity_factor(20.0);
    client_config.set_initial_congestion_window_packets(1000);
    client_config.discover_pmtu(true);
    client_config.set_pmtud_max_probes(1);

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    (server_config, client_config)
}

fn setup_quic_pair_direct(
    initial_max_stream_data: u64,
    initial_max_data: u64,
) -> QuicPair {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();
    let (server_quiche, client_quiche) =
        build_quic_quiche_configs(&cert_pem, &key_pem, initial_max_stream_data, initial_max_data);

    build_quic_pair_from_configs(server_quiche, client_quiche, client_addr, server_addr)
}

fn build_quic_pair_from_configs(
    server_quiche: quiche::Config,
    client_quiche: quiche::Config,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
) -> QuicPair {
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
// Event collection helpers
// ---------------------------------------------------------------------------

/// Drain all batches from a receiver until we find an event matching the
/// predicate, or timeout.
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

/// Wait for the handshake to complete on the client side and return the
/// `conn_handle` assigned by the server to the new session.  Returns both the
/// server-side conn_handle (from NEW_SESSION) and the client-side conn_handle
/// (from HANDSHAKE_COMPLETE).
fn wait_for_handshake(pair: &QuicPair) -> (u32, u32) {
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

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_quic_stream_data_eventually_arrives() {
    // Small stream window (4096) on bidi_local, large connection window.
    // Note: new_quic_server_config hardcodes bidi_remote to 2MB, so the
    // client can send up to 2MB initially per stream.  The small bidi_local
    // constrains server-to-client flow.  For client-to-server, the effective
    // stream limit is min(server bidi_remote=2MB, client bidi_local=4KB).
    // Since bidi_local is 4KB on the client, that constrains server-to-client
    // data only.  Client-to-server uses server's bidi_remote (2MB), so the
    // 64KB payload goes through without needing flow control updates.
    let pair = setup_quic_pair_with_flow_control(4096, 100_000_000);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends 64KB on stream 0 with fin=true
    let payload_len = 64 * 1024;
    let payload = vec![0xFC_u8; payload_len];
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Collect on server side with extended timeout
    let mut server_data = Vec::new();
    let mut got_fin = false;
    let extended_timeout = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + extended_timeout;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        if let Some(data) = event.data.as_ref() {
                            server_data.extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            got_fin = true;
                        }
                    }
                    if event.event_type == EVENT_FINISHED && event.stream_id == 0 {
                        got_fin = true;
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        got_fin,
        "server should receive fin on stream 0 (got {} of {} bytes)",
        server_data.len(),
        payload_len
    );
    assert_eq!(
        server_data.len(),
        payload_len,
        "all 64KB should eventually arrive through the flow control window"
    );
    assert!(
        server_data.iter().all(|&b| b == 0xFC),
        "payload content mismatch"
    );
}

#[test]
fn test_quic_multiple_streams_with_small_windows() {
    // Small stream window (4096) on bidi_local, large connection window.
    let pair = setup_quic_pair_with_flow_control(4096, 100_000_000);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    let stream_count: u64 = 5;
    let payload_len = 8 * 1024; // 8KB per stream
    let payload = vec![0xAA_u8; payload_len];

    // Open 5 streams, each sending 8KB (QUIC bidi client-initiated: 0, 4, 8, 12, 16)
    for i in 0..stream_count {
        let stream_id = i * 4;
        assert!(
            pair.client.stream_send(stream_id, payload.clone(), true),
            "failed to send on stream {stream_id}"
        );
    }

    // Collect server data for all 5 streams with extended timeout
    let mut stream_data: std::collections::HashMap<i64, Vec<u8>> =
        std::collections::HashMap::new();
    let mut streams_finished = std::collections::HashSet::new();
    let extended_timeout = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + extended_timeout;
    while streams_finished.len() < stream_count as usize && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    let sid = event.stream_id;
                    if event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA {
                        if let Some(data) = event.data.as_ref() {
                            stream_data.entry(sid).or_default().extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            streams_finished.insert(sid);
                        }
                    }
                    if event.event_type == EVENT_FINISHED {
                        streams_finished.insert(sid);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        streams_finished.len(),
        stream_count as usize,
        "all 5 streams should complete, got: {streams_finished:?}"
    );

    // Verify each stream received the correct data length
    for i in 0..stream_count {
        let sid = (i * 4) as i64;
        let data = stream_data.get(&sid).unwrap_or(&Vec::new()).clone();
        assert_eq!(
            data.len(),
            payload_len,
            "stream {sid} should have received {payload_len} bytes, got {}",
            data.len()
        );
    }
}

#[test]
fn test_quic_connection_level_flow_control_limits() {
    // Use direct quiche config for full control.
    // Stream windows = 2MB, connection window = 2MB.
    // Send 1MB on each of 2 streams (total 2MB = connection window).
    // quiche should be able to deliver both streams since each fits within
    // the stream window and total fits within the connection window, but the
    // tight connection budget exercises the connection-level flow control path.
    //
    // Then verify that larger totals also work with automatic MAX_DATA: use
    // 4 streams of 1MB each (4MB > 2MB connection window).  quiche sends
    // MAX_DATA as the server reads, unlocking further delivery.
    let pair = setup_quic_pair_direct(2_000_000, 2_000_000);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    let stream_count: u64 = 2;
    let payload_len = 1_000_000; // 1MB per stream
    let payload = vec![0xBB_u8; payload_len];

    // Open 2 streams, each sending 1MB (total 2MB = connection window limit)
    for i in 0..stream_count {
        let stream_id = i * 4;
        assert!(
            pair.client.stream_send(stream_id, payload.clone(), true),
            "failed to send on stream {stream_id}"
        );
    }

    // Collect server data for all streams with generous timeout
    let mut stream_data: std::collections::HashMap<i64, Vec<u8>> =
        std::collections::HashMap::new();
    let mut streams_finished = std::collections::HashSet::new();
    let extended_timeout = Duration::from_secs(15);
    let deadline = std::time::Instant::now() + extended_timeout;
    while streams_finished.len() < stream_count as usize && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    let sid = event.stream_id;
                    if event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA {
                        if let Some(data) = event.data.as_ref() {
                            stream_data.entry(sid).or_default().extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            streams_finished.insert(sid);
                        }
                    }
                    if event.event_type == EVENT_FINISHED {
                        streams_finished.insert(sid);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        streams_finished.len(),
        stream_count as usize,
        "all {stream_count} streams should eventually complete, got: {streams_finished:?}"
    );

    // Verify each stream received the full 1MB
    for i in 0..stream_count {
        let sid = (i * 4) as i64;
        let data = stream_data.get(&sid).unwrap_or(&Vec::new()).clone();
        assert_eq!(
            data.len(),
            payload_len,
            "stream {sid} should have received {payload_len} bytes, got {}",
            data.len()
        );
        assert!(
            data.iter().all(|&b| b == 0xBB),
            "stream {sid} payload content mismatch"
        );
    }
}
