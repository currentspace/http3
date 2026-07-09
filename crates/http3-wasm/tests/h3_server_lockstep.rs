//! Lockstep integration test: drives `http3::wasm_exports::H3ServerHandler`
//! (the exact direct-call surface `crates/http3-wasm`'s new `hs_*` ABI
//! wraps) against `http3::wasm_exports::H3ClientHandler` (the EXISTING
//! `h3c_*` ABI's own direct-call surface) — proving client-ABI-vs-
//! server-ABI interop entirely within this one crate's test suite, ahead
//! of any TS/wasm-runtime work.
//!
//! Deliberately exercises both handlers at the `wasm_exports` level (real
//! Rust references, no `u32` pointer marshaling) rather than through this
//! crate's own `extern "C"` `hs_*`/`h3c_*` functions — see
//! `h3_lockstep.rs`'s identical doc comment (and `src/abi.rs`'s test
//! module) for why: that ABI's pointer convention is only sound on an
//! actual 32-bit (wasm32) address space, so a host-target test that
//! wants to drive *two* real sessions safely has to stay at this level.
//! The `extern "C"` wrapper layer itself is exercised for real by the
//! bonus wasm-artifact validation script
//! (`spikes/quiche-wasm-wasip1/validate-handshake.mjs`) for the client
//! side; the equivalent server-side wasm-artifact proof is `pnpm run
//! build:wasm` + the real cross-compile (see the verification report).
#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http3::wasm_exports::{
    Chunk, EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_NEW_SESSION,
    EVENT_SESSION_CLOSE, H3ClientHandler, H3ServerHandler, Http3Config, JsClientOptions, JsH3Event,
    JsServerOptions, OutboundAdmission, TxDatagram,
};

const TEST_SCID_LEN: usize = 20; // quiche::MAX_CONN_ID_LEN, not re-exported — fixed by the QUIC spec.
const PUMP_DEADLINE: Duration = Duration::from_secs(5);

fn test_addrs() -> (SocketAddr, SocketAddr) {
    (
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45_001), // client
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 55_001), // server
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

/// Builds a real `H3ServerHandler::new_direct` via the exact pair a
/// `hs_new` ABI implementation calls: `Http3Config::from_server_options`
/// + `Http3Config::new_server_quiche_config_in_memory`.
fn build_server_direct() -> H3ServerHandler {
    let (cert_pem, key_pem) = generate_self_signed_pem();
    let options = JsServerOptions {
        key: key_pem,
        cert: cert_pem,
        ca: None,
        client_auth: None,
        runtime_mode: None,
        max_idle_timeout_ms: Some(5_000),
        max_udp_payload_size: Some(1_350),
        initial_max_data: Some(1_000_000),
        initial_max_stream_data_bidi_local: Some(1_000_000),
        initial_max_streams_bidi: Some(100),
        disable_active_migration: Some(true),
        enable_datagrams: None,
        qpack_max_table_capacity: None,
        qpack_blocked_streams: None,
        recv_batch_size: None,
        send_batch_size: None,
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        max_connections: Some(128),
        disable_retry: Some(true),
        reuse_port: None,
        keylog: None,
        quic_lb: None,
        server_id: None,
    };
    let quiche_config = Http3Config::new_server_quiche_config_in_memory(&options).unwrap();
    let http3_config = Http3Config::from_server_options(&options).unwrap();
    H3ServerHandler::new_direct(
        quiche_config,
        http3_config,
        [0xab_u8; 32],
        Arc::new(OutboundAdmission::default()),
    )
}

fn build_client_direct(scid_byte: u8, client_addr: SocketAddr, server_addr: SocketAddr) -> H3ClientHandler {
    let client_options = JsClientOptions {
        ca: None,
        reject_unauthorized: Some(false),
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
    let mut client_config = Http3Config::new_client_quiche_config_in_memory(&client_options).unwrap();
    H3ClientHandler::new_direct(
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
    client: &mut H3ClientHandler,
    server: &mut H3ServerHandler,
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
    client.poll_drain_events_for_handle(client_batch, 0);
    client.flush_pending_writes_for_handle(client_batch, 0);

    progressed
}

#[allow(clippy::too_many_arguments)]
fn pump_until<F>(
    client: &mut H3ClientHandler,
    server: &mut H3ServerHandler,
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
fn h3c_client_abi_vs_hs_server_abi_handshake_and_request_response() {
    let (client_addr, server_addr) = test_addrs();
    let mut server = build_server_direct();
    let mut client = build_client_direct(0x51, client_addr, server_addr);

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

    let stream_id = client
        .send_request(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "localhost".into()),
                (":path".into(), "/from-h3c-abi".into()),
            ],
            true,
        )
        .expect("send_request should succeed once established");

    server_batch.clear();
    pump_until(
        &mut client,
        &mut server,
        client_addr,
        server_addr,
        &mut client_batch,
        &mut server_batch,
        |_client_batch, server_batch| server_batch.iter().any(|e| e.event_type == EVENT_HEADERS),
    );

    server
        .send_response_headers(
            conn_handle,
            stream_id,
            vec![(":status".into(), "200".into())],
            false,
            &mut server_batch,
        )
        .expect("send_response_headers should succeed");
    let released = server.queue_stream_send(
        conn_handle,
        stream_id,
        Chunk::unpooled(b"hello from hs_* server ABI".to_vec()),
        true,
        &mut server_batch,
    );
    assert!(released > 0, "response body should be admitted");

    client_batch.clear();
    let mut body = Vec::new();
    let mut got_headers = false;
    pump_until(
        &mut client,
        &mut server,
        client_addr,
        server_addr,
        &mut client_batch,
        &mut server_batch,
        |client_batch, _server_batch| {
            for event in client_batch.iter() {
                if event.stream_id as u64 != stream_id {
                    continue;
                }
                if event.event_type == EVENT_HEADERS {
                    got_headers = true;
                }
                if event.event_type == EVENT_HEADERS || event.event_type == EVENT_DATA {
                    if let Some(data) = &event.data {
                        body.extend_from_slice(data);
                    }
                }
            }
            got_headers && !body.is_empty()
        },
    );

    assert!(got_headers, "client should have observed EVENT_HEADERS");
    assert_eq!(body, b"hello from hs_* server ABI");

    // --- Close, from the server side ---
    server.close_connection(conn_handle, 0, "test done".to_string());
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
