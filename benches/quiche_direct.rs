//! Criterion benchmarks for quiche state machine in isolation.
//! No sockets — all packet exchange is in-memory.
#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rcgen::{CertificateParams, KeyPair};

// ── Cert generation ─────────────────────────────────────────────────

fn generate_test_certs() -> (String, String) {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair.serialize_pem())
}

fn write_temp_certs(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let (cert_pem, key_pem) = generate_test_certs();
    let id = std::process::id();
    let cert_path = std::env::temp_dir().join(format!("bench_cert_{id}_{label}.pem"));
    let key_path = std::env::temp_dir().join(format!("bench_key_{id}_{label}.pem"));
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();
    (cert_path, key_path)
}

// ── Config builders ─────────────────────────────────────────────────

const MAX_DATAGRAM_SIZE: usize = 1350;

fn make_client_config() -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config
        .set_application_protos(&[b"bench"])
        .unwrap();
    config.verify_peer(false);
    config.set_max_idle_timeout(5000);
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

fn make_server_config(cert_path: &std::path::Path, key_path: &std::path::Path) -> quiche::Config {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config
        .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
        .unwrap();
    config
        .load_priv_key_from_pem_file(key_path.to_str().unwrap())
        .unwrap();
    config
        .set_application_protos(&[b"bench"])
        .unwrap();
    config.set_max_idle_timeout(5000);
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

// ── Pair helper ─────────────────────────────────────────────────────

struct BenchPair {
    client: quiche::Connection,
    server: Option<quiche::Connection>,
    server_config: quiche::Config,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    buf: Vec<u8>,
}

impl BenchPair {
    fn new(cert_path: &std::path::Path, key_path: &std::path::Path) -> Self {
        let mut client_config = make_client_config();
        let server_config = make_server_config(cert_path, key_path);
        let client_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let server_addr: SocketAddr = "127.0.0.1:54321".parse().unwrap();

        let scid = vec![0xba; quiche::MAX_CONN_ID_LEN];
        let scid = quiche::ConnectionId::from_ref(&scid);
        let client =
            quiche::connect(Some("localhost"), &scid, client_addr, server_addr, &mut client_config)
                .unwrap();

        Self {
            client,
            server: None,
            server_config,
            client_addr,
            server_addr,
            buf: vec![0u8; 65535],
        }
    }

    fn exchange_until_established(&mut self) {
        for _ in 0..100 {
            let mut exchanged = false;
            // Client -> Server
            loop {
                match self.client.send(&mut self.buf) {
                    Ok((len, info)) => {
                        exchanged = true;
                        if self.server.is_none() {
                            let hdr = quiche::Header::from_slice(
                                &mut self.buf[..len],
                                quiche::MAX_CONN_ID_LEN,
                            )
                            .unwrap();
                            let srv_scid = vec![0xab; quiche::MAX_CONN_ID_LEN];
                            let srv_scid = quiche::ConnectionId::from_ref(&srv_scid);
                            self.server = Some(
                                quiche::accept(
                                    &srv_scid,
                                    Some(&hdr.dcid),
                                    self.server_addr,
                                    self.client_addr,
                                    &mut self.server_config,
                                )
                                .unwrap(),
                            );
                        }
                        let recv_info = quiche::RecvInfo {
                            from: self.client_addr,
                            to: info.to,
                        };
                        self.server
                            .as_mut()
                            .unwrap()
                            .recv(&mut self.buf[..len], recv_info)
                            .unwrap();
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => panic!("client send: {e}"),
                }
            }
            // Server -> Client
            if let Some(srv) = self.server.as_mut() {
                loop {
                    match srv.send(&mut self.buf) {
                        Ok((len, info)) => {
                            exchanged = true;
                            let recv_info = quiche::RecvInfo {
                                from: self.server_addr,
                                to: info.to,
                            };
                            self.client.recv(&mut self.buf[..len], recv_info).unwrap();
                        }
                        Err(quiche::Error::Done) => break,
                        Err(e) => panic!("server send: {e}"),
                    }
                }
            }
            if !exchanged
                || (self.client.is_established()
                    && self
                        .server
                        .as_ref()
                        .is_some_and(quiche::Connection::is_established))
            {
                break;
            }
        }
        assert!(self.client.is_established());
        assert!(
            self.server
                .as_ref()
                .unwrap()
                .is_established()
        );
    }

    /// Exchange packets between client and server until no progress.
    fn flush(&mut self) {
        for _ in 0..100 {
            let mut exchanged = false;
            loop {
                match self.client.send(&mut self.buf) {
                    Ok((len, info)) => {
                        exchanged = true;
                        let ri = quiche::RecvInfo {
                            from: self.client_addr,
                            to: info.to,
                        };
                        self.server.as_mut().unwrap().recv(&mut self.buf[..len], ri).unwrap();
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => panic!("client send: {e}"),
                }
            }
            if let Some(srv) = self.server.as_mut() {
                loop {
                    match srv.send(&mut self.buf) {
                        Ok((len, info)) => {
                            exchanged = true;
                            let ri = quiche::RecvInfo {
                                from: self.server_addr,
                                to: info.to,
                            };
                            self.client.recv(&mut self.buf[..len], ri).unwrap();
                        }
                        Err(quiche::Error::Done) => break,
                        Err(e) => panic!("server send: {e}"),
                    }
                }
            }
            if !exchanged {
                break;
            }
        }
    }
}

// ── Benchmarks ──────────────────────────────────────────────────────

fn quiche_handshake(c: &mut Criterion) {
    let (cert_path, key_path) = write_temp_certs("handshake");
    c.bench_function("quiche_handshake", |b| {
        b.iter(|| {
            let mut pair = BenchPair::new(&cert_path, &key_path);
            pair.exchange_until_established();
        });
    });
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

fn quiche_stream_echo(c: &mut Criterion) {
    let (cert_path, key_path) = write_temp_certs("stream_echo");
    let mut group = c.benchmark_group("quiche_stream_echo");
    for payload_size in [1024, 16384, 65536] {
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_size),
            &payload_size,
            |b, &size| {
                let payload = vec![0xAA_u8; size];
                let cert = cert_path.clone();
                let key = key_path.clone();

                b.iter_batched(
                    || {
                        let mut pair = BenchPair::new(&cert, &key);
                        pair.exchange_until_established();
                        pair
                    },
                    |mut pair| {
                        let stream_id = 0u64;

                        // Client sends
                        pair.client
                            .stream_send(stream_id, &payload, true)
                            .unwrap();
                        pair.flush();

                        // Server echoes
                        let srv = pair.server.as_mut().unwrap();
                        let mut recv_buf = vec![0u8; 65535];
                        let mut echo_data = Vec::new();
                        loop {
                            match srv.stream_recv(stream_id, &mut recv_buf) {
                                Ok((n, _fin)) => echo_data.extend_from_slice(&recv_buf[..n]),
                                Err(quiche::Error::Done) => break,
                                Err(e) => panic!("server recv: {e}"),
                            }
                        }
                        srv.stream_send(stream_id, &echo_data, true).unwrap();
                        pair.flush();

                        // Client drains
                        loop {
                            match pair.client.stream_recv(stream_id, &mut recv_buf) {
                                Ok((_n, fin)) => {
                                    if fin {
                                        break;
                                    }
                                }
                                Err(quiche::Error::Done) => break,
                                Err(e) => panic!("client recv: {e}"),
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

fn quiche_conn_send_burst(c: &mut Criterion) {
    let (cert_path, key_path) = write_temp_certs("send_burst");
    let mut group = c.benchmark_group("quiche_conn_send_burst");
    for burst_count in [1, 16, 64, 256] {
        group.bench_with_input(
            BenchmarkId::from_parameter(burst_count),
            &burst_count,
            |b, &count| {
                let payload = vec![0xBB_u8; 1200];
                let cert = cert_path.clone();
                let key = key_path.clone();

                b.iter_batched(
                    || {
                        let mut pair = BenchPair::new(&cert, &key);
                        pair.exchange_until_established();
                        pair
                    },
                    |mut pair| {
                        // Write data on fresh streams to give conn.send() work
                        for i in 0..count {
                            let stream_id = (i as u64) * 4;
                            let _ = pair.client.stream_send(stream_id, &payload, true);
                        }

                        // Drain conn.send() — this is what we're measuring
                        let mut buf = vec![0u8; 65535];
                        let mut packets = 0;
                        loop {
                            match pair.client.send(&mut buf) {
                                Ok((_len, _info)) => packets += 1,
                                Err(quiche::Error::Done) => break,
                                Err(e) => panic!("send: {e}"),
                            }
                        }
                        assert!(packets > 0);
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

fn quiche_conn_recv_burst(c: &mut Criterion) {
    let (cert_path, key_path) = write_temp_certs("recv_burst");
    let mut group = c.benchmark_group("quiche_conn_recv_burst");
    for burst_count in [1, 16, 64, 256] {
        group.bench_with_input(
            BenchmarkId::from_parameter(burst_count),
            &burst_count,
            |b, &count| {
                let payload = vec![0xCC_u8; 1200];
                let cert = cert_path.clone();
                let key = key_path.clone();

                b.iter_batched(
                    || {
                        let mut pair = BenchPair::new(&cert, &key);
                        pair.exchange_until_established();

                        // Server sends data on streams
                        let srv = pair.server.as_mut().unwrap();
                        for i in 0..count {
                            let stream_id = (i as u64) * 4 + 1; // server-initiated bidi
                            let _ = srv.stream_send(stream_id, &payload, true);
                        }

                        // Capture server's outbound packets
                        let mut captured = Vec::new();
                        let mut buf = vec![0u8; 65535];
                        loop {
                            match pair.server.as_mut().unwrap().send(&mut buf) {
                                Ok((len, info)) => captured.push((buf[..len].to_vec(), info)),
                                Err(quiche::Error::Done) => break,
                                Err(e) => panic!("server send: {e}"),
                            }
                        }
                        (pair, captured)
                    },
                    |(mut pair, captured)| {
                        // Feed captured packets to client — this is what we measure
                        for (pkt, _info) in &captured {
                            let ri = quiche::RecvInfo {
                                from: pair.server_addr,
                                to: pair.client_addr,
                            };
                            let mut pkt_copy = pkt.clone();
                            let _ = pair.client.recv(&mut pkt_copy, ri);
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

criterion_group!(
    benches,
    quiche_handshake,
    quiche_stream_echo,
    quiche_conn_send_burst,
    quiche_conn_recv_burst
);
criterion_main!(benches);
