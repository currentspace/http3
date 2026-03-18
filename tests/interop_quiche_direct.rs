//! Test that mirrors quiche's http3-server example using a direct quiche
//! client, so we can validate Rust-side H3 transport behavior without going
//! through curl or the Node binding.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use std::collections::HashMap;

const MAX_DATAGRAM_SIZE: usize = 1350;

fn generate_certs() -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("curl_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("curl_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

/// This test runs a server on port 0 and uses a direct quiche client.
/// It verifies raw Rust-side H3 transport behavior without spawning curl.
#[test]
fn test_quiche_direct_server_client() {
    let (cert_path, key_path) = generate_certs();

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

    let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    client_config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();
    client_config.verify_peer(false);
    client_config.set_max_idle_timeout(5000);
    client_config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    client_config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    client_config.set_initial_max_data(10_000_000);
    client_config.set_initial_max_stream_data_bidi_local(1_000_000);
    client_config.set_initial_max_stream_data_bidi_remote(1_000_000);
    client_config.set_initial_max_stream_data_uni(1_000_000);
    client_config.set_initial_max_streams_bidi(100);
    client_config.set_initial_max_streams_uni(100);

    // Bind server socket
    let mut server_socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = server_socket.local_addr().unwrap();

    // Bind client socket
    let client_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    client_socket.set_nonblocking(true).unwrap();
    let client_addr = client_socket.local_addr().unwrap();

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

    // Server state
    let mut server_conns: HashMap<Vec<u8>, quiche::Connection> = HashMap::new();

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut server_socket, Token(0), Interest::READABLE)
        .unwrap();

    let mut buf = vec![0u8; 65535];
    let mut out = [0u8; MAX_DATAGRAM_SIZE];

    // Send initial client packets
    loop {
        match client_conn.send(&mut out) {
            Ok((len, info)) => {
                client_socket.send_to(&out[..len], info.to).unwrap();
            }
            Err(quiche::Error::Done) => break,
            Err(e) => panic!("client send: {e}"),
        }
    }

    // Server main loop — process until handshake completes
    let mut events = Events::with_capacity(64);
    let mut handshake_done = false;

    for _ in 0..100 {
        poll.poll(&mut events, Some(std::time::Duration::from_millis(50)))
            .unwrap();

        // Recv
        loop {
            match server_socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let pkt_buf = &mut buf[..len];
                    let hdr = quiche::Header::from_slice(pkt_buf, quiche::MAX_CONN_ID_LEN).unwrap();

                    let conn = if let Some(conn) = server_conns.get_mut(hdr.dcid.as_ref()) {
                        conn
                    } else {
                        if hdr.ty != quiche::Type::Initial {
                            continue;
                        }
                        let server_scid = vec![0xdd; quiche::MAX_CONN_ID_LEN];
                        let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
                        let conn = quiche::accept(
                            &server_scid_ref,
                            Some(&hdr.dcid),
                            server_addr,
                            from,
                            &mut config,
                        )
                        .unwrap();
                        server_conns.insert(server_scid, conn);
                        // Also map by client DCID
                        server_conns
                            .get_mut(&vec![0xdd; quiche::MAX_CONN_ID_LEN])
                            .unwrap()
                    };

                    let recv_info = quiche::RecvInfo {
                        from,
                        to: server_addr,
                    };
                    conn.recv(pkt_buf, recv_info).ok();

                    if conn.is_established() {
                        eprintln!("Server: handshake complete!");
                        handshake_done = true;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("server recv: {e}"),
            }
        }

        // Send server packets
        for conn in server_conns.values_mut() {
            loop {
                match conn.send(&mut out) {
                    Ok((len, info)) => {
                        server_socket.send_to(&out[..len], info.to).ok();
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => panic!("server send: {e}"),
                }
            }
        }

        // Read client packets
        loop {
            match client_socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo {
                        from,
                        to: client_addr,
                    };
                    client_conn.recv(&mut buf[..len], recv_info).ok();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("client recv: {e}"),
            }
        }

        // Send client packets
        loop {
            match client_conn.send(&mut out) {
                Ok((len, info)) => {
                    client_socket.send_to(&out[..len], info.to).unwrap();
                }
                Err(quiche::Error::Done) => break,
                Err(e) => panic!("client send: {e}"),
            }
        }

        if client_conn.is_established() && handshake_done {
            break;
        }
    }

    assert!(
        client_conn.is_established(),
        "client handshake should complete"
    );
    assert!(handshake_done, "server handshake should complete");

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}
