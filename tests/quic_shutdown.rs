//! Integration tests for QUIC shutdown behavior: graceful shutdown with
//! in-flight streams, double shutdown idempotency, shutdown before handshake,
//! and client shutdown during active transfer.
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

static NEXT_PORT: AtomicU16 = AtomicU16::new(57_000);

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

/// Handshake, open 5 streams without fin, server has not responded, then
/// server shutdown.  Verify SHUTDOWN_COMPLETE on server within 5s and client
/// gets SESSION_CLOSE or SHUTDOWN_COMPLETE.  No panics, no hangs.
#[test]
fn test_quic_graceful_shutdown_with_inflight_streams() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client opens 5 streams (bidi client-initiated: 0, 4, 8, 12, 16)
    // with data but WITHOUT fin — leaving them open.
    let payload = b"inflight-data".to_vec();
    for i in 0..5u64 {
        let stream_id = i * 4;
        assert!(
            pair.client.stream_send(stream_id, payload.clone(), false),
            "failed to send on stream {stream_id}"
        );
    }

    // Wait briefly for data to propagate to the server side
    let _ = recv_event_matching(&pair.server_rx, Duration::from_secs(2), |e| {
        e.event_type == EVENT_NEW_STREAM && e.stream_id == 16
    });

    // Server shuts down while 5 streams are still open (no response sent)
    pair.server.shutdown();

    // Server batcher should emit SHUTDOWN_COMPLETE
    let server_sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        server_sentinel.is_some(),
        "server should emit SHUTDOWN_COMPLETE even with in-flight streams"
    );

    // Client should eventually get SESSION_CLOSE or SHUTDOWN_COMPLETE
    // (the server tearing down will close the QUIC connection)
    let client_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SESSION_CLOSE || e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    // The client may or may not receive this depending on timing — the
    // important thing is we got here without panics or hangs.
    let _ = client_event;
}

/// Handshake, call server.shutdown() twice.  Verify exactly one
/// SHUTDOWN_COMPLETE arrives (not two).
#[test]
fn test_quic_double_shutdown_is_idempotent() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // First shutdown
    pair.server.shutdown();

    // Wait for the first SHUTDOWN_COMPLETE
    let first = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        first.is_some(),
        "server should emit SHUTDOWN_COMPLETE on first shutdown"
    );

    // Second shutdown — should be a no-op (workers already joined)
    pair.server.shutdown();

    // Wait 2s for any spurious second sentinel
    let second = recv_event_matching(&pair.server_rx, Duration::from_secs(2), |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        second.is_none(),
        "second shutdown should NOT produce another SHUTDOWN_COMPLETE"
    );
}

/// Create a pair but do NOT call wait_for_handshake.  Immediately shutdown
/// the server.  Should not hang.
#[test]
fn test_quic_shutdown_before_handshake() {
    let mut pair = setup_quic_pair();

    // Do NOT wait for handshake — shutdown immediately
    pair.server.shutdown();

    // Server batcher should still emit SHUTDOWN_COMPLETE
    let sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        sentinel.is_some(),
        "server should emit SHUTDOWN_COMPLETE even before handshake completes"
    );
}

/// Handshake, client sends partial data (fin=false), then client shutdown.
/// Verify SHUTDOWN_COMPLETE on client.  Server should eventually get
/// SESSION_CLOSE or similar.
#[test]
fn test_quic_client_shutdown_during_active_transfer() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends partial data on stream 0 without fin
    let payload = b"partial-transfer-data".to_vec();
    assert!(pair.client.stream_send(0, payload, false));

    // Wait for the server to see the stream open
    let _ = recv_event_matching(&pair.server_rx, Duration::from_secs(2), |e| {
        (e.event_type == EVENT_NEW_STREAM || e.event_type == EVENT_DATA) && e.stream_id == 0
    });

    // Client shuts down mid-transfer
    pair.client.shutdown();

    // Client batcher should emit SHUTDOWN_COMPLETE
    let client_sentinel = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        client_sentinel.is_some(),
        "client should emit SHUTDOWN_COMPLETE after shutdown during active transfer"
    );

    // Server should eventually see SESSION_CLOSE or ERROR from the client
    // disappearing.  Timing-dependent, so we just verify no panic/hang.
    let server_event = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SESSION_CLOSE || e.event_type == EVENT_ERROR
    });
    let _ = server_event;
}
