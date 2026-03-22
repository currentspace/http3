//! Standalone H3 server test with curl.
//! Handles the ENTIRE H3 cycle (handshake + response) in a single Rust function.
//! No JS, no command channel. This isolates quiche API usage from JS timing.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::struct_field_names,
    clippy::manual_let_else,
    clippy::range_plus_one,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::match_same_arms
)]

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use quiche::h3::NameValue;
use std::collections::HashMap;
use std::process::Command;
use std::sync::OnceLock;
const MAX_DATAGRAM_SIZE: usize = 1350;

fn has_curl_http3() -> bool {
    static HAS_HTTP3: OnceLock<bool> = OnceLock::new();
    *HAS_HTTP3.get_or_init(|| {
        let Ok(output) = Command::new("curl").arg("--version").output() else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let version = String::from_utf8_lossy(&output.stdout);
        version.contains("HTTP3") || version.contains("http3")
    })
}

fn generate_certs() -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".try_into().unwrap()),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    let cert = params.self_signed(&key_pair).unwrap();

    let id = std::thread::current().id();
    let cert_path = std::env::temp_dir().join(format!("curl_standalone_cert_{id:?}.pem"));
    let key_path = std::env::temp_dir().join(format!("curl_standalone_key_{id:?}.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

/// Run a standalone HTTP/3 server that handles ONE request from curl.
/// Modeled exactly after the quiche http3-server example.
#[test]
fn test_standalone_h3_server_with_curl() {
    if !has_curl_http3() {
        eprintln!("skipping standalone curl test: curl with HTTP/3 support not available");
        return;
    }
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
    config.set_disable_active_migration(true);

    let mut socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = socket.local_addr().unwrap();
    eprintln!("[server] Listening on {server_addr}");

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut socket, Token(0), Interest::READABLE)
        .unwrap();

    // Spawn curl
    let curl = Command::new("curl")
        .args([
            "--http3-only",
            "-k",
            "-s",
            "-o",
            "-",
            "-D",
            "/dev/stderr",
            "--max-time",
            "5",
            &format!("https://127.0.0.1:{}/", server_addr.port()),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("curl must be installed with HTTP/3 support");

    // Connection state
    struct Conn {
        quiche_conn: quiche::Connection,
        h3_conn: Option<quiche::h3::Connection>,
        responded: std::collections::HashSet<u64>,
    }

    // conn_id (server SCID) -> Conn
    let mut conns: HashMap<Vec<u8>, Conn> = HashMap::new();
    // dcid -> server_scid mapping (for routing by client's original DCID)
    let mut dcid_to_scid: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    let mut events = Events::with_capacity(256);
    let mut recv_buf = vec![0u8; 65535];
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let mut done = false;
    let response_body = b"Hello from standalone H3 server!\n";
    let mut scid_counter: u8 = 0;

    for iteration in 0..500 {
        if done {
            break;
        }

        // Compute poll timeout from connection timeouts
        let timeout = conns
            .values()
            .filter_map(|c| c.quiche_conn.timeout())
            .min()
            .unwrap_or(std::time::Duration::from_millis(100));

        poll.poll(&mut events, Some(timeout)).unwrap();

        // Process expired timers BEFORE recv (like quiche example)
        for sc in conns.values_mut() {
            if sc.quiche_conn.timeout() == Some(std::time::Duration::ZERO) {
                sc.quiche_conn.on_timeout();
            }
        }

        // Drain ALL incoming packets
        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, peer)) => {
                    let pkt = &mut recv_buf[..len];
                    let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("[server] iter={iteration} Header parse error: {e}");
                            continue;
                        }
                    };

                    eprintln!(
                        "[server] iter={iteration} RECV {len}B type={:?} dcid={} scid={}",
                        hdr.ty,
                        hex(&hdr.dcid),
                        hex(&hdr.scid),
                    );

                    // Route: try DCID directly, then via dcid_to_scid mapping
                    let scid_key = if conns.contains_key(hdr.dcid.as_ref()) {
                        hdr.dcid.as_ref().to_vec()
                    } else if let Some(scid) = dcid_to_scid.get(hdr.dcid.as_ref()) {
                        scid.clone()
                    } else {
                        // New connection
                        if hdr.ty != quiche::Type::Initial {
                            eprintln!("[server]   non-Initial for unknown DCID, dropping");
                            continue;
                        }

                        // Generate unique server SCID
                        let mut server_scid = vec![0xaa; quiche::MAX_CONN_ID_LEN];
                        server_scid[0] = scid_counter;
                        scid_counter = scid_counter.wrapping_add(1);
                        let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);

                        // NOTE: odcid must be None when not using retry.
                        // Setting it incorrectly adds original_destination_connection_id
                        // transport parameter, which confuses the client.
                        let quiche_conn = quiche::accept(
                            &server_scid_ref,
                            None, // No retry, so no original DCID
                            server_addr,
                            peer,
                            &mut config,
                        )
                        .unwrap();

                        eprintln!(
                            "[server]   NEW connection scid={} odcid={}",
                            hex(&server_scid),
                            hex(&hdr.dcid),
                        );

                        // Map client's original DCID for routing retransmits
                        let client_dcid = hdr.dcid.as_ref().to_vec();
                        dcid_to_scid.insert(client_dcid, server_scid.clone());

                        conns.insert(
                            server_scid.clone(),
                            Conn {
                                quiche_conn,
                                h3_conn: None,
                                responded: std::collections::HashSet::new(),
                            },
                        );
                        server_scid
                    };

                    let sc = conns.get_mut(&scid_key).unwrap();
                    let recv_info = quiche::RecvInfo {
                        from: peer,
                        to: server_addr,
                    };

                    match sc.quiche_conn.recv(pkt, recv_info) {
                        Ok(n) => eprintln!(
                            "[server]   recv() OK {n}B, established={}, early_data={}",
                            sc.quiche_conn.is_established(),
                            sc.quiche_conn.is_in_early_data(),
                        ),
                        Err(e) => {
                            eprintln!("[server]   recv() error: {e}");
                            continue;
                        }
                    }

                    // Init H3 when handshake completes
                    if sc.quiche_conn.is_established() && sc.h3_conn.is_none() {
                        eprintln!("[server]   HANDSHAKE COMPLETE — initializing H3");
                        let h3_config = quiche::h3::Config::new().unwrap();
                        let h3_conn =
                            quiche::h3::Connection::with_transport(&mut sc.quiche_conn, &h3_config)
                                .unwrap();
                        sc.h3_conn = Some(h3_conn);
                    }

                    // Poll H3 events and respond inline
                    if let Some(h3) = sc.h3_conn.as_mut() {
                        loop {
                            match h3.poll(&mut sc.quiche_conn) {
                                Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                                    eprintln!("[server]   H3 HEADERS on stream {stream_id}:");
                                    for hdr in &list {
                                        eprintln!(
                                            "[server]     {}: {}",
                                            String::from_utf8_lossy(hdr.name()),
                                            String::from_utf8_lossy(hdr.value()),
                                        );
                                    }

                                    if !sc.responded.contains(&stream_id) {
                                        sc.responded.insert(stream_id);

                                        let resp_headers = vec![
                                            quiche::h3::Header::new(b":status", b"200"),
                                            quiche::h3::Header::new(b"content-type", b"text/plain"),
                                            quiche::h3::Header::new(
                                                b"content-length",
                                                response_body.len().to_string().as_bytes(),
                                            ),
                                        ];

                                        match h3.send_response(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            &resp_headers,
                                            false,
                                        ) {
                                            Ok(()) => eprintln!(
                                                "[server]   SENT response headers on stream {stream_id}"
                                            ),
                                            Err(e) => {
                                                eprintln!("[server]   send_response ERROR: {e}")
                                            }
                                        }

                                        match h3.send_body(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            response_body,
                                            true,
                                        ) {
                                            Ok(written) => eprintln!(
                                                "[server]   SENT {written}/{} body bytes with FIN",
                                                response_body.len()
                                            ),
                                            Err(e) => eprintln!("[server]   send_body ERROR: {e}"),
                                        }
                                    }
                                }
                                Ok((stream_id, quiche::h3::Event::Data)) => {
                                    let mut data_buf = [0u8; 4096];
                                    loop {
                                        match h3.recv_body(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            &mut data_buf,
                                        ) {
                                            Ok(n) => eprintln!(
                                                "[server]   H3 DATA stream {stream_id}: {n}B"
                                            ),
                                            Err(quiche::h3::Error::Done) => break,
                                            Err(e) => {
                                                eprintln!("[server]   recv_body error: {e}");
                                                break;
                                            }
                                        }
                                    }
                                }
                                Ok((stream_id, quiche::h3::Event::Finished)) => {
                                    eprintln!("[server]   H3 FINISHED stream {stream_id}");
                                    if sc.responded.contains(&stream_id) {
                                        done = true;
                                    }
                                }
                                Ok((_, ev)) => {
                                    eprintln!("[server]   H3 event: {ev:?}");
                                }
                                Err(quiche::h3::Error::Done) => break,
                                Err(e) => {
                                    eprintln!("[server]   H3 poll error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("recv error: {e}"),
            }
        }

        // Flush ALL outbound packets
        let mut total_sent = 0;
        for sc in conns.values_mut() {
            loop {
                match sc.quiche_conn.send(&mut out) {
                    Ok((len, send_info)) => {
                        eprintln!("[server] iter={iteration} SEND {len}B to {}", send_info.to);
                        let _ = socket.send_to(&out[..len], send_info.to);
                        total_sent += 1;
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => {
                        eprintln!("[server] send error: {e}");
                        break;
                    }
                }
            }
        }
        if total_sent > 0 || iteration < 10 {
            eprintln!("[server] iter={iteration} flushed {total_sent} packets");
        }
    }

    let output = curl.wait_with_output().expect("curl should finish");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("\n=== CURL STDOUT ===\n{stdout}");
    eprintln!("=== CURL STDERR ===\n{stderr}");
    eprintln!("=== CURL EXIT CODE: {:?} ===", output.status.code());

    assert!(
        output.status.success(),
        "curl should succeed, stderr: {stderr}"
    );
    assert_eq!(
        stdout.trim(),
        "Hello from standalone H3 server!",
        "curl should receive the response body"
    );

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

/// Same test but WITH stateless retry (matching worker thread mode).
#[test]
fn test_standalone_h3_server_with_curl_retry() {
    if !has_curl_http3() {
        eprintln!("skipping standalone curl retry test: curl with HTTP/3 support not available");
        return;
    }
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
    config.set_disable_active_migration(true);

    let mut socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = socket.local_addr().unwrap();
    eprintln!("[retry-server] Listening on {server_addr}");

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut socket, Token(0), Interest::READABLE)
        .unwrap();

    let curl = Command::new("curl")
        .args([
            "--http3-only",
            "-k",
            "-s",
            "-o",
            "-",
            "-D",
            "/dev/stderr",
            "--max-time",
            "5",
            &format!("https://127.0.0.1:{}/", server_addr.port()),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("curl must be installed with HTTP/3 support");

    struct Conn {
        quiche_conn: quiche::Connection,
        h3_conn: Option<quiche::h3::Connection>,
        responded: std::collections::HashSet<u64>,
    }

    let mut conns: HashMap<Vec<u8>, Conn> = HashMap::new();
    let mut dcid_to_scid: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

    // Simple token: just the client's original DCID + peer address
    // (In production, use HMAC. For testing, this is sufficient.)
    fn mint_token(odcid: &[u8], peer: &std::net::SocketAddr) -> Vec<u8> {
        let mut token = b"quiche_test".to_vec();
        token.push(odcid.len() as u8);
        token.extend_from_slice(odcid);
        match peer {
            std::net::SocketAddr::V4(v4) => {
                token.push(4);
                token.extend_from_slice(&v4.ip().octets());
                token.extend_from_slice(&v4.port().to_be_bytes());
            }
            std::net::SocketAddr::V6(v6) => {
                token.push(6);
                token.extend_from_slice(&v6.ip().octets());
                token.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        token
    }

    fn validate_token(token: &[u8], peer: &std::net::SocketAddr) -> Option<Vec<u8>> {
        if !token.starts_with(b"quiche_test") {
            return None;
        }
        let rest = &token[b"quiche_test".len()..];
        if rest.is_empty() {
            return None;
        }
        let odcid_len = rest[0] as usize;
        if rest.len() < 1 + odcid_len + 1 {
            return None;
        }
        let odcid = rest[1..1 + odcid_len].to_vec();
        let addr_data = &rest[1 + odcid_len..];
        // Verify peer address matches
        match (addr_data.first(), peer) {
            (Some(4), std::net::SocketAddr::V4(v4)) => {
                if addr_data.len() < 7 {
                    return None;
                }
                if addr_data[1..5] != v4.ip().octets() {
                    return None;
                }
                if addr_data[5..7] != v4.port().to_be_bytes() {
                    return None;
                }
            }
            _ => return None,
        }
        Some(odcid)
    }

    let mut events = Events::with_capacity(256);
    let mut recv_buf = vec![0u8; 65535];
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let mut done = false;
    let response_body = b"Hello from standalone H3 server with retry!\n";
    let mut scid_counter: u8 = 0;

    for iteration in 0..500 {
        if done {
            break;
        }

        let timeout = conns
            .values()
            .filter_map(|c| c.quiche_conn.timeout())
            .min()
            .unwrap_or(std::time::Duration::from_millis(100));

        poll.poll(&mut events, Some(timeout)).unwrap();

        // Timeouts
        for sc in conns.values_mut() {
            if sc.quiche_conn.timeout() == Some(std::time::Duration::ZERO) {
                sc.quiche_conn.on_timeout();
            }
        }

        // Drain ALL incoming packets
        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, peer)) => {
                    let pkt = &mut recv_buf[..len];
                    let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
                        Ok(h) => h,
                        Err(_) => continue,
                    };

                    eprintln!(
                        "[retry-server] iter={iteration} RECV {len}B type={:?} dcid={} token_len={}",
                        hdr.ty,
                        hex(&hdr.dcid),
                        hdr.token.as_ref().map_or(0, |t| t.len()),
                    );

                    // Route
                    let scid_key = if conns.contains_key(hdr.dcid.as_ref()) {
                        hdr.dcid.as_ref().to_vec()
                    } else if let Some(scid) = dcid_to_scid.get(hdr.dcid.as_ref()) {
                        scid.clone()
                    } else {
                        if hdr.ty != quiche::Type::Initial {
                            continue;
                        }

                        // Check for retry token
                        if let Some(token) = hdr.token.as_ref().filter(|t| !t.is_empty()) {
                            // Validate token
                            let Some(odcid) = validate_token(token, &peer) else {
                                eprintln!("[retry-server]   invalid token, dropping");
                                continue;
                            };

                            // After retry: use hdr.dcid as our SCID (it's the SCID
                            // we sent in the Retry packet, which client echoed back)
                            let server_scid = hdr.dcid.as_ref().to_vec();
                            let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
                            let odcid_ref = quiche::ConnectionId::from_ref(&odcid);

                            let quiche_conn = quiche::accept(
                                &server_scid_ref,
                                Some(&odcid_ref), // After retry, odcid is correct
                                server_addr,
                                peer,
                                &mut config,
                            )
                            .unwrap();

                            eprintln!(
                                "[retry-server]   ACCEPTED after retry, scid={} odcid={}",
                                hex(&server_scid),
                                hex(&odcid),
                            );

                            dcid_to_scid.insert(odcid.clone(), server_scid.clone());

                            conns.insert(
                                server_scid.clone(),
                                Conn {
                                    quiche_conn,
                                    h3_conn: None,
                                    responded: std::collections::HashSet::new(),
                                },
                            );
                            server_scid
                        } else {
                            // No token — send Retry
                            let mut retry_scid = vec![0xbb; quiche::MAX_CONN_ID_LEN];
                            retry_scid[0] = scid_counter;
                            scid_counter = scid_counter.wrapping_add(1);
                            let retry_scid_ref = quiche::ConnectionId::from_ref(&retry_scid);

                            let token = mint_token(hdr.dcid.as_ref(), &peer);
                            let mut retry_out = vec![0u8; 65535];
                            if let Ok(len) = quiche::retry(
                                &hdr.scid,
                                &hdr.dcid,
                                &retry_scid_ref,
                                &token,
                                hdr.version,
                                &mut retry_out,
                            ) {
                                eprintln!(
                                    "[retry-server]   RETRY sent {len}B, retry_scid={}",
                                    hex(&retry_scid),
                                );
                                let _ = socket.send_to(&retry_out[..len], peer);
                            }
                            continue;
                        }
                    };

                    let sc = conns.get_mut(&scid_key).unwrap();
                    let recv_info = quiche::RecvInfo {
                        from: peer,
                        to: server_addr,
                    };

                    match sc.quiche_conn.recv(pkt, recv_info) {
                        Ok(n) => eprintln!(
                            "[retry-server]   recv() OK {n}B, established={}",
                            sc.quiche_conn.is_established(),
                        ),
                        Err(e) => {
                            eprintln!("[retry-server]   recv() error: {e}");
                            continue;
                        }
                    }

                    // Init H3
                    if sc.quiche_conn.is_established() && sc.h3_conn.is_none() {
                        eprintln!("[retry-server]   HANDSHAKE COMPLETE — initializing H3");
                        let h3_config = quiche::h3::Config::new().unwrap();
                        let h3_conn =
                            quiche::h3::Connection::with_transport(&mut sc.quiche_conn, &h3_config)
                                .unwrap();
                        sc.h3_conn = Some(h3_conn);
                    }

                    // Poll H3 and respond
                    if let Some(h3) = sc.h3_conn.as_mut() {
                        loop {
                            match h3.poll(&mut sc.quiche_conn) {
                                Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                                    eprintln!("[retry-server]   H3 HEADERS on stream {stream_id}");
                                    for hdr in &list {
                                        eprintln!(
                                            "[retry-server]     {}: {}",
                                            String::from_utf8_lossy(hdr.name()),
                                            String::from_utf8_lossy(hdr.value()),
                                        );
                                    }

                                    if !sc.responded.contains(&stream_id) {
                                        sc.responded.insert(stream_id);
                                        let resp_headers = vec![
                                            quiche::h3::Header::new(b":status", b"200"),
                                            quiche::h3::Header::new(b"content-type", b"text/plain"),
                                            quiche::h3::Header::new(
                                                b"content-length",
                                                response_body.len().to_string().as_bytes(),
                                            ),
                                        ];

                                        match h3.send_response(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            &resp_headers,
                                            false,
                                        ) {
                                            Ok(()) => {
                                                eprintln!("[retry-server]   SENT response headers")
                                            }
                                            Err(e) => eprintln!(
                                                "[retry-server]   send_response ERROR: {e}"
                                            ),
                                        }

                                        match h3.send_body(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            response_body,
                                            true,
                                        ) {
                                            Ok(written) => eprintln!(
                                                "[retry-server]   SENT {written}/{} body with FIN",
                                                response_body.len()
                                            ),
                                            Err(e) => {
                                                eprintln!("[retry-server]   send_body ERROR: {e}")
                                            }
                                        }
                                    }
                                }
                                Ok((stream_id, quiche::h3::Event::Data)) => {
                                    let mut data_buf = [0u8; 4096];
                                    loop {
                                        match h3.recv_body(
                                            &mut sc.quiche_conn,
                                            stream_id,
                                            &mut data_buf,
                                        ) {
                                            Ok(_) => {}
                                            Err(quiche::h3::Error::Done) => break,
                                            Err(_) => break,
                                        }
                                    }
                                }
                                Ok((stream_id, quiche::h3::Event::Finished)) => {
                                    eprintln!("[retry-server]   H3 FINISHED stream {stream_id}");
                                    if sc.responded.contains(&stream_id) {
                                        done = true;
                                    }
                                }
                                Ok((_, _)) => {}
                                Err(quiche::h3::Error::Done) => break,
                                Err(e) => {
                                    eprintln!("[retry-server]   H3 poll error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("recv error: {e}"),
            }
        }

        // Flush
        for sc in conns.values_mut() {
            loop {
                match sc.quiche_conn.send(&mut out) {
                    Ok((len, send_info)) => {
                        eprintln!("[retry-server] iter={iteration} SEND {len}B");
                        let _ = socket.send_to(&out[..len], send_info.to);
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => {
                        eprintln!("[retry-server] send error: {e}");
                        break;
                    }
                }
            }
        }
    }

    let output = curl.wait_with_output().expect("curl should finish");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("\n=== CURL STDOUT ===\n{stdout}");
    eprintln!("=== CURL STDERR ===\n{stderr}");
    eprintln!("=== CURL EXIT CODE: {:?} ===", output.status.code());

    assert!(
        output.status.success(),
        "curl should succeed, stderr: {stderr}"
    );
    assert_eq!(
        stdout.trim(),
        "Hello from standalone H3 server with retry!",
        "curl should receive the response body"
    );

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

/// Test with DELAYED response — response sent in a later iteration,
/// simulating the JS command channel round-trip.
#[test]
fn test_standalone_h3_delayed_response() {
    if !has_curl_http3() {
        eprintln!("skipping standalone curl delayed-response test: curl with HTTP/3 support not available");
        return;
    }
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
    config.set_disable_active_migration(true);

    let mut socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = socket.local_addr().unwrap();
    eprintln!("[delayed] Listening on {server_addr}");

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut socket, Token(0), Interest::READABLE)
        .unwrap();

    let curl = Command::new("curl")
        .args([
            "--http3-only",
            "-k",
            "-s",
            "-o",
            "-",
            "-D",
            "/dev/stderr",
            "--max-time",
            "5",
            &format!("https://127.0.0.1:{}/", server_addr.port()),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("curl must be installed");

    struct Conn {
        quiche_conn: quiche::Connection,
        h3_conn: Option<quiche::h3::Connection>,
        pending_response: Option<u64>, // stream_id to respond to later
        responded: bool,
    }

    let mut conns: HashMap<Vec<u8>, Conn> = HashMap::new();
    let mut dcid_to_scid: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut events = Events::with_capacity(256);
    let mut recv_buf = vec![0u8; 65535];
    let mut out = vec![0u8; MAX_DATAGRAM_SIZE];
    let mut done = false;
    let response_body = b"Hello from delayed response server!\n";

    for iteration in 0..500 {
        if done {
            break;
        }

        let timeout = conns
            .values()
            .filter_map(|c| c.quiche_conn.timeout())
            .min()
            .unwrap_or(std::time::Duration::from_millis(100));

        poll.poll(&mut events, Some(timeout)).unwrap();

        for sc in conns.values_mut() {
            if sc.quiche_conn.timeout() == Some(std::time::Duration::ZERO) {
                sc.quiche_conn.on_timeout();
            }
        }

        // FIRST: send pending responses from PREVIOUS iteration (simulates command channel)
        for sc in conns.values_mut() {
            if let Some(stream_id) = sc.pending_response.take() {
                let h3 = sc.h3_conn.as_mut().unwrap();
                let resp_headers = vec![
                    quiche::h3::Header::new(b":status", b"200"),
                    quiche::h3::Header::new(b"content-type", b"text/plain"),
                    quiche::h3::Header::new(
                        b"content-length",
                        response_body.len().to_string().as_bytes(),
                    ),
                ];
                h3.send_response(&mut sc.quiche_conn, stream_id, &resp_headers, false)
                    .unwrap();
                h3.send_body(&mut sc.quiche_conn, stream_id, response_body, true)
                    .unwrap();
                sc.responded = true;
                eprintln!("[delayed] iter={iteration} SENT delayed response on stream {stream_id}");
            }
        }

        // Flush after command processing (like worker step 1b)
        for sc in conns.values_mut() {
            loop {
                match sc.quiche_conn.send(&mut out) {
                    Ok((len, send_info)) => {
                        eprintln!("[delayed] iter={iteration} CMD-FLUSH {len}B");
                        let _ = socket.send_to(&out[..len], send_info.to);
                    }
                    Err(quiche::Error::Done) => break,
                    Err(_) => break,
                }
            }
        }

        // Read incoming
        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, peer)) => {
                    let pkt = &mut recv_buf[..len];
                    let hdr = match quiche::Header::from_slice(pkt, quiche::MAX_CONN_ID_LEN) {
                        Ok(h) => h,
                        Err(_) => continue,
                    };

                    eprintln!("[delayed] iter={iteration} RECV {len}B type={:?}", hdr.ty,);

                    let scid_key = if conns.contains_key(hdr.dcid.as_ref()) {
                        hdr.dcid.as_ref().to_vec()
                    } else if let Some(scid) = dcid_to_scid.get(hdr.dcid.as_ref()) {
                        scid.clone()
                    } else {
                        if hdr.ty != quiche::Type::Initial {
                            continue;
                        }

                        let server_scid = vec![0xcc; quiche::MAX_CONN_ID_LEN];
                        let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
                        let quiche_conn =
                            quiche::accept(&server_scid_ref, None, server_addr, peer, &mut config)
                                .unwrap();

                        let client_dcid = hdr.dcid.as_ref().to_vec();
                        dcid_to_scid.insert(client_dcid, server_scid.clone());
                        conns.insert(
                            server_scid.clone(),
                            Conn {
                                quiche_conn,
                                h3_conn: None,
                                pending_response: None,
                                responded: false,
                            },
                        );
                        server_scid
                    };

                    let sc = conns.get_mut(&scid_key).unwrap();
                    let recv_info = quiche::RecvInfo {
                        from: peer,
                        to: server_addr,
                    };
                    if sc.quiche_conn.recv(pkt, recv_info).is_err() {
                        continue;
                    }

                    if sc.quiche_conn.is_established() && sc.h3_conn.is_none() {
                        eprintln!("[delayed] iter={iteration} HANDSHAKE COMPLETE");
                        let h3_config = quiche::h3::Config::new().unwrap();
                        sc.h3_conn = Some(
                            quiche::h3::Connection::with_transport(&mut sc.quiche_conn, &h3_config)
                                .unwrap(),
                        );
                    }

                    if let Some(h3) = sc.h3_conn.as_mut() {
                        loop {
                            match h3.poll(&mut sc.quiche_conn) {
                                Ok((stream_id, quiche::h3::Event::Headers { .. })) => {
                                    eprintln!(
                                        "[delayed] iter={iteration} H3 HEADERS on stream {stream_id} — DEFERRING response"
                                    );
                                    // Don't respond now! Defer to next iteration.
                                    sc.pending_response = Some(stream_id);
                                }
                                Ok((stream_id, quiche::h3::Event::Finished)) => {
                                    eprintln!(
                                        "[delayed] iter={iteration} H3 FINISHED stream {stream_id}"
                                    );
                                    if sc.responded {
                                        done = true;
                                    }
                                }
                                Ok((_, quiche::h3::Event::Data)) => {
                                    let mut buf = [0u8; 4096];
                                    while h3.recv_body(&mut sc.quiche_conn, 0, &mut buf).is_ok() {}
                                }
                                Ok(_) => {}
                                Err(quiche::h3::Error::Done) => break,
                                Err(_) => break,
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("recv error: {e}"),
            }
        }

        // Flush outbound (like worker step 4)
        for sc in conns.values_mut() {
            loop {
                match sc.quiche_conn.send(&mut out) {
                    Ok((len, send_info)) => {
                        eprintln!("[delayed] iter={iteration} MAIN-FLUSH {len}B");
                        let _ = socket.send_to(&out[..len], send_info.to);
                    }
                    Err(quiche::Error::Done) => break,
                    Err(_) => break,
                }
            }
        }
    }

    let output = curl.wait_with_output().expect("curl should finish");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("\n=== CURL STDOUT ===\n{stdout}");
    eprintln!("=== CURL STDERR ===\n{stderr}");
    eprintln!("=== EXIT CODE: {:?} ===", output.status.code());

    assert!(
        output.status.success(),
        "curl should succeed, stderr: {stderr}"
    );
    assert_eq!(stdout.trim(), "Hello from delayed response server!",);

    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

fn hex(data: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
