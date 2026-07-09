//! Lockstep integration test: drives `http3::wasm_exports::QuicServerHandler`
//! (the exact direct-call surface `crates/http3-wasm`'s new `qs_*` ABI
//! wraps) against `http3::wasm_exports::QuicClientHandler` (the EXISTING
//! `qc_*` ABI's own direct-call surface) — proving client-ABI-vs-
//! server-ABI interop for raw QUIC too, mirroring
//! `h3_server_lockstep.rs` exactly. See that file's doc comment for why
//! this stays at the `wasm_exports` level rather than the raw `extern
//! "C"` `qs_*`/`qc_*` functions (pointer-truncation hazard on a 64-bit
//! host).
#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http3::wasm_exports::{
    Chunk, CidEncoding, ClientAuthMode, EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION,
    EVENT_NEW_STREAM, EVENT_SESSION_CLOSE, JsH3Event, JsQuicClientOptions, JsQuicServerOptions,
    OutboundAdmission, QuicClientHandler, QuicServerConfig, QuicServerHandler, TransportRuntimeMode,
    TxDatagram, new_quic_client_config_in_memory, new_quic_server_config_in_memory,
};

const TEST_SCID_LEN: usize = 20; // quiche::MAX_CONN_ID_LEN, not re-exported.
const PUMP_DEADLINE: Duration = Duration::from_secs(5);

fn test_addrs() -> (SocketAddr, SocketAddr) {
    (
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 46_001), // client
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 56_001), // server
    )
}

fn generate_self_signed_pem() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (
        cert.pem().into_bytes(),
        key_pair.serialize_pem().into_bytes(),
    )
}

fn build_server_direct() -> QuicServerHandler {
    let (cert_pem, key_pem) = generate_self_signed_pem();
    let options = JsQuicServerOptions {
        key: key_pem,
        cert: cert_pem,
        ca: None,
        client_auth: None,
        alpn: None,
        runtime_mode: None,
        max_idle_timeout_ms: Some(5_000),
        max_udp_payload_size: Some(1_350),
        initial_max_data: Some(1_000_000),
        initial_max_stream_data_bidi_local: Some(1_000_000),
        initial_max_streams_bidi: Some(100),
        disable_active_migration: Some(true),
        enable_datagrams: None,
        max_connections: Some(128),
        disable_retry: Some(true),
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        keylog: None,
    };
    let quiche_config = new_quic_server_config_in_memory(&options).unwrap();
    let server_config = QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: options.max_connections.unwrap_or(10_000) as usize,
        disable_retry: options.disable_retry.unwrap_or(false),
        client_auth: ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some()).unwrap(),
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
    };
    QuicServerHandler::new_direct(
        quiche_config,
        server_config,
        [0xcd_u8; 32],
        Arc::new(OutboundAdmission::default()),
    )
}

fn build_client_direct(scid_byte: u8, client_addr: SocketAddr, server_addr: SocketAddr) -> QuicClientHandler {
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: None,
        runtime_mode: None,
        max_idle_timeout_ms: Some(5_000),
        max_udp_payload_size: Some(1_350),
        initial_max_data: None,
        initial_max_stream_data_bidi_local: None,
        initial_max_streams_bidi: None,
        session_ticket: None,
        allow_0rtt: None,
        enable_datagrams: None,
        keylog: None,
        qlog_dir: None,
        qlog_level: None,
        disable_pacing: Some(true),
    };
    let mut client_config = new_quic_client_config_in_memory(&client_options).unwrap();
    QuicClientHandler::new_direct(
        vec![scid_byte; TEST_SCID_LEN],
        client_addr,
        server_addr,
        "localhost",
        None,
        None,
        None,
        &mut client_config,
        Arc::new(OutboundAdmission::default()),
    )
    .expect("client new_direct should construct")
}

#[allow(clippy::too_many_arguments)]
fn pump(
    client: &mut QuicClientHandler,
    server: &mut QuicServerHandler,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    client_batch: &mut Vec<JsH3Event>,
    server_batch: &mut Vec<JsH3Event>,
) -> bool {
    let mut progressed = false;

    while let Some(pkt) = client.try_send_next() {
        progressed = true;
        let mut buf = pkt.payload().to_vec();
        let mut pending_outbound: Vec<TxDatagram> = Vec::new();
        server.process_inbound_packet(
            &mut buf,
            client_addr,
            server_addr,
            &mut pending_outbound,
            usize::MAX,
            server_batch,
        );
        for reply in pending_outbound {
            let mut reply_buf = reply.payload().to_vec();
            client.process_packet_for_handle(&mut reply_buf, server_addr, client_addr, usize::MAX, client_batch, 0);
        }
    }

    let mut server_outbound: Vec<TxDatagram> = Vec::new();
    server.flush_all_sends(&mut server_outbound);
    for pkt in server_outbound {
        progressed = true;
        let mut buf = pkt.payload().to_vec();
        client.process_packet_for_handle(&mut buf, server_addr, client_addr, usize::MAX, client_batch, 0);
    }

    if client.next_timer_deadline().is_some_and(|d| d <= Instant::now()) {
        client.process_timers_for_handle(Instant::now(), usize::MAX, client_batch, 0);
        progressed = true;
    }
    if server.soonest_deadline().is_some_and(|d| d <= Instant::now()) {
        server.expire_timers(Instant::now(), usize::MAX, server_batch);
        progressed = true;
    }

    server.collect_drain_events(usize::MAX, server_batch);
    server.flush_all_pending_writes(server_batch);
    client.poll_drain_events_for_handle(usize::MAX, client_batch, 0);
    client.flush_pending_writes_for_handle(client_batch, 0);

    progressed
}

#[allow(clippy::too_many_arguments)]
fn pump_until<F>(
    client: &mut QuicClientHandler,
    server: &mut QuicServerHandler,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    client_batch: &mut Vec<JsH3Event>,
    server_batch: &mut Vec<JsH3Event>,
    mut done: F,
) where
    F: FnMut(&[JsH3Event], &[JsH3Event]) -> bool,
{
    let deadline = Instant::now() + PUMP_DEADLINE;
    while Instant::now() < deadline {
        if done(client_batch, server_batch) {
            return;
        }
        let progressed = pump(client, server, client_addr, server_addr, client_batch, server_batch);
        if done(client_batch, server_batch) {
            return;
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    panic!("pump_until exceeded the 5s deadline without reaching the target condition");
}

#[test]
fn qc_client_abi_vs_qs_server_abi_handshake_stream_echo_and_close() {
    let (client_addr, server_addr) = test_addrs();
    let mut server = build_server_direct();
    let mut client = build_client_direct(0x91, client_addr, server_addr);

    let mut client_batch = Vec::new();
    let mut server_batch = Vec::new();

    pump_until(
        &mut client,
        &mut server,
        client_addr,
        server_addr,
        &mut client_batch,
        &mut server_batch,
        |client_batch, server_batch| {
            client_batch.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
                && server_batch.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
        },
    );

    assert!(server_batch.iter().any(|e| e.event_type == EVENT_NEW_SESSION));
    let conn_handle = server_batch
        .iter()
        .find(|e| e.event_type == EVENT_NEW_SESSION)
        .expect("new session event")
        .conn_handle;
    assert_eq!(server.connection_count(), 1);

    let stream_id = client.open_bidi_stream().expect("open_bidi_stream");
    let released = client.queue_stream_send(stream_id, Chunk::unpooled(b"ping".to_vec()), true, &mut client_batch, 0);
    assert!(released > 0, "client stream send should be admitted");

    // Raw QUIC coalesces a new stream's first recv into `EVENT_NEW_STREAM`
    // itself (see `QuicConnection::poll_quic_events`'s "Coalesce first
    // recv into NEW_STREAM event" comment) — check `.data` on every event
    // for this stream, not just `EVENT_DATA`-typed ones.
    server_batch.clear();
    pump_until(
        &mut client,
        &mut server,
        client_addr,
        server_addr,
        &mut client_batch,
        &mut server_batch,
        |_client_batch, server_batch| server_batch.iter().any(|e| e.stream_id as u64 == stream_id && e.data.is_some()),
    );
    assert!(server_batch.iter().any(|e| e.event_type == EVENT_NEW_STREAM));
    let received: Vec<u8> = server_batch
        .iter()
        .filter(|e| e.stream_id as u64 == stream_id)
        .filter_map(|e| e.data.as_deref())
        .flatten()
        .copied()
        .collect();
    assert_eq!(received, b"ping");

    let echoed = server.queue_stream_send(conn_handle, stream_id, Chunk::unpooled(b"pong".to_vec()), true, &mut server_batch);
    assert!(echoed > 0, "server echo should be admitted");

    client_batch.clear();
    pump_until(
        &mut client,
        &mut server,
        client_addr,
        server_addr,
        &mut client_batch,
        &mut server_batch,
        |client_batch, _server_batch| {
            client_batch.iter().any(|e| e.event_type == EVENT_DATA && e.stream_id as u64 == stream_id)
        },
    );
    let echoed_body: Vec<u8> = client_batch
        .iter()
        .filter(|e| e.event_type == EVENT_DATA && e.stream_id as u64 == stream_id)
        .filter_map(|e| e.data.as_deref())
        .flatten()
        .copied()
        .collect();
    assert_eq!(echoed_body, b"pong");

    // --- Close, from the server side ---
    server.close_connection(conn_handle, 0, "test done");
    let close_deadline = Instant::now() + PUMP_DEADLINE;
    while Instant::now() < close_deadline && !server.connection_is_closed(conn_handle) {
        let progressed = pump(&mut client, &mut server, client_addr, server_addr, &mut client_batch, &mut server_batch);
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(server.connection_is_closed(conn_handle));

    server.reap_closed_connections(&mut server_batch);
    assert!(server_batch.iter().any(|e| e.event_type == EVENT_SESSION_CLOSE));
    assert!(server.is_idle());
}
