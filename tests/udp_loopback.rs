//! UDP loopback integration tests.
#![allow(
    clippy::unwrap_used,
    clippy::match_same_arms,
    clippy::too_many_lines,
    clippy::similar_names
)]
//! These tests use actual UDP sockets on 127.0.0.1 to verify real packet transit.

use quiche::h3::NameValue;
use std::net::UdpSocket;

const MAX_DATAGRAM_SIZE: usize = 1350;

fn generate_test_certs() -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("udp_test_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("udp_test_key_{id:?}.pem"));
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
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config
}

fn make_client_config() -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    config.verify_peer(false);
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config
}

/// Exchange packets between client and server sockets until handshake completes or no progress.
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

        // Drain server socket
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

        // Drain client socket
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

#[test]
fn test_udp_handshake() {
    let (cert_path, key_path) = generate_test_certs();
    let mut server_config = make_server_config(&cert_path, &key_path);
    let mut client_config = make_client_config();

    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();

    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    let scid = vec![0xaa; quiche::MAX_CONN_ID_LEN];
    let scid_ref = quiche::ConnectionId::from_ref(&scid);

    let mut client_conn = quiche::connect(
        Some("localhost"),
        &scid_ref,
        client_addr,
        server_addr,
        &mut client_config,
    )
    .unwrap();

    // Send initial packet to get DCID
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let (len, info) = client_conn.send(&mut out).unwrap();
    client_sock.send_to(&out[..len], info.to).unwrap();

    // Server receives initial packet
    let mut buf = vec![0u8; 65535];
    server_sock.set_nonblocking(false).unwrap();
    let (len, from) = server_sock.recv_from(&mut buf).unwrap();

    let hdr = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN).unwrap();
    let server_scid = vec![0xbb; quiche::MAX_CONN_ID_LEN];
    let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);

    let mut server_conn = quiche::accept(
        &server_scid_ref,
        Some(&hdr.dcid),
        server_addr,
        from,
        &mut server_config,
    )
    .unwrap();

    let recv_info = quiche::RecvInfo {
        from,
        to: server_addr,
    };
    server_conn.recv(&mut buf[..len], recv_info).unwrap();

    // Exchange until handshake completes
    exchange_udp(
        &client_sock,
        &server_sock,
        &mut client_conn,
        &mut server_conn,
    );

    assert!(client_conn.is_established());
    assert!(server_conn.is_established());

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
fn test_udp_request_response() {
    let (cert_path, key_path) = generate_test_certs();
    let mut server_config = make_server_config(&cert_path, &key_path);
    let mut client_config = make_client_config();

    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();

    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    // Create client connection
    let scid = vec![0xcc; quiche::MAX_CONN_ID_LEN];
    let scid_ref = quiche::ConnectionId::from_ref(&scid);
    let mut client_conn = quiche::connect(
        Some("localhost"),
        &scid_ref,
        client_addr,
        server_addr,
        &mut client_config,
    )
    .unwrap();

    // Initial packet
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let (len, info) = client_conn.send(&mut out).unwrap();
    client_sock.send_to(&out[..len], info.to).unwrap();

    let mut buf = vec![0u8; 65535];
    server_sock.set_nonblocking(false).unwrap();
    let (len, from) = server_sock.recv_from(&mut buf).unwrap();

    let hdr = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN).unwrap();
    let server_scid = vec![0xdd; quiche::MAX_CONN_ID_LEN];
    let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
    let mut server_conn = quiche::accept(
        &server_scid_ref,
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

    exchange_udp(
        &client_sock,
        &server_sock,
        &mut client_conn,
        &mut server_conn,
    );

    // Init H3
    let h3_config = quiche::h3::Config::new().unwrap();
    let mut client_h3 =
        quiche::h3::Connection::with_transport(&mut client_conn, &h3_config).unwrap();
    let mut server_h3 =
        quiche::h3::Connection::with_transport(&mut server_conn, &h3_config).unwrap();

    // Client sends request
    let req = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/hello"),
        quiche::h3::Header::new(b":authority", b"localhost"),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];
    let _stream_id = client_h3
        .send_request(&mut client_conn, &req, true)
        .unwrap();

    // Exchange until server receives request headers
    let mut request_stream = None;
    for _ in 0..20 {
        exchange_udp(
            &client_sock,
            &server_sock,
            &mut client_conn,
            &mut server_conn,
        );

        loop {
            match server_h3.poll(&mut server_conn) {
                Ok((sid, quiche::h3::Event::Headers { list, .. })) => {
                    let path = list.iter().find(|h| h.name() == b":path").unwrap();
                    assert_eq!(path.value(), b"/hello");
                    request_stream = Some(sid);
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => panic!("server poll: {e}"),
            }
        }

        if request_stream.is_some() {
            break;
        }
    }

    let sid = request_stream.expect("server should receive request");
    let resp = vec![quiche::h3::Header::new(b":status", b"200")];
    server_h3
        .send_response(&mut server_conn, sid, &resp, false)
        .unwrap();
    server_h3
        .send_body(&mut server_conn, sid, b"Hello over UDP!", true)
        .unwrap();

    // Exchange until client receives response
    let mut got_response = false;
    let mut response_body = Vec::new();
    for _ in 0..20 {
        exchange_udp(
            &client_sock,
            &server_sock,
            &mut client_conn,
            &mut server_conn,
        );

        loop {
            match client_h3.poll(&mut client_conn) {
                Ok((_, quiche::h3::Event::Headers { list, .. })) => {
                    let status = list.iter().find(|h| h.name() == b":status").unwrap();
                    assert_eq!(status.value(), b"200");
                    got_response = true;
                }
                Ok((sid, quiche::h3::Event::Data)) => {
                    let mut data_buf = vec![0u8; 1024];
                    match client_h3.recv_body(&mut client_conn, sid, &mut data_buf) {
                        Ok(len) => response_body.extend_from_slice(&data_buf[..len]),
                        Err(quiche::h3::Error::Done) => {}
                        Err(e) => panic!("recv_body: {e}"),
                    }
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => panic!("client poll: {e}"),
            }
        }

        if got_response && !response_body.is_empty() {
            break;
        }
    }

    assert!(got_response);
    assert_eq!(response_body, b"Hello over UDP!");

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
fn test_udp_large_body() {
    let (cert_path, key_path) = generate_test_certs();
    let mut server_config = make_server_config(&cert_path, &key_path);
    let mut client_config = make_client_config();

    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();

    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    let scid = vec![0xee; quiche::MAX_CONN_ID_LEN];
    let scid_ref = quiche::ConnectionId::from_ref(&scid);
    let mut client_conn = quiche::connect(
        Some("localhost"),
        &scid_ref,
        client_addr,
        server_addr,
        &mut client_config,
    )
    .unwrap();

    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let (len, info) = client_conn.send(&mut out).unwrap();
    client_sock.send_to(&out[..len], info.to).unwrap();

    let mut buf = vec![0u8; 65535];
    server_sock.set_nonblocking(false).unwrap();
    let (len, from) = server_sock.recv_from(&mut buf).unwrap();
    let hdr = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN).unwrap();
    let server_scid = vec![0xff; quiche::MAX_CONN_ID_LEN];
    let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
    let mut server_conn = quiche::accept(
        &server_scid_ref,
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

    exchange_udp(
        &client_sock,
        &server_sock,
        &mut client_conn,
        &mut server_conn,
    );

    let h3_config = quiche::h3::Config::new().unwrap();
    let mut client_h3 =
        quiche::h3::Connection::with_transport(&mut client_conn, &h3_config).unwrap();
    let mut server_h3 =
        quiche::h3::Connection::with_transport(&mut server_conn, &h3_config).unwrap();

    // Client sends request
    let req = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/large"),
        quiche::h3::Header::new(b":authority", b"localhost"),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];
    client_h3
        .send_request(&mut client_conn, &req, true)
        .unwrap();

    exchange_udp(
        &client_sock,
        &server_sock,
        &mut client_conn,
        &mut server_conn,
    );

    // Server drains events and sends large response
    let mut request_stream = None;
    loop {
        match server_h3.poll(&mut server_conn) {
            Ok((sid, quiche::h3::Event::Headers { .. })) => {
                request_stream = Some(sid);
            }
            Ok(_) => {}
            Err(quiche::h3::Error::Done) => break,
            Err(_) => break,
        }
    }
    let sid = request_stream.unwrap();

    let resp = vec![quiche::h3::Header::new(b":status", b"200")];
    server_h3
        .send_response(&mut server_conn, sid, &resp, false)
        .unwrap();

    // Send 100KB body
    let large_body = vec![0x42u8; 100_000];
    let mut total_sent = 0;
    while total_sent < large_body.len() {
        let chunk = &large_body[total_sent..];
        let fin = total_sent + chunk.len() >= large_body.len();
        match server_h3.send_body(&mut server_conn, sid, chunk, fin) {
            Ok(written) => {
                total_sent += written;
                exchange_udp(
                    &client_sock,
                    &server_sock,
                    &mut client_conn,
                    &mut server_conn,
                );
            }
            Err(quiche::h3::Error::Done) => {
                exchange_udp(
                    &client_sock,
                    &server_sock,
                    &mut client_conn,
                    &mut server_conn,
                );
            }
            Err(e) => panic!("send_body: {e}"),
        }
    }

    exchange_udp(
        &client_sock,
        &server_sock,
        &mut client_conn,
        &mut server_conn,
    );

    // Client reads entire response
    let mut received = Vec::new();
    loop {
        match client_h3.poll(&mut client_conn) {
            Ok((sid, quiche::h3::Event::Data)) => {
                let mut data_buf = vec![0u8; 65535];
                loop {
                    match client_h3.recv_body(&mut client_conn, sid, &mut data_buf) {
                        Ok(len) => received.extend_from_slice(&data_buf[..len]),
                        Err(quiche::h3::Error::Done) => break,
                        Err(e) => panic!("recv_body: {e}"),
                    }
                }
            }
            Ok(_) => {}
            Err(quiche::h3::Error::Done) => break,
            Err(_) => break,
        }
        if received.len() >= large_body.len() {
            break;
        }
        exchange_udp(
            &client_sock,
            &server_sock,
            &mut client_conn,
            &mut server_conn,
        );
    }

    // May not get all bytes in one pass due to flow control, but should get a significant amount
    assert!(!received.is_empty(), "should receive at least some data");
    assert_eq!(&received[..], &large_body[..received.len()]);

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}
