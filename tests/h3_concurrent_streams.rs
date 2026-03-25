//! Integration tests for concurrent stream handling at both the HTTP/3
//! and raw QUIC layers.  Verifies that the event loop correctly multiplexes
//! many simultaneous requests without dropping data or deadlocking.
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::collections::{HashMap, HashSet};
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

static NEXT_PORT: AtomicU16 = AtomicU16::new(59_000);

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
// H3 quiche config builder
// ---------------------------------------------------------------------------

fn build_h3_quiche_configs(cert_pem: &str, key_pem: &str) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("h3_conc_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("h3_conc_key_{id:?}.pem"));
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

// ---------------------------------------------------------------------------
// H3 pair setup
// ---------------------------------------------------------------------------

struct H3Pair {
    server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(30);

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
// QUIC pair setup
// ---------------------------------------------------------------------------

struct QuicPair {
    server: QuicServerHandle,
    client: QuicClientHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
}

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

fn send_server_cmd(pair: &H3Pair, cmd: WorkerCommand) {
    pair.server.cmd_tx.send(cmd).unwrap();
    let _ = pair.server.waker.wake();
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_h3_100_concurrent_get_requests() {
    let pair = setup_h3_pair();
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    let request_count: usize = 100;
    let mut stream_ids = Vec::with_capacity(request_count);

    // Client sends 100 GET requests, each with fin=true (no body).
    for i in 0..request_count {
        let stream_id = pair
            .client
            .send_request(
                vec![
                    (":method".into(), "GET".into()),
                    (":scheme".into(), "https".into()),
                    (":authority".into(), "localhost".into()),
                    (":path".into(), format!("/req/{i}")),
                ],
                true,
            )
            .expect("send_request should succeed");
        stream_ids.push(stream_id);
    }

    // Server: process incoming HEADERS events and auto-respond to each
    // with 200 + a small body.  We interleave reading server events and
    // sending responses so the event loop stays alive.
    let mut responded_streams = HashSet::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while responded_streams.len() < request_count && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.server_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.event_type == EVENT_HEADERS
                        && !responded_streams.contains(&event.stream_id)
                    {
                        let sid = event.stream_id as u64;
                        send_server_cmd(
                            &pair,
                            WorkerCommand::SendResponseHeaders {
                                conn_handle: server_conn,
                                stream_id: sid,
                                headers: vec![(":status".into(), "200".into())],
                                fin: false,
                            },
                        );
                        send_server_cmd(
                            &pair,
                            WorkerCommand::StreamSend {
                                conn_handle: server_conn,
                                stream_id: sid,
                                chunk: Chunk::unpooled(format!("ok-{sid}").into_bytes()),
                                fin: true,
                            },
                        );
                        responded_streams.insert(event.stream_id);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        responded_streams.len(),
        request_count,
        "server should have responded to all {request_count} requests, got: {}",
        responded_streams.len()
    );

    // Client: collect completed responses.
    let mut completed_streams = HashSet::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while completed_streams.len() < request_count && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    if event.event_type == EVENT_FINISHED
                        || (event.event_type == EVENT_DATA && event.fin == Some(true))
                    {
                        completed_streams.insert(event.stream_id);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        completed_streams.len(),
        request_count,
        "client should have received all {request_count} complete responses, got: {}",
        completed_streams.len()
    );
}

#[test]
fn test_h3_50_concurrent_post_requests() {
    let pair = setup_h3_pair();
    let (server_conn, _client_conn) = wait_for_h3_handshake(&pair);

    let request_count: usize = 50;
    let body_size: usize = 1024; // 1KB
    let batch_size: usize = 10;
    let mut all_stream_ids = Vec::with_capacity(request_count);

    // We send POST requests in batches of 10 to avoid overwhelming the
    // H3 stream concurrency / flow control window.  For each batch we
    // send requests, let the server process and respond, then collect
    // client responses before moving on.
    let mut total_client_completed: HashSet<i64> = HashSet::new();

    for batch_start in (0..request_count).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(request_count);
        let this_batch = batch_end - batch_start;
        let mut batch_ids = Vec::with_capacity(this_batch);

        // Client sends this batch of POST requests.
        for i in batch_start..batch_end {
            let stream_id = pair
                .client
                .send_request(
                    vec![
                        (":method".into(), "POST".into()),
                        (":scheme".into(), "https".into()),
                        (":authority".into(), "localhost".into()),
                        (":path".into(), format!("/upload/{i}")),
                    ],
                    false,
                )
                .expect("send_request should succeed");

            let body = vec![(i & 0xFF) as u8; body_size];
            assert!(pair.client.stream_send(stream_id, Chunk::unpooled(body), true));
            batch_ids.push(stream_id);
            all_stream_ids.push(stream_id);
        }

        // Server: process incoming events and respond to each request
        // in this batch once its HEADERS + fin arrive.
        let mut seen_headers: HashSet<i64> = HashSet::new();
        let mut finished_requests: HashSet<i64> = HashSet::new();
        let mut responded: HashSet<i64> = HashSet::new();
        let deadline = std::time::Instant::now() + RECV_TIMEOUT;
        while responded.len() < this_batch && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match pair.server_rx.recv_timeout(remaining) {
                Ok(batch) => {
                    for event in batch.events {
                        let sid = event.stream_id;
                        if event.event_type == EVENT_HEADERS {
                            seen_headers.insert(sid);
                        }
                        if event.event_type == EVENT_FINISHED
                            || (event.event_type == EVENT_DATA && event.fin == Some(true))
                        {
                            finished_requests.insert(sid);
                        }

                        if seen_headers.contains(&sid)
                            && finished_requests.contains(&sid)
                            && !responded.contains(&sid)
                        {
                            let stream_id = sid as u64;
                            send_server_cmd(
                                &pair,
                                WorkerCommand::SendResponseHeaders {
                                    conn_handle: server_conn,
                                    stream_id,
                                    headers: vec![(":status".into(), "200".into())],
                                    fin: true,
                                },
                            );
                            responded.insert(sid);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        assert_eq!(
            responded.len(),
            this_batch,
            "server should have responded to batch [{batch_start}..{batch_end}), got: {}",
            responded.len()
        );

        // Client: collect responses for this batch.
        let deadline = std::time::Instant::now() + RECV_TIMEOUT;
        while total_client_completed.len() < batch_end && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match pair.client_rx.recv_timeout(remaining) {
                Ok(batch) => {
                    for event in batch.events {
                        if event.event_type == EVENT_HEADERS
                            || event.event_type == EVENT_FINISHED
                        {
                            total_client_completed.insert(event.stream_id);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    assert_eq!(
        total_client_completed.len(),
        request_count,
        "client should have completed all {request_count} POST responses, got: {}",
        total_client_completed.len()
    );
}

#[test]
fn test_quic_100_concurrent_bidi_streams() {
    let pair = setup_quic_pair();
    let (server_conn, _client_conn) = wait_for_quic_handshake(&pair);

    let stream_count: usize = 100;
    let payload_size: usize = 512;

    // Client opens 100 bidi streams (IDs: 0, 4, 8, ..., 396) and sends
    // 512 bytes on each with fin=true.
    for i in 0..stream_count {
        let stream_id = (i as u64) * 4;
        let payload = vec![(i & 0xFF) as u8; payload_size];
        assert!(
            pair.client.stream_send(stream_id, payload, true),
            "failed to send on stream {stream_id}"
        );
    }

    // Server: collect data for each stream and echo it back once we see fin.
    let mut pending_data: HashMap<i64, Vec<u8>> = HashMap::new();
    let mut streams_finished: HashSet<i64> = HashSet::new();
    let mut echoed: HashSet<i64> = HashSet::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while echoed.len() < stream_count && std::time::Instant::now() < deadline {
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
                            streams_finished.insert(sid);
                        }
                    }
                    if event.event_type == EVENT_FINISHED {
                        streams_finished.insert(sid);
                    }

                    // Echo back once we have the full stream.
                    if streams_finished.contains(&sid) && !echoed.contains(&sid) {
                        let data = pending_data.remove(&sid).unwrap_or_default();
                        pair.server.send_command(QuicServerCommand::StreamSend {
                            conn_handle: server_conn,
                            stream_id: sid as u64,
                            chunk: Chunk::unpooled(data),
                            fin: true,
                        });
                        echoed.insert(sid);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        echoed.len(),
        stream_count,
        "server should have echoed all {stream_count} streams, got: {}",
        echoed.len()
    );

    // Client: verify all 100 echo responses arrive.
    let mut client_completed = HashSet::new();
    let mut client_data: HashMap<i64, Vec<u8>> = HashMap::new();
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while client_completed.len() < stream_count && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
                    let sid = event.stream_id;
                    if event.event_type == EVENT_NEW_STREAM || event.event_type == EVENT_DATA {
                        if let Some(data) = event.data.as_ref() {
                            client_data.entry(sid).or_default().extend_from_slice(data);
                        }
                        if event.fin == Some(true) {
                            client_completed.insert(sid);
                        }
                    }
                    if event.event_type == EVENT_FINISHED {
                        client_completed.insert(sid);
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        client_completed.len(),
        stream_count,
        "client should have received all {stream_count} echo responses, got: {}",
        client_completed.len()
    );

    // Verify payload integrity for each stream.
    for i in 0..stream_count {
        let sid = (i as i64) * 4;
        let expected = vec![(i & 0xFF) as u8; payload_size];
        let received = client_data.get(&sid).cloned().unwrap_or_default();
        assert_eq!(
            received, expected,
            "payload mismatch on stream {sid}: got {} bytes, expected {payload_size}",
            received.len()
        );
    }
}
