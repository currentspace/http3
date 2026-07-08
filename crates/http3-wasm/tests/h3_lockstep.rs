//! Lockstep integration test: drives `http3::wasm_exports::H3ClientHandler`
//! (the exact direct-call surface `crates/http3-wasm`'s ABI wraps) against
//! a hand-rolled raw-`quiche` H3 server, over real loopback UDP sockets.
//!
//! Per docs/WASM_CLIENT_PLAN.md A3's Definition of Done: the A2-task-6
//! lockstep pumps are `#[cfg(test)]`-internal to the `http3` crate and
//! unreachable here, so this test hand-rolls the server side directly
//! with the `quiche` dev-dependency instead (same pattern as this repo's
//! own `tests/interop_udp_loopback.rs`).
//!
//! Deliberately exercises the client at the `wasm_exports` level (real
//! Rust references, no `u32` pointer marshaling) rather than through this
//! crate's own `extern "C"` ABI — that ABI's pointer convention is only
//! sound on an actual 32-bit (wasm32) address space; see
//! `src/abi.rs`'s test module for why. The `extern "C"` wrapper layer
//! itself is exercised for real in the bonus wasm-artifact validation
//! script (`spikes/quiche-wasm-wasip1/validate-handshake.mjs`).
#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http3::wasm_exports::{
    EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, H3ClientHandler, Http3Config,
    JsClientOptions, JsH3Event, OutboundAdmission,
};

const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

fn generate_test_certs() -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("http3wasm_lockstep_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("http3wasm_lockstep_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

fn make_server_config(cert_path: &std::path::Path, key_path: &std::path::Path) -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config
        .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
        .unwrap();
    config
        .load_priv_key_from_pem_file(key_path.to_str().unwrap())
        .unwrap();
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config
}

/// One pump step for the `H3ClientHandler` side: flush every pending
/// outbound datagram to the socket, then drain (non-blockingly) any
/// datagrams the socket already has buffered. Returns whether anything
/// happened.
fn pump_client(
    handler: &mut H3ClientHandler,
    sock: &UdpSocket,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    events: &mut Vec<JsH3Event>,
) -> bool {
    let mut progressed = false;

    while let Some(tx) = handler.try_send_next() {
        sock.send_to(tx.payload(), tx.to).unwrap();
        progressed = true;
    }

    let mut buf = vec![0u8; 65535];
    loop {
        sock.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        match sock.recv_from(&mut buf) {
            Ok((len, _from)) => {
                handler.process_packet_for_handle(
                    &mut buf[..len],
                    server_addr,
                    client_addr,
                    usize::MAX,
                    events,
                    1,
                );
                progressed = true;
            }
            Err(_) => break,
        }
    }

    if handler.next_timer_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
        handler.process_timers_for_handle(Instant::now(), usize::MAX, events, 1);
        progressed = true;
    }

    progressed
}

fn pump_server(
    sock: &UdpSocket,
    server_conn: &mut quiche::Connection,
    server_addr: SocketAddr,
) -> bool {
    let mut progressed = false;
    let mut out = vec![0u8; 1350];

    loop {
        match server_conn.send(&mut out) {
            Ok((len, info)) => {
                sock.send_to(&out[..len], info.to).unwrap();
                progressed = true;
            }
            Err(quiche::Error::Done) => break,
            Err(e) => panic!("server send: {e}"),
        }
    }

    let mut buf = vec![0u8; 65535];
    loop {
        sock.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        match sock.recv_from(&mut buf) {
            Ok((len, from)) => {
                let recv_info = quiche::RecvInfo {
                    from,
                    to: server_addr,
                };
                let _ = server_conn.recv(&mut buf[..len], recv_info);
                progressed = true;
            }
            Err(_) => break,
        }
    }

    if server_conn.timeout().is_some_and(|t| t.is_zero()) {
        server_conn.on_timeout();
        progressed = true;
    }

    progressed
}

#[test]
fn client_handler_completes_handshake_and_gets_h3_response() {
    let (cert_path, key_path) = generate_test_certs();
    let mut server_config = make_server_config(&cert_path, &key_path);

    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    let client_opts = JsClientOptions {
        ca: None,
        reject_unauthorized: Some(false),
        runtime_mode: None,
        max_idle_timeout_ms: Some(5000),
        max_udp_payload_size: Some(1350),
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
    let mut client_quiche_config =
        Http3Config::new_client_quiche_config_in_memory(&client_opts).unwrap();

    let scid: Vec<u8> = (0u8..20).collect();
    let mut client = H3ClientHandler::new_direct(
        scid,
        client_addr,
        server_addr,
        "localhost",
        None,
        None,
        None,
        &mut client_quiche_config,
        Arc::new(OutboundAdmission::default()),
    )
    .expect("client handler construction should succeed");

    let mut client_events: Vec<JsH3Event> = Vec::new();

    // --- Handshake: pump client, feed its Initial into a fresh server accept ---
    pump_client(&mut client, &client_sock, server_addr, client_addr, &mut client_events);

    let mut buf = vec![0u8; 65535];
    server_sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (len, from) = server_sock
        .recv_from(&mut buf)
        .expect("server should receive the client Initial");
    let hdr = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN).unwrap();
    let server_scid = quiche::ConnectionId::from_ref(b"http3wasm-lockstep-0");
    let mut server_conn = quiche::accept(
        &server_scid,
        Some(&hdr.dcid),
        server_addr,
        from,
        &mut server_config,
    )
    .unwrap();
    server_conn
        .recv(
            &mut buf[..len],
            quiche::RecvInfo {
                from,
                to: server_addr,
            },
        )
        .unwrap();

    let h3_config = quiche::h3::Config::new().unwrap();
    let mut server_h3: Option<quiche::h3::Connection> = None;

    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    while Instant::now() < deadline {
        let c = pump_client(&mut client, &client_sock, server_addr, client_addr, &mut client_events);
        let s = pump_server(&server_sock, &mut server_conn, server_addr);
        if server_h3.is_none() && server_conn.is_established() {
            server_h3 = Some(quiche::h3::Connection::with_transport(&mut server_conn, &h3_config).unwrap());
        }
        if !c && !s && client_events.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE) {
            break;
        }
    }

    assert!(
        client_events.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE),
        "client should have observed EVENT_HANDSHAKE_COMPLETE"
    );
    let mut server_h3 = server_h3.expect("server H3 connection should be initialized");

    // --- GET request ---
    let stream_id = client
        .send_request(
            vec![
                (":method".to_string(), "GET".to_string()),
                (":path".to_string(), "/hello".to_string()),
                (":authority".to_string(), "localhost".to_string()),
                (":scheme".to_string(), "https".to_string()),
            ],
            true,
        )
        .expect("send_request should succeed once established");

    let mut responded = false;
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    while Instant::now() < deadline && !responded {
        pump_client(&mut client, &client_sock, server_addr, client_addr, &mut client_events);
        pump_server(&server_sock, &mut server_conn, server_addr);

        loop {
            match server_h3.poll(&mut server_conn) {
                Ok((sid, quiche::h3::Event::Headers { .. })) => {
                    let resp = vec![quiche::h3::Header::new(b":status", b"200")];
                    server_h3.send_response(&mut server_conn, sid, &resp, false).unwrap();
                    server_h3
                        .send_body(&mut server_conn, sid, b"hello from lockstep server", true)
                        .unwrap();
                    responded = true;
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => panic!("server h3 poll: {e}"),
            }
        }
    }
    assert!(responded, "server should have received the GET and responded");

    // --- Drain client-side response events ---
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    let mut got_headers = false;
    let mut body = Vec::new();
    let mut processed = 0usize;
    while Instant::now() < deadline {
        pump_client(&mut client, &client_sock, server_addr, client_addr, &mut client_events);
        pump_server(&server_sock, &mut server_conn, server_addr);

        for event in &client_events[processed..] {
            if event.stream_id as u64 != stream_id {
                continue;
            }
            if event.event_type == EVENT_HEADERS {
                got_headers = true;
            }
            // A HEADERS event may carry a small coalesced DATA payload
            // (`try_coalesce_following_data` in connection.rs) — check
            // `.data` on every event for this stream, not just
            // `EVENT_DATA`-typed ones.
            let carries_data = event.event_type == EVENT_HEADERS || event.event_type == EVENT_DATA;
            // Named boolean + nested `if let` reads clearer here than a
            // squashed `&&`/`then_some` chain; let-chains aren't stable yet.
            #[allow(clippy::collapsible_if)]
            if carries_data {
                if let Some(data) = &event.data {
                    body.extend_from_slice(data);
                }
            }
        }
        processed = client_events.len();
        if got_headers && !body.is_empty() {
            break;
        }
    }

    assert!(got_headers, "client should have received EVENT_HEADERS");
    assert_eq!(body, b"hello from lockstep server");

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}
