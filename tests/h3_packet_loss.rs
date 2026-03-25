//! Packet loss simulation tests for HTTP/3 retransmission.
//! Verifies that H3 request/response cycles complete under packet loss.
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
static NEXT_PORT: AtomicU16 = AtomicU16::new(57_000);

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

fn build_h3_quiche_configs(cert_pem: &str, key_pem: &str) -> (quiche::Config, quiche::Config) {
    let _lock = CONFIG_MUTEX.lock().unwrap();
    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("h3_loss_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("h3_loss_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

    let mut server_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    server_config.load_cert_chain_from_pem_file(cert_path.to_str().unwrap()).unwrap();
    server_config.load_priv_key_from_pem_file(key_path.to_str().unwrap()).unwrap();
    server_config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).unwrap();
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
    client_config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).unwrap();
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

struct H3PairWithLoss {
    _server: H3ServerWorker,
    client: ClientWorkerHandle,
    server_rx: Receiver<TaggedEventBatch>,
    client_rx: Receiver<TaggedEventBatch>,
    loss: PacketLossConfig,
}

const RECV_TIMEOUT: Duration = Duration::from_secs(15);

fn setup_h3_pair_with_loss(drop_pct: u32) -> H3PairWithLoss {
    let (cert_pem, key_pem) = generate_test_certs();
    let (client_addr, server_addr) = next_pair_addrs();
    let (server_quiche, client_quiche) =
        build_h3_quiche_configs(std::str::from_utf8(&cert_pem).unwrap(), std::str::from_utf8(&key_pem).unwrap());

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

    let loss = PacketLossConfig::new(drop_pct);
    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair_with_loss(client_addr, server_addr, loss.clone());

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _) = channel_batcher("server", server_event_tx);
    let (client_batcher, _) = channel_batcher("client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (_client_cmd_tx, client_cmd_rx) = unbounded();

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
        _client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    )
    ;

    H3PairWithLoss {
        _server: server_worker,
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

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn test_h3_request_response_under_5pct_loss() {
    let pair = setup_h3_pair_with_loss(5);

    // Wait for handshake
    let server_new = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server NEW_SESSION under 5% loss");
    let _client_hs = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client HANDSHAKE_COMPLETE under 5% loss");

    // Client sends GET
    let _ = pair.client.send_request(
        vec![
            (":method".into(), "GET".into()),
            (":path".into(), "/lossy".into()),
            (":authority".into(), "localhost".into()),
            (":scheme".into(), "https".into()),
        ],
        true,
    );

    // Server sees HEADERS
    let headers_event = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS
    })
    .expect("server HEADERS under 5% loss");

    // Server responds
    fn send_cmd(server: &H3ServerWorker, cmd: WorkerCommand) {
        server.cmd_tx.send(cmd).unwrap();
        let _ = server.waker.wake();
    }

    send_cmd(&pair._server, WorkerCommand::SendResponseHeaders {
        conn_handle: server_new.conn_handle,
        stream_id: headers_event.stream_id as u64,
        headers: vec![(":status".into(), "200".into())],
        fin: false,
    });
    send_cmd(&pair._server, WorkerCommand::StreamSend {
        conn_handle: server_new.conn_handle,
        stream_id: headers_event.stream_id as u64,
        chunk: Chunk::unpooled(b"hello under loss".to_vec()),
        fin: true,
    });

    // Client receives response
    let mut client_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
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
            Err(_) => break,
        }
    }

    assert!(got_fin, "client should receive response under 5% H3 loss");
    assert_eq!(client_data, b"hello under loss");

    let (sent, dropped) = pair.loss.stats();
    eprintln!(
        "  H3 5% loss: {sent} sent, {dropped} dropped ({:.1}%)",
        if sent > 0 { (dropped as f64 / sent as f64) * 100.0 } else { 0.0 }
    );
}

#[test]
fn test_h3_large_body_under_10pct_loss() {
    let pair = setup_h3_pair_with_loss(10);

    let server_new = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_NEW_SESSION
    })
    .expect("server NEW_SESSION under 10% loss");
    let _client_hs = recv_event_matching(&pair.client_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HANDSHAKE_COMPLETE
    })
    .expect("client HANDSHAKE_COMPLETE under 10% loss");

    let _ = pair.client.send_request(
        vec![
            (":method".into(), "GET".into()),
            (":path".into(), "/large-lossy".into()),
            (":authority".into(), "localhost".into()),
            (":scheme".into(), "https".into()),
        ],
        true,
    );

    let headers_event = recv_event_matching(&pair.server_rx, RECV_TIMEOUT, |e| {
        e.event_type == EVENT_HEADERS
    })
    .expect("server HEADERS under 10% loss");

    // Server responds with 32KB body
    fn send_cmd2(server: &H3ServerWorker, cmd: WorkerCommand) {
        server.cmd_tx.send(cmd).unwrap();
        let _ = server.waker.wake();
    }

    send_cmd2(&pair._server, WorkerCommand::SendResponseHeaders {
        conn_handle: server_new.conn_handle,
        stream_id: headers_event.stream_id as u64,
        headers: vec![(":status".into(), "200".into())],
        fin: false,
    });
    send_cmd2(&pair._server, WorkerCommand::StreamSend {
        conn_handle: server_new.conn_handle,
        stream_id: headers_event.stream_id as u64,
        chunk: Chunk::unpooled(vec![0xCC_u8; 32 * 1024]),
        fin: true,
    });

    let mut client_data = Vec::new();
    let mut got_fin = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !got_fin && std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match pair.client_rx.recv_timeout(remaining) {
            Ok(batch) => {
                for event in batch.events {
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
            Err(_) => break,
        }
    }

    assert!(
        got_fin,
        "should receive 32KB body under 10% H3 loss (got {} bytes)",
        client_data.len()
    );
    assert_eq!(client_data.len(), 32 * 1024);

    let (sent, dropped) = pair.loss.stats();
    eprintln!(
        "  H3 10% loss 32KB: {sent} sent, {dropped} dropped ({:.1}%)",
        if sent > 0 { (dropped as f64 / sent as f64) * 100.0 } else { 0.0 }
    );
}
