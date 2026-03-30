//! Connection churn stress tests using real UDP sockets.
//! All tests are `#[ignore]` so they do not run in CI.
//! Run with: cargo test --test stress_connection_churn --features bench-internals --no-default-features -- --ignored
#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    let cert_path = std::env::temp_dir().join(format!("stress_churn_cert_{id:?}_{ts}.pem"));
    let key_path = std::env::temp_dir().join(format!("stress_churn_key_{id:?}_{ts}.pem"));
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

/// Bidirectional packet exchange while also draining stream_recv on both sides.
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

    // Server stream drain
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

    // Client stream drain
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

/// Full lifecycle: send 1KB, server echoes, client verifies.
fn echo_stream(pair: &mut UdpQuicPair, stream_id: u64, payload: &[u8]) -> bool {
    let mut recv_buf = vec![0u8; 65535];
    let mut server_data = Vec::new();
    let mut server_fin = false;
    let mut client_data = Vec::new();
    let mut client_fin = false;

    // Client sends
    if pair
        .client_conn
        .stream_send(stream_id, payload, true)
        .is_err()
    {
        return false;
    }

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
    if !server_fin {
        return false;
    }

    // Server echoes
    if pair
        .server_conn
        .stream_send(stream_id, &server_data, true)
        .is_err()
    {
        return false;
    }

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

    client_fin && client_data.len() == payload.len()
}

// ── Single connection churn cycle ───────────────────────────────────

fn churn_one_connection(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
    scid_byte: u8,
) -> bool {
    let mut server_config = make_server_config(cert_path, key_path);
    let mut client_config = make_client_config();
    let mut pair = setup_udp_quic_pair(&mut server_config, &mut client_config, scid_byte);

    let payload = vec![0xDD_u8; 1024]; // 1KB
    echo_stream(&mut pair, 0, &payload)
}

// ===========================================================================
// Tests
// ===========================================================================

const TEST_DURATION: Duration = Duration::from_secs(300); // 5 minutes

#[test]
#[ignore]
fn test_connection_churn_5_minutes() {
    let (cert_path, key_path) = generate_test_certs();
    let start = Instant::now();
    let mut success_count: u64 = 0;
    let mut attempt: u64 = 0;

    while start.elapsed() < TEST_DURATION {
        let scid_byte = (attempt % 256) as u8;
        if churn_one_connection(&cert_path, &key_path, scid_byte) {
            success_count += 1;
        }
        attempt += 1;
    }

    let elapsed = start.elapsed();
    eprintln!(
        "connection churn: {success_count}/{attempt} successful in {:.1}s",
        elapsed.as_secs_f64()
    );

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    assert!(
        success_count > 50,
        "expected > 50 successful connection cycles, got {success_count}"
    );
}

#[test]
#[ignore]
fn test_parallel_connection_churn_5_minutes() {
    let (cert_path, key_path) = generate_test_certs();
    let total_success = Arc::new(AtomicU64::new(0));
    let total_attempts = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..4u8)
        .map(|thread_idx| {
            let cert = cert_path.clone();
            let key = key_path.clone();
            let success = Arc::clone(&total_success);
            let attempts = Arc::clone(&total_attempts);

            std::thread::spawn(move || {
                let start = Instant::now();
                let mut local_success: u64 = 0;
                let mut local_attempt: u64 = 0;

                while start.elapsed() < TEST_DURATION {
                    // Unique scid_byte per thread + attempt
                    let scid_byte =
                        thread_idx.wrapping_mul(64).wrapping_add((local_attempt % 64) as u8);
                    if churn_one_connection(&cert, &key, scid_byte) {
                        local_success += 1;
                    }
                    local_attempt += 1;
                }

                success.fetch_add(local_success, Ordering::Relaxed);
                attempts.fetch_add(local_attempt, Ordering::Relaxed);
                eprintln!(
                    "  thread {thread_idx}: {local_success}/{local_attempt} successful"
                );
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join()
            .unwrap_or_else(|e| panic!("thread {i} panicked: {e:?}"));
    }

    let final_success = total_success.load(Ordering::Relaxed);
    let final_attempts = total_attempts.load(Ordering::Relaxed);

    eprintln!(
        "parallel connection churn: {final_success}/{final_attempts} total successful across 4 threads"
    );

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    assert!(
        final_success > 100,
        "expected > 100 total successful connection cycles across 4 threads, got {final_success}"
    );
}
