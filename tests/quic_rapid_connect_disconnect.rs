//! Integration tests for rapid connect/disconnect cycles at the QUIC layer.
//! Verifies that the event loop correctly handles repeated session setup and
//! teardown without leaking resources or hanging.
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

static NEXT_PORT: AtomicU16 = AtomicU16::new(61_000);

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
fn test_quic_rapid_connect_disconnect_10_cycles() {
    for cycle in 0..10 {
        let mut pair = setup_quic_pair();
        let (_server_conn, _client_conn) = wait_for_handshake(&pair);

        // Send 1KB on stream 0.
        let payload = vec![0xCC_u8; 1024];
        assert!(
            pair.client.stream_send(0, payload.clone(), true),
            "cycle {cycle}: client stream_send failed"
        );

        // Wait for server to receive the data.
        let mut got_data = false;
        let deadline = std::time::Instant::now() + RECV_TIMEOUT;
        while !got_data && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match pair.server_rx.recv_timeout(remaining) {
                Ok(batch) => {
                    for event in batch.events {
                        if (event.event_type == EVENT_NEW_STREAM
                            || event.event_type == EVENT_DATA)
                            && event.stream_id == 0
                        {
                            got_data = true;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        assert!(got_data, "cycle {cycle}: server should receive data on stream 0");

        // Shut down both sides.
        pair.server.shutdown();
        pair.client.shutdown();

        // Verify SHUTDOWN_COMPLETE on server.
        let server_sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
            e.event_type == EVENT_SHUTDOWN_COMPLETE
        });
        assert!(
            server_sentinel.is_some(),
            "cycle {cycle}: server should emit SHUTDOWN_COMPLETE"
        );

        // Verify SHUTDOWN_COMPLETE on client.
        let client_sentinel = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
            e.event_type == EVENT_SHUTDOWN_COMPLETE
        });
        assert!(
            client_sentinel.is_some(),
            "cycle {cycle}: client should emit SHUTDOWN_COMPLETE"
        );
    }
}

#[test]
fn test_quic_connect_close_before_data() {
    let mut pair = setup_quic_pair();
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Immediately close the session without sending any stream data.
    assert!(pair.server.send_command(QuicServerCommand::CloseSession {
        conn_handle: _server_conn,
        error_code: 0,
        reason: "immediate close".to_string(),
    }));

    // Client should receive SESSION_CLOSE.
    let close_event = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SESSION_CLOSE
    });
    assert!(
        close_event.is_some(),
        "client should receive SESSION_CLOSE after immediate server close"
    );

    // Shut down both sides.
    pair.server.shutdown();
    pair.client.shutdown();

    // Verify SHUTDOWN_COMPLETE on server.
    let server_sentinel = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        server_sentinel.is_some(),
        "server should emit SHUTDOWN_COMPLETE"
    );

    // Verify SHUTDOWN_COMPLETE on client.
    let client_sentinel = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_SHUTDOWN_COMPLETE
    });
    assert!(
        client_sentinel.is_some(),
        "client should emit SHUTDOWN_COMPLETE"
    );
}
