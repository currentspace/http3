#![allow(clippy::unwrap_used)]
use std::net::SocketAddr;
fn config(cap: usize, discover: bool) -> quiche::Config {
    let mut c = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    c.set_application_protos(&[b"h3"]).unwrap();
    c.verify_peer(false);
    c.set_max_send_udp_payload_size(cap);
    c.set_max_recv_udp_payload_size(cap);
    c.set_initial_max_data(10_000_000);
    c.set_initial_max_stream_data_bidi_local(10_000_000);
    c.set_initial_max_stream_data_bidi_remote(10_000_000);
    c.set_initial_max_streams_bidi(100);
    c.set_initial_congestion_window_packets(1000);
    c.discover_pmtu(discover);
    c.set_pmtud_max_probes(1);
    c
}
#[test]
fn pmtud_respects_local_and_peer_udp_limits() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = std::env::temp_dir().join(format!("http3-pmtud-{}.crt", std::process::id()));
    let key_path = cert_path.with_extension("key");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
    for (cap, peer_cap) in [
        (16356, 1472),
        (1472, 1200),
        (1472, 1350),
        (1200, 1472),
        (1472, 65527),
    ] {
        let discover = true;
        let mut client_config = config(peer_cap, false);
        let mut server_config = config(cap, discover);
        server_config
            .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
            .unwrap();
        server_config
            .load_priv_key_from_pem_file(key_path.to_str().unwrap())
            .unwrap();
        let client_addr: SocketAddr = "127.0.0.1:11111".parse().unwrap();
        let server_addr: SocketAddr = "127.0.0.1:22222".parse().unwrap();
        let cid = quiche::ConnectionId::from_ref(&[1; 16]);
        let sid = quiche::ConnectionId::from_ref(&[2; 16]);
        let mut client = quiche::connect(
            Some("localhost"),
            &cid,
            client_addr,
            server_addr,
            &mut client_config,
        )
        .unwrap();
        let mut server =
            quiche::accept(&sid, None, server_addr, client_addr, &mut server_config).unwrap();
        let mut b = vec![0; 65535];
        let mut sizes = vec![];
        let mut queued = false;
        let mut received = Vec::new();
        for _ in 0..30 {
            while let Ok((len, _)) = client.send(&mut b) {
                server
                    .recv(
                        &mut b[..len],
                        quiche::RecvInfo {
                            from: client_addr,
                            to: server_addr,
                        },
                    )
                    .unwrap();
            }
            if client.is_established() && server.is_established() && !queued {
                client.stream_send(0, b"GET", true).unwrap();
                server.stream_send(1, &vec![b'x'; 50000], true).unwrap();
                queued = true;
            }
            while let Ok((len, _)) = server.send(&mut b) {
                sizes.push(len);
                client
                    .recv(
                        &mut b[..len],
                        quiche::RecvInfo {
                            from: server_addr,
                            to: client_addr,
                        },
                    )
                    .unwrap();
            }
            while let Ok((len, _)) = client.stream_recv(1, &mut b) {
                received.extend_from_slice(&b[..len]);
            }
        }
        assert_eq!(received, vec![b'x'; 50000]);
        assert!(queued && client.is_established() && server.is_established());
        assert!(sizes.len() > 3, "must exercise application data and probes");
        assert!(
            sizes.iter().all(|n| *n <= cap.min(peer_cap)),
            "local={cap}, peer={peer_cap}, packets={sizes:?}"
        );
    }
    std::fs::remove_file(cert_path).unwrap();
    std::fs::remove_file(key_path).unwrap();
}
