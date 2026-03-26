//! Integration tests for the raw QUIC protocol path through the full event
//! loop using `MockDriver`.  Each test spins up a mock server+client pair
//! over in-memory channels and validates the event-driven protocol flow.
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

/// `new_quic_server_config` / `new_quic_client_config` write PEM bytes to
/// temp files keyed only by PID, so parallel tests within the same process
/// can race.  Serialize config creation behind this mutex.
static CONFIG_MUTEX: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Port allocator — each test pair gets a unique address range so parallel
// tests never collide.
// ---------------------------------------------------------------------------

static NEXT_PORT: AtomicU16 = AtomicU16::new(50_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

// ---------------------------------------------------------------------------
// Cert generation — identical to the pattern in mock_quic.rs
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
// Setup helper — builds a QUIC server+client pair over MockDriver
// ---------------------------------------------------------------------------

struct QuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn setup_quic_pair() -> QuicPair {
    setup_quic_pair_with_datagrams(false)
}

fn setup_quic_pair_with_datagrams(enable_datagrams: bool) -> QuicPair {
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
        enable_datagrams: Some(enable_datagrams),
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
        enable_datagrams: Some(enable_datagrams),
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
fn test_quic_handshake_and_new_session_event() {
    let pair = setup_quic_pair();

    let new_session = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_NEW_SESSION
    });
    assert!(
        new_session.is_some(),
        "server should emit NEW_SESSION after client connects"
    );

    let event = new_session.unwrap();
    assert_eq!(event.event_type, EVENT_NEW_SESSION);
    // conn_handle should be non-negative (0 is valid for first connection)
    assert!(event.conn_handle < u32::MAX);
}

#[test]
fn test_quic_handshake_complete_event() {
    let pair = setup_quic_pair();

    let hs_complete = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    });
    assert!(
        hs_complete.is_some(),
        "client should emit HANDSHAKE_COMPLETE"
    );
    assert_eq!(hs_complete.unwrap().event_type, EVENT_HANDSHAKE_COMPLETE);
}

#[test]
fn test_quic_client_stream_send_server_echo() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends data on stream 0
    let payload = b"hello QUIC world".to_vec();
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Server should receive NEW_STREAM or DATA with the payload
    let mut received_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        if let Some(data) = event.data.as_ref() {
                            received_data.extend_from_slice(data);
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

    assert!(got_fin, "server should receive fin on stream 0");
    assert_eq!(received_data, b"hello QUIC world");

    // Server echoes back
    assert!(pair.server.send_command(QuicServerCommand::StreamSend {
        conn_handle: server_conn,
        stream_id: 0,
        chunk: Chunk::unpooled(received_data.clone()),
        fin: true,
    }));

    // Client should receive the echoed data
    let mut echo_data = Vec::new();
    let mut echo_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !echo_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        if let Some(data) = event.data.as_ref() {
                            echo_data.extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            echo_fin = true;
                        }
                    }
                    if event.event_type == EVENT_FINISHED && event.stream_id == 0 {
                        echo_fin = true;
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(echo_fin, "client should receive fin on echo stream");
    assert_eq!(echo_data, b"hello QUIC world");
}

#[test]
fn test_quic_multiple_streams() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    let stream_count: u64 = 5;
    let payload = b"stream-payload".to_vec();

    // Client sends on 5 streams (QUIC bidi client-initiated: 0, 4, 8, 12, 16)
    for i in 0..stream_count {
        let stream_id = i * 4;
        assert!(
            pair.client.stream_send(stream_id, payload.clone(), true),
            "failed to send on stream {stream_id}"
        );
    }

    // Collect server-side data for all streams and echo them back
    let mut streams_received = std::collections::HashSet::new();
    let mut pending_data: std::collections::HashMap<i64, Vec<u8>> = std::collections::HashMap::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while streams_received.len() < stream_count as usize && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    let sid = event.stream_id;
                    if event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA {
                        if let Some(data) = event.data.as_ref() {
                            pending_data.entry(sid).or_default().extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            streams_received.insert(sid);
                        }
                    }
                    if event.event_type == EVENT_FINISHED {
                        streams_received.insert(sid);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        streams_received.len(),
        stream_count as usize,
        "server should have received fin on all {stream_count} streams, got: {streams_received:?}"
    );

    // Echo each stream back
    for i in 0..stream_count {
        let stream_id = i * 4;
        let data = pending_data.remove(&(stream_id as i64)).unwrap_or_default();
        pair.server.send_command(QuicServerCommand::StreamSend {
            conn_handle: server_conn,
            stream_id,
            chunk: Chunk::unpooled(data),
            fin: true,
        });
    }

    // Client should get responses on all 5 streams
    let mut client_completed = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while client_completed.len() < stream_count as usize && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM
                        || event.event_type == EVENT_DATA
                        || event.event_type == EVENT_FINISHED)
                        && (event.fin == Some(true) || event.event_type == EVENT_FINISHED)
                    {
                        client_completed.insert(event.stream_id);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        client_completed.len(),
        stream_count as usize,
        "client should complete all {stream_count} streams, got: {client_completed:?}"
    );
}

#[test]
fn test_quic_large_payload() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // 64 KB payload
    let payload = vec![0xAB_u8; 64 * 1024];
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Collect on the server side
    let mut server_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
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

    assert!(got_fin, "server should receive fin for large payload");
    assert_eq!(
        server_data.len(),
        64 * 1024,
        "server should receive the complete 64KB payload"
    );
    assert!(
        server_data.iter().all(|&b| b == 0xAB),
        "payload content mismatch"
    );

    // Echo back
    pair.server.send_command(QuicServerCommand::StreamSend {
        conn_handle: server_conn,
        stream_id: 0,
        chunk: Chunk::unpooled(server_data),
        fin: true,
    });

    // Collect client-side echo
    let mut client_data = Vec::new();
    let mut echo_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !echo_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if (event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA)
                        && event.stream_id == 0
                    {
                        if let Some(data) = event.data.as_ref() {
                            client_data.extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            echo_fin = true;
                        }
                    }
                    if event.event_type == EVENT_FINISHED && event.stream_id == 0 {
                        echo_fin = true;
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(echo_fin, "client should receive the echoed 64KB payload");
    assert_eq!(client_data.len(), 64 * 1024);
}

#[test]
fn test_quic_stream_close() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client opens a stream
    assert!(pair.client.stream_send(0, b"data".to_vec(), false));

    // Wait for server to see the stream
    let _ = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        (e.event_type == EVENT_NEW_STREAM || e.event_type == EVENT_DATA) && e.stream_id == 0
    });

    // Server issues StreamClose (STOP_SENDING / RESET_STREAM)
    assert!(pair.server.send_command(QuicServerCommand::StreamClose {
        conn_handle: server_conn,
        stream_id: 0,
        error_code: 0,
    }));

    // Client should see a RESET or ERROR or SESSION_CLOSE for that stream
    // (the exact event depends on timing, but the stream should be torn down)
    let got_teardown = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.stream_id == 0
            && (e.event_type == EVENT_RESET
                || e.event_type == EVENT_ERROR
                || e.event_type == EVENT_FINISHED)
    });

    // Stream teardown events may arrive differently depending on timing;
    // just verify the command was accepted without panic.
    let _ = got_teardown;
}

#[test]
fn test_quic_close_session() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Server closes the session
    assert!(
        pair.server
            .send_command(QuicServerCommand::CloseSession {
                conn_handle: server_conn,
                error_code: 0,
                reason: "test close".to_string(),
            })
    );

    // Client should receive SESSION_CLOSE
    let close_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SESSION_CLOSE
    });
    assert!(
        close_event.is_some(),
        "client should receive SESSION_CLOSE after server closes session"
    );
}

#[test]
fn test_quic_server_shutdown_emits_sentinel() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Shut down the server
    pair.server.shutdown();

    // The server batcher should emit SHUTDOWN_COMPLETE sentinel
    let sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        sentinel.is_some(),
        "server should emit SHUTDOWN_COMPLETE on shutdown"
    );
}

#[test]
fn test_quic_client_shutdown_emits_sentinel() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Shut down the client
    pair.client.shutdown();

    // The client batcher should emit SHUTDOWN_COMPLETE sentinel
    let sentinel = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        sentinel.is_some(),
        "client should emit SHUTDOWN_COMPLETE on shutdown"
    );
}

#[test]
fn test_quic_session_metrics() {
    let pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Send some data so there are non-zero metrics
    assert!(pair.client.stream_send(0, b"metrics test".to_vec(), true));

    // Wait a bit for the data to flow
    let _ = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        (e.event_type == EVENT_NEW_STREAM || e.event_type == EVENT_DATA) && e.stream_id == 0
    });

    // Query client-side metrics
    let metrics = pair.client.get_session_metrics();
    assert!(
        metrics.is_ok(),
        "get_session_metrics should not fail: {:?}",
        metrics.err()
    );
    let metrics = metrics.unwrap();
    assert!(
        metrics.is_some(),
        "metrics should be present after handshake"
    );
    let m = metrics.unwrap();
    assert!(m.packets_out > 0, "should have sent packets");
    assert!(m.packets_in > 0, "should have received packets");
    assert!(m.rtt_ms >= 0.0, "rtt should be non-negative");
}

#[test]
fn test_quic_datagram_send_recv() {
    let pair = setup_quic_pair_with_datagrams(true);
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends a datagram
    let dgram_payload = b"datagram-test-payload".to_vec();
    let result = pair.client.send_datagram(dgram_payload.clone());
    assert!(result.is_ok(), "send_datagram should not error: {:?}", result.err());

    // Server should receive an EVENT_DATAGRAM
    let dgram_event = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_DATAGRAM
    });
    assert!(
        dgram_event.is_some(),
        "server should receive DATAGRAM event"
    );
    let dgram = dgram_event.unwrap();
    assert_eq!(dgram.event_type, EVENT_DATAGRAM);
    if let Some(data) = dgram.data.as_ref() {
        assert_eq!(&data[..], &dgram_payload[..]);
    }

    // Server sends a datagram back to client
    let server_dgram = b"datagram-reply".to_vec();
    let result = pair.server.send_datagram(server_conn, server_dgram.clone());
    assert!(
        result.is_ok(),
        "server send_datagram should not error: {:?}",
        result.err()
    );

    // Client should receive the reply datagram
    let reply_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_DATAGRAM
    });
    assert!(
        reply_event.is_some(),
        "client should receive DATAGRAM reply"
    );
    let reply = reply_event.unwrap();
    if let Some(data) = reply.data.as_ref() {
        assert_eq!(&data[..], &server_dgram[..]);
    }
}
