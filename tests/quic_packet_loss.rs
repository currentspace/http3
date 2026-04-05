//! Packet loss simulation tests for QUIC retransmission.
//! Verifies that QUIC recovers data integrity under simulated packet loss
//! at the mock transport layer.
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

static CONFIG_MUTEX: Mutex<()> = Mutex::new(());
static NEXT_PORT: AtomicU16 = AtomicU16::new(56_000);

fn next_pair_addrs() -> (SocketAddr, SocketAddr) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base);
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base + 1);
    (client, server)
}

fn generate_test_certs() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes())
}

struct QuicPairWithLoss {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
    loss: PacketLossConfig,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(15);

fn setup_quic_pair_with_loss(drop_pct: u32) -> QuicPairWithLoss {
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

    let loss = PacketLossConfig::new(drop_pct);
    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair_with_loss(client_addr, server_addr, loss.clone());

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _) = channel_batcher("server", server_event_tx);
    let (client_batcher, _) = channel_batcher("client", client_event_tx);

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

    QuicPairWithLoss {
        server,
        client,
        server_rx: server_event_rx,
        client_rx: client_event_rx,
        loss,
    }
}

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

fn wait_for_handshake(pair: &QuicPairWithLoss) -> (u32, u32) {
    let server_new = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server should receive NEW_SESSION under packet loss");

    let client_hs = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client should receive HANDSHAKE_COMPLETE under packet loss");

    (server_new.conn_handle, client_hs.conn_handle)
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_quic_handshake_completes_under_5pct_loss() {
    let pair = setup_quic_pair_with_loss(5);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);
    let (sent, dropped) = pair.loss.stats();
    eprintln!("  5% loss: handshake ok, {sent} sent, {dropped} dropped");
    assert!(dropped > 0 || sent > 10, "expected some traffic: sent={sent}");
}

#[test]
fn test_quic_handshake_completes_under_10pct_loss() {
    let pair = setup_quic_pair_with_loss(10);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);
    let (sent, dropped) = pair.loss.stats();
    eprintln!("  10% loss: handshake ok, {sent} sent, {dropped} dropped");
}

#[test]
fn test_quic_handshake_completes_under_20pct_loss() {
    let pair = setup_quic_pair_with_loss(20);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);
    let (sent, dropped) = pair.loss.stats();
    eprintln!("  20% loss: handshake ok, {sent} sent, {dropped} dropped");
}

#[test]
fn test_quic_stream_echo_under_5pct_loss() {
    let pair = setup_quic_pair_with_loss(5);
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends 4KB on stream 0
    let payload = vec![0xAA_u8; 4096];
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Server collects data
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

    assert!(got_fin, "server should receive fin despite 5% packet loss");
    assert_eq!(
        received_data.len(),
        payload.len(),
        "server should receive all 4KB despite 5% loss"
    );
    assert!(
        received_data.iter().all(|&b| b == 0xAA),
        "data integrity check"
    );

    // Server echoes back
    assert!(pair.server.send_command(QuicServerCommand::StreamSend {
        conn_handle: server_conn,
        stream_id: 0,
        chunk: Chunk::unpooled(received_data),
        fin: true,
    }));

    // Client collects echo
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

    assert!(echo_fin, "client should receive echo fin despite 5% loss");
    assert_eq!(echo_data.len(), 4096, "echo should be complete 4KB");

    let (sent, dropped) = pair.loss.stats();
    eprintln!("  5% loss echo: {sent} sent, {dropped} dropped ({:.1}%)",
        (dropped as f64 / sent as f64) * 100.0);
}

#[test]
fn test_quic_large_transfer_under_10pct_loss() {
    let pair = setup_quic_pair_with_loss(10);
    let (server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends 64KB
    let payload = vec![0xBB_u8; 64 * 1024];
    assert!(pair.client.stream_send(0, payload.clone(), true));

    // Server collects — with 10% loss QUIC must retransmit
    let mut received_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
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

    assert!(
        got_fin,
        "server should receive fin despite 10% packet loss (got {} bytes)",
        received_data.len()
    );
    assert_eq!(
        received_data.len(),
        64 * 1024,
        "QUIC retransmission should deliver all 64KB under 10% loss"
    );

    let (sent, dropped) = pair.loss.stats();
    eprintln!(
        "  10% loss 64KB: {sent} sent, {dropped} dropped ({:.1}%), retransmissions recovered all data",
        (dropped as f64 / sent as f64) * 100.0
    );
}

#[test]
fn test_quic_multiple_streams_under_10pct_loss() {
    let pair = setup_quic_pair_with_loss(10);
    let (_server_conn, _client_conn) = wait_for_handshake(&pair);

    // Client sends on 5 streams
    let payload = b"stream-payload-under-loss".to_vec();
    for i in 0..5u64 {
        let stream_id = i * 4;
        assert!(
            pair.client.stream_send(stream_id, payload.clone(), true),
            "failed to send on stream {stream_id}"
        );
    }

    // Collect server-side FINs
    let mut streams_completed = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while streams_completed.len() < 5 && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.fin == Some(true) || event.event_type == EVENT_FINISHED {
                        streams_completed.insert(event.stream_id);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        streams_completed.len(),
        5,
        "all 5 streams should complete under 10% loss, got: {streams_completed:?}"
    );

    let (sent, dropped) = pair.loss.stats();
    eprintln!(
        "  10% loss 5 streams: {sent} sent, {dropped} dropped ({:.1}%)",
        (dropped as f64 / sent as f64) * 100.0
    );
}
