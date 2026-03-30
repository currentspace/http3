//! Stress tests for real UDP QUIC connections.
//! All tests are `#[ignore]` so they do not run in CI.
//! Run with: cargo test --test stress_udp_quic --features bench-internals --no-default-features -- --ignored
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::net::UdpSocket;

const MAX_DATAGRAM_SIZE: usize = 1350;

// ── Cert generation ─────────────────────────────────────────────────

fn generate_test_certs() -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();

    let id = std::thread::current().id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let cert_path = std::env::temp_dir().join(format!("stress_cert_{id:?}_{ts}.pem"));
    let key_path = std::env::temp_dir().join(format!("stress_key_{id:?}_{ts}.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

// ── Config builders ─────────────────────────────────────────────────

fn make_server_config(cert_path: &std::path::Path, key_path: &std::path::Path) -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config
        .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
        .unwrap();
    config
        .load_priv_key_from_pem_file(key_path.to_str().unwrap())
        .unwrap();
    config.set_application_protos(&[b"bench"]).unwrap();
    config.set_max_idle_timeout(10_000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config
}

fn make_client_config() -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(&[b"bench"]).unwrap();
    config.verify_peer(false);
    config.set_max_idle_timeout(10_000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(100);
    config
}

// ── UDP exchange helper ─────────────────────────────────────────────

/// Exchange packets between client and server over real UDP sockets
/// until both connections are established or we exhaust rounds.
fn exchange_udp(
    client_sock: &UdpSocket,
    server_sock: &UdpSocket,
    client_conn: &mut quiche::Connection,
    server_conn: &mut quiche::Connection,
) {
    let mut buf = vec![0u8; 65535];
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];

    for _ in 0..50 {
        // Client -> Server
        loop {
            match client_conn.send(&mut out) {
                Ok((len, info)) => {
                    client_sock.send_to(&out[..len], info.to).unwrap();
                }
                Err(quiche::Error::Done) => break,
                Err(e) => panic!("client send: {e}"),
            }
        }

        server_sock.set_nonblocking(true).unwrap();
        loop {
            match server_sock.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo {
                        from,
                        to: server_sock.local_addr().unwrap(),
                    };
                    server_conn.recv(&mut buf[..len], recv_info).ok();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("server recv: {e}"),
            }
        }

        // Server -> Client
        loop {
            match server_conn.send(&mut out) {
                Ok((len, info)) => {
                    server_sock.send_to(&out[..len], info.to).unwrap();
                }
                Err(quiche::Error::Done) => break,
                Err(e) => panic!("server send: {e}"),
            }
        }

        client_sock.set_nonblocking(true).unwrap();
        loop {
            match client_sock.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo {
                        from,
                        to: client_sock.local_addr().unwrap(),
                    };
                    client_conn.recv(&mut buf[..len], recv_info).ok();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("client recv: {e}"),
            }
        }

        if client_conn.is_established() && server_conn.is_established() {
            break;
        }
    }
}

// ── Connection setup helper ─────────────────────────────────────────

struct UdpQuicPair {
    client_sock: UdpSocket,
    server_sock: UdpSocket,
    client_conn: quiche::Connection,
    server_conn: quiche::Connection,
}

fn setup_udp_quic_pair(
    server_config: &mut quiche::Config,
    client_config: &mut quiche::Config,
    scid_byte: u8,
) -> UdpQuicPair {
    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();

    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    let scid = vec![scid_byte; quiche::MAX_CONN_ID_LEN];
    let scid_ref = quiche::ConnectionId::from_ref(&scid);

    let mut client_conn = quiche::connect(
        Some("localhost"),
        &scid_ref,
        client_addr,
        server_addr,
        client_config,
    )
    .unwrap();

    // Send initial packet
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let (len, info) = client_conn.send(&mut out).unwrap();
    client_sock.send_to(&out[..len], info.to).unwrap();

    // Server receives initial and accepts
    let mut buf = vec![0u8; 65535];
    server_sock.set_nonblocking(false).unwrap();
    let (len, from) = server_sock.recv_from(&mut buf).unwrap();

    let hdr = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN).unwrap();
    let server_scid = vec![scid_byte.wrapping_add(0x11); quiche::MAX_CONN_ID_LEN];
    let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);

    let mut server_conn = quiche::accept(
        &server_scid_ref,
        Some(&hdr.dcid),
        server_addr,
        from,
        server_config,
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

    // Complete handshake
    exchange_udp(&client_sock, &server_sock, &mut client_conn, &mut server_conn);

    assert!(client_conn.is_established(), "client handshake failed");
    assert!(server_conn.is_established(), "server handshake failed");

    UdpQuicPair {
        client_sock,
        server_sock,
        client_conn,
        server_conn,
    }
}

/// Single round of bidirectional packet exchange while also draining
/// `stream_recv` on both sides.  This keeps the streams alive long enough
/// for the application to read the data before quiche garbage-collects them.
fn exchange_and_drain(
    pair: &mut UdpQuicPair,
    stream_id: u64,
    server_buf: &mut Vec<u8>,
    client_buf: &mut Vec<u8>,
    recv_buf: &mut [u8],
    server_fin: &mut bool,
    client_fin: &mut bool,
) {
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let mut pkt_buf = vec![0u8; 65535];

    // Client -> Server (packets)
    loop {
        match pair.client_conn.send(&mut out) {
            Ok((len, info)) => {
                pair.client_sock.send_to(&out[..len], info.to).unwrap();
            }
            Err(quiche::Error::Done) => break,
            Err(e) => panic!("client send: {e}"),
        }
    }

    // Server socket drain
    pair.server_sock.set_nonblocking(true).unwrap();
    loop {
        match pair.server_sock.recv_from(&mut pkt_buf) {
            Ok((len, from)) => {
                let ri = quiche::RecvInfo {
                    from,
                    to: pair.server_sock.local_addr().unwrap(),
                };
                pair.server_conn.recv(&mut pkt_buf[..len], ri).ok();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("server recv: {e}"),
        }
    }

    // Server stream drain (read data before quiche can reap)
    if !*server_fin {
        loop {
            match pair.server_conn.stream_recv(stream_id, recv_buf) {
                Ok((n, fin)) => {
                    server_buf.extend_from_slice(&recv_buf[..n]);
                    if fin {
                        *server_fin = true;
                    }
                }
                Err(quiche::Error::Done) | Err(quiche::Error::InvalidStreamState(..)) => break,
                Err(e) => panic!("server stream_recv: {e}"),
            }
        }
    }

    // Server -> Client (packets)
    loop {
        match pair.server_conn.send(&mut out) {
            Ok((len, info)) => {
                pair.server_sock.send_to(&out[..len], info.to).unwrap();
            }
            Err(quiche::Error::Done) => break,
            Err(e) => panic!("server send: {e}"),
        }
    }

    // Client socket drain
    pair.client_sock.set_nonblocking(true).unwrap();
    loop {
        match pair.client_sock.recv_from(&mut pkt_buf) {
            Ok((len, from)) => {
                let ri = quiche::RecvInfo {
                    from,
                    to: pair.client_sock.local_addr().unwrap(),
                };
                pair.client_conn.recv(&mut pkt_buf[..len], ri).ok();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("client recv: {e}"),
        }
    }

    // Client stream drain (read data before quiche can reap)
    if !*client_fin {
        loop {
            match pair.client_conn.stream_recv(stream_id, recv_buf) {
                Ok((n, fin)) => {
                    client_buf.extend_from_slice(&recv_buf[..n]);
                    if fin {
                        *client_fin = true;
                    }
                }
                Err(quiche::Error::Done) | Err(quiche::Error::InvalidStreamState(..)) => break,
                Err(e) => panic!("client stream_recv: {e}"),
            }
        }
    }
}

/// Send payload on a stream, exchange, server echoes, exchange, client drains.
fn echo_stream(pair: &mut UdpQuicPair, stream_id: u64, payload: &[u8]) {
    let mut recv_buf = vec![0u8; 65535];
    let mut server_data = Vec::new();
    let mut server_fin = false;
    let mut client_data = Vec::new();
    let mut client_fin = false;

    // Client sends
    pair.client_conn
        .stream_send(stream_id, payload, true)
        .unwrap();

    // Exchange until server has received the full stream
    for _ in 0..100 {
        exchange_and_drain(
            pair,
            stream_id,
            &mut server_data,
            &mut client_data,
            &mut recv_buf,
            &mut server_fin,
            &mut client_fin,
        );
        if server_fin {
            break;
        }
    }
    assert!(server_fin, "server did not receive fin on stream {stream_id}");

    // Server echoes
    pair.server_conn
        .stream_send(stream_id, &server_data, true)
        .unwrap();

    // Exchange until client has received the echo
    for _ in 0..100 {
        exchange_and_drain(
            pair,
            stream_id,
            &mut server_data,
            &mut client_data,
            &mut recv_buf,
            &mut server_fin,
            &mut client_fin,
        );
        if client_fin {
            break;
        }
    }

    assert!(client_fin, "client never completed stream {stream_id}");
    assert_eq!(
        client_data.len(),
        payload.len(),
        "echo mismatch on stream {stream_id}: got {} bytes, expected {}",
        client_data.len(),
        payload.len()
    );
}

// ── Tests ───────────────────────────────────────────────────────────

#[test]
#[ignore]
fn test_udp_10_connections_serial() {
    let (cert_path, key_path) = generate_test_certs();
    let payload = vec![0xAA_u8; 1024];

    for i in 0..10u8 {
        let mut server_config = make_server_config(&cert_path, &key_path);
        let mut client_config = make_client_config();
        let mut pair = setup_udp_quic_pair(&mut server_config, &mut client_config, 0x10 + i);

        // Echo on stream 0
        echo_stream(&mut pair, 0, &payload);
    }

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
#[ignore]
fn test_udp_5_connections_parallel() {
    let (cert_path, key_path) = generate_test_certs();
    let payload = vec![0xBB_u8; 1024];

    let handles: Vec<_> = (0..5u8)
        .map(|i| {
            let cert = cert_path.clone();
            let key = key_path.clone();
            let data = payload.clone();
            std::thread::spawn(move || {
                let mut server_config = make_server_config(&cert, &key);
                let mut client_config = make_client_config();
                let mut pair =
                    setup_udp_quic_pair(&mut server_config, &mut client_config, 0x20 + i);

                // 3 streams per connection, echoed sequentially
                for s in 0..3u64 {
                    let stream_id = s * 4;
                    echo_stream(&mut pair, stream_id, &data);
                }
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|e| panic!("connection {i} panicked: {e:?}"));
    }

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
#[ignore]
fn test_udp_256kb_transfer() {
    let (cert_path, key_path) = generate_test_certs();
    let mut server_config = make_server_config(&cert_path, &key_path);
    let mut client_config = make_client_config();

    let mut pair = setup_udp_quic_pair(&mut server_config, &mut client_config, 0x30);

    let payload = vec![0xCC_u8; 256 * 1024];
    let stream_id: u64 = 0;
    let mut recv_buf = vec![0u8; 65535];

    // Phase 1: Client sends 256KB with interleaved exchange.
    // We read server-side data inline to prevent stream reaping.
    let mut total_sent = 0;
    let mut server_data = Vec::new();
    let mut server_fin = false;
    let mut _client_data_phase1 = Vec::new();
    let mut _client_fin_phase1 = false;
    for _ in 0..500 {
        match pair
            .client_conn
            .stream_send(stream_id, &payload[total_sent..], true)
        {
            Ok(n) => total_sent += n,
            Err(quiche::Error::Done) => {}
            Err(e) => panic!("client stream_send: {e}"),
        }
        exchange_and_drain(
            &mut pair,
            stream_id,
            &mut server_data,
            &mut _client_data_phase1,
            &mut recv_buf,
            &mut server_fin,
            &mut _client_fin_phase1,
        );
        if total_sent >= payload.len() && server_fin {
            break;
        }
    }
    // Extra rounds to ensure all data arrives at server
    for _ in 0..50 {
        if server_fin {
            break;
        }
        exchange_and_drain(
            &mut pair,
            stream_id,
            &mut server_data,
            &mut _client_data_phase1,
            &mut recv_buf,
            &mut server_fin,
            &mut _client_fin_phase1,
        );
    }
    assert_eq!(total_sent, payload.len(), "client did not send all 256KB");
    assert!(server_fin, "server did not receive fin");

    assert_eq!(
        server_data.len(),
        256 * 1024,
        "server should receive complete 256KB payload, got {} bytes",
        server_data.len()
    );
    assert!(
        server_data.iter().all(|&b| b == 0xCC),
        "payload content mismatch"
    );

    // Phase 2: Server echoes back with interleaved exchange.
    let mut echo_sent = 0;
    let mut client_data = Vec::new();
    let mut client_fin = false;
    // server_fin is already true, reuse a dummy for the server side
    let mut _server_dummy = Vec::new();
    let mut _server_fin_dummy = true;
    for _ in 0..500 {
        match pair
            .server_conn
            .stream_send(stream_id, &server_data[echo_sent..], true)
        {
            Ok(n) => echo_sent += n,
            Err(quiche::Error::Done) => {}
            Err(e) => panic!("server echo stream_send: {e}"),
        }
        exchange_and_drain(
            &mut pair,
            stream_id,
            &mut _server_dummy,
            &mut client_data,
            &mut recv_buf,
            &mut _server_fin_dummy,
            &mut client_fin,
        );
        if echo_sent >= server_data.len() && client_fin {
            break;
        }
    }
    // Extra rounds for final delivery
    for _ in 0..50 {
        if client_fin {
            break;
        }
        exchange_and_drain(
            &mut pair,
            stream_id,
            &mut _server_dummy,
            &mut client_data,
            &mut recv_buf,
            &mut _server_fin_dummy,
            &mut client_fin,
        );
    }

    assert_eq!(
        client_data.len(),
        256 * 1024,
        "client should receive complete 256KB echo, got {} bytes",
        client_data.len()
    );

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}
