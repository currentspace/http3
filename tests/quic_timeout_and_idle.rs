//! Integration tests for QUIC idle timeout behavior and session lifecycle
//! using `MockDriver`.  Each test spins up a mock server+client pair over
//! in-memory channels and validates timeout, keep-alive, and shutdown flows.
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
// Port allocator — base 54_000 to avoid collision with quic_mock_pair (50k)
// and h3_mock_pair (52k).
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(54_000);

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

// ---------------------------------------------------------------------------
// Setup helper — builds a QUIC server+client pair with configurable idle
// timeout over MockDriver
// ---------------------------------------------------------------------------

struct QuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_quic_pair_with_timeout(idle_timeout_ms: u32) -> QuicPair {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();

    let server_options = JsQuicServerOptions {
        key: key_pem.clone(),
        cert: cert_pem.clone(),
        ca: None,
        client_auth: None,
        alpn: None,
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(idle_timeout_ms),
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
        max_idle_timeout_ms: Some(idle_timeout_ms),
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

/// Wait for the handshake to complete.  Returns (server_conn_handle,
/// client_conn_handle).
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
fn test_quic_idle_timeout_triggers_session_close() {
    // 500ms idle timeout — handshake, then do nothing.
    let pair = setup_quic_pair_with_timeout(500);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

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
        "at least one side should see SESSION_CLOSE after idle timeout"
    );
}

#[test]
fn test_quic_idle_timeout_reset_by_data() {
    // 1000ms idle timeout — sending data at 400ms and 800ms should keep
    // the connection alive past the 1.5s mark.
    let pair = setup_quic_pair_with_timeout(1000);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Send data at ~400ms
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        pair.client.stream_send(0, b"keepalive-1".to_vec(), false),
        "first keepalive send should succeed"
    );

    // Send data at ~800ms
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        pair.client.stream_send(0, b"keepalive-2".to_vec(), false),
        "second keepalive send should succeed"
    );

    // At 1.5s from handshake, check that NO SESSION_CLOSE has arrived
    std::thread::sleep(Duration::from_millis(700));
    let premature_close = recv_event_matching(
        &pair.client_rx,
        Duration::from_millis(200),
        |e| e.event_type == EVENT_SESSION_CLOSE,
    );
    assert!(
        premature_close.is_none(),
        "connection should NOT have timed out — data kept it alive"
    );

    // Prove the connection is still usable by sending one more piece of data
    assert!(
        pair.client.stream_send(0, b"still-alive".to_vec(), true),
        "post-keepalive send should succeed, proving connection is alive"
    );

    // Server should receive data on stream 0
    let server_data = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        (e.event_type == EVENT_NEW_STREAM || e.event_type == EVENT_DATA) && e.stream_id == 0
    });
    assert!(
        server_data.is_some(),
        "server should still receive data on the alive connection"
    );
}

#[test]
fn test_quic_connection_close_and_cleanup() {
    // Normal 30s timeout — explicit close + shutdown verification.
    let mut pair = setup_quic_pair_with_timeout(30_000);
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends data on stream 0
    let payload = b"close-test-data".to_vec();
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Server receives the data
    let mut received = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !received && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        received = true;
                    }
                    if event.event_type == EVENT_FINISHED && event.stream_id == 0 {
                        received = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(received, "server should receive data on stream 0");

    // Server issues CloseSession
    assert!(
        pair.server.send_command(QuicServerCommand::CloseSession {
            conn_handle: server_conn,
            error_code: 0,
            reason: "test cleanup".to_string(),
        })
    );

    // Client should see SESSION_CLOSE
    let close_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SESSION_CLOSE
    });
    assert!(
        close_event.is_some(),
        "client should receive SESSION_CLOSE after server closes session"
    );

    // Shutdown both sides and verify SHUTDOWN_COMPLETE events
    pair.server.shutdown();
    let server_sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        server_sentinel.is_some(),
        "server should emit SHUTDOWN_COMPLETE"
    );

    pair.client.shutdown();
    let client_sentinel = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        client_sentinel.is_some(),
        "client should emit SHUTDOWN_COMPLETE"
    );
}
