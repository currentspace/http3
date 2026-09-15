use super::*;
use crate::arc_buf::ArcBuf;
use crate::reactor_metrics;
use std::collections::HashMap;
use std::sync::MutexGuard;

fn setup_metrics() -> MutexGuard<'static, ()> {
    let guard = reactor_metrics::test_metrics_guard();
    reactor_metrics::reset();
    guard
}

#[test]
fn quic_pending_write_byte_accounting_tracks_queue_lifecycle() {
    let _guard = setup_metrics();
    let mut pending = HashMap::new();

    insert_pending_write(
        &mut pending,
        11_u64,
        PendingWrite::new(ArcBuf::from_vec(vec![0; 3]), false),
    );
    assert_eq!(reactor_metrics::snapshot().outboundPendingWriteBytes, 3);

    let write = pending.get_mut(&11).expect("pending write exists");
    reactor_metrics::record_outbound_pending_write_added(
        write.push_chunk(Chunk::unpooled(vec![0; 8])),
    );
    let snap = reactor_metrics::snapshot();
    assert_eq!(snap.outboundPendingWriteBytes, 11);
    assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 11);

    assert_eq!(remove_pending_write(&mut pending, &11), 11);
    let snap = reactor_metrics::snapshot();
    assert_eq!(snap.outboundPendingWriteBytes, 0);
    assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 11);
}

#[test]
fn quic_command_outbound_bytes_reads_unflattened_chunks_client() {
    let client_cmd = QuicClientCommand::StreamSend {
        stream_id: 4,
        chunk: Chunk::unpooled(vec![2; 12]),
        fin: true,
    };
    let (client_resp_tx, _client_resp_rx) = crossbeam_channel::bounded(1);
    let client_datagram_cmd = QuicClientCommand::SendDatagram {
        data: Chunk::unpooled(vec![4; 15]),
        resp_tx: client_resp_tx,
    };

    assert_eq!(quic_client_command_outbound_bytes(&client_cmd), 12);
    assert_eq!(quic_client_command_outbound_bytes(&client_datagram_cmd), 15);
}

#[cfg(feature = "os-runtime")]
#[test]
fn quic_command_outbound_bytes_reads_unflattened_chunks_server() {
    let server_cmd = QuicServerCommand::StreamSend {
        conn_handle: 1,
        stream_id: 2,
        chunk: Chunk::unpooled(vec![1; 7]),
        fin: false,
    };
    let (server_resp_tx, _server_resp_rx) = crossbeam_channel::bounded(1);
    let server_datagram_cmd = QuicServerCommand::SendDatagram {
        conn_handle: 1,
        data: Chunk::unpooled(vec![3; 14]),
        resp_tx: server_resp_tx,
    };

    assert_eq!(quic_server_command_outbound_bytes(&server_cmd), 7);
    assert_eq!(quic_server_command_outbound_bytes(&server_datagram_cmd), 14);
}

// ── A2 task 6: direct-call surface tests (QuicClientHandler::new_direct) ──
//
// Mirrors `worker.rs`'s `direct_call_h3` module exactly (see the doc
// comments there for the rationale behind the pump helpers), but drives
// the raw-QUIC handler surface: `open_bidi_stream`/`queue_stream_send`
// instead of `send_request`.
mod direct_call_quic {
    use super::*;
    use crate::h3_event::{EVENT_HANDSHAKE_COMPLETE, EVENT_SESSION_CLOSE};
    use std::net::{IpAddr, Ipv4Addr};

    const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

    fn test_addrs() -> (SocketAddr, SocketAddr) {
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_001),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52_001),
        )
    }

    fn build_test_configs() -> (quiche::Config, quiche::Config) {
        use rcgen::{CertificateParams, KeyPair};
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("test key");
        let mut params =
            CertificateParams::new(vec!["localhost".into()]).expect("test cert params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key_pair).expect("test cert");
        let (cert_pem, key_pem) = (cert.pem(), key_pair.serialize_pem());

        let id = std::thread::current().id();
        let cert_path = std::env::temp_dir().join(format!("quic_direct_test_cert_{id:?}.pem"));
        let key_path = std::env::temp_dir().join(format!("quic_direct_test_key_{id:?}.pem"));
        std::fs::write(&cert_path, cert_pem).expect("write test cert");
        std::fs::write(&key_path, key_pem).expect("write test key");

        let mut server_config = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("server cfg");
        server_config
            .load_cert_chain_from_pem_file(cert_path.to_str().expect("cert path"))
            .expect("load cert");
        server_config
            .load_priv_key_from_pem_file(key_path.to_str().expect("key path"))
            .expect("load key");
        server_config
            .set_application_protos(&[b"quic"])
            .expect("server alpn");
        server_config.set_max_idle_timeout(30_000);
        server_config.set_initial_max_data(1_000_000);
        server_config.set_initial_max_stream_data_bidi_local(100_000);
        server_config.set_initial_max_stream_data_bidi_remote(100_000);
        server_config.set_initial_max_stream_data_uni(100_000);
        server_config.set_initial_max_streams_bidi(100);
        server_config.set_initial_max_streams_uni(100);
        server_config.set_disable_active_migration(true);

        let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("client cfg");
        client_config
            .set_application_protos(&[b"quic"])
            .expect("client alpn");
        client_config.verify_peer(false);
        client_config.set_max_idle_timeout(30_000);
        client_config.set_initial_max_data(1_000_000);
        client_config.set_initial_max_stream_data_bidi_local(100_000);
        client_config.set_initial_max_stream_data_bidi_remote(100_000);
        client_config.set_initial_max_stream_data_uni(100_000);
        client_config.set_initial_max_streams_bidi(100);
        client_config.set_initial_max_streams_uni(100);
        client_config.set_disable_active_migration(true);

        let _ = std::fs::remove_file(cert_path);
        let _ = std::fs::remove_file(key_path);
        (server_config, client_config)
    }

    fn pump_until_established(
        handler: &mut QuicClientHandler,
        server_conn: &mut Option<quiche::Connection<ArcBufFactory>>,
        server_config: &mut quiche::Config,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
        batch: &mut Vec<JsH3Event>,
    ) {
        for _ in 0..200 {
            let mut progressed = false;

            let mut outbound = Vec::new();
            while let Some(pkt) = handler.try_send_next() {
                outbound.push(pkt);
            }
            for pkt in &outbound {
                progressed = true;
                let payload_len = pkt.payload_len();
                let mut buf = pkt.payload().to_vec();
                if server_conn.is_none() {
                    let hdr = quiche::Header::from_slice(&mut buf, quiche::MAX_CONN_ID_LEN)
                        .expect("parse initial header");
                    let server_scid = vec![0xef; quiche::MAX_CONN_ID_LEN];
                    let server_scid = quiche::ConnectionId::from_ref(&server_scid);
                    *server_conn = Some(
                        quiche::accept_with_buf_factory::<ArcBufFactory>(
                            &server_scid,
                            Some(&hdr.dcid),
                            server_addr,
                            client_addr,
                            server_config,
                        )
                        .expect("accept server conn"),
                    );
                }
                server_conn
                    .as_mut()
                    .expect("server conn")
                    .recv(
                        &mut buf[..payload_len],
                        quiche::RecvInfo {
                            from: client_addr,
                            to: server_addr,
                        },
                    )
                    .expect("server recv client packet");
            }

            if let Some(server) = server_conn.as_mut() {
                let mut send_buf = vec![0_u8; 65_535];
                loop {
                    match server.send(&mut send_buf) {
                        Ok((len, _info)) => {
                            progressed = true;
                            handler.process_packet_for_handle(
                                &mut send_buf[..len],
                                server_addr,
                                client_addr,
                                usize::MAX,
                                batch,
                                0,
                            );
                        }
                        Err(quiche::Error::Done) => break,
                        Err(error) => panic!("server send failed: {error}"),
                    }
                }

                if handler.conn.quiche_conn.is_established() && server.is_established() {
                    return;
                }
            }

            assert!(progressed, "handshake made no progress");
        }
        panic!("handshake did not complete");
    }

    /// See `worker.rs`'s `direct_call_h3::pump_until_reapable` doc
    /// comment for why this needs real wall-clock time.
    fn pump_until_reapable(
        handler: &mut QuicClientHandler,
        server_conn: &mut Option<quiche::Connection<ArcBufFactory>>,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
        batch: &mut Vec<JsH3Event>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if handler.is_reapable() {
                return;
            }

            let mut outbound = Vec::new();
            while let Some(pkt) = handler.try_send_next() {
                outbound.push(pkt);
            }
            handler.refresh_timeout_deadline();
            for pkt in &outbound {
                let payload_len = pkt.payload_len();
                let mut buf = pkt.payload().to_vec();
                if let Some(server) = server_conn.as_mut() {
                    let _ = server.recv(
                        &mut buf[..payload_len],
                        quiche::RecvInfo {
                            from: client_addr,
                            to: server_addr,
                        },
                    );
                }
            }

            if let Some(server) = server_conn.as_mut() {
                let mut send_buf = vec![0_u8; 65_535];
                loop {
                    match server.send(&mut send_buf) {
                        Ok((len, _info)) => {
                            handler.process_packet_for_handle(
                                &mut send_buf[..len],
                                server_addr,
                                client_addr,
                                usize::MAX,
                                batch,
                                0,
                            );
                        }
                        Err(_) => break,
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(20));
            handler.process_timers_for_handle(Instant::now(), usize::MAX, batch, 0);
        }
        panic!("handler did not become reapable within the drain deadline");
    }

    #[test]
    fn new_direct_completes_handshake_open_stream_and_close() {
        let _guard = setup_metrics();
        let (mut server_config, mut client_config) = build_test_configs();
        let (client_addr, server_addr) = test_addrs();
        let scid = vec![0x33_u8; TEST_SCID_LEN];
        let outbound_admission = Arc::new(OutboundAdmission::default());

        let mut handler = QuicClientHandler::new_direct(
            scid,
            client_addr,
            server_addr,
            "localhost",
            None,
            None,
            None,
            &mut client_config,
            outbound_admission,
        )
        .expect("new_direct should construct a handler");

        let mut batch = Vec::new();
        let mut server_conn = None;
        pump_until_established(
            &mut handler,
            &mut server_conn,
            &mut server_config,
            client_addr,
            server_addr,
            &mut batch,
        );

        let event_types: Vec<u8> = batch.iter().map(|e| e.event_type).collect();
        assert!(
            event_types.contains(&EVENT_HANDSHAKE_COMPLETE),
            "expected a handshake-complete event, got event types {event_types:?}"
        );
        assert!(!handler.is_reapable());
        assert!(handler.next_timer_deadline().is_some());

        let stream_id = handler
            .open_bidi_stream()
            .expect("open_bidi_stream should succeed after handshake");
        assert_eq!(stream_id, 0, "first client-initiated bidi stream is 0");

        batch.clear();
        let released = handler.queue_stream_send(
            stream_id,
            Chunk::unpooled(b"hello".to_vec()),
            true,
            &mut batch,
            0,
        );
        assert!(released > 0, "expected admitted bytes back for the write");

        // Drive the stream data to the server.
        pump_until_established(
            &mut handler,
            &mut server_conn,
            &mut server_config,
            client_addr,
            server_addr,
            &mut batch,
        );
        if let Some(server) = server_conn.as_mut() {
            let mut recv_buf = vec![0_u8; 1024];
            let (len, fin) = server
                .stream_recv(stream_id, &mut recv_buf)
                .expect("server should have received the client's stream data");
            assert_eq!(&recv_buf[..len], b"hello");
            assert!(fin, "expected the FIN to be delivered with the data");
        }

        handler.close_session(0, "test done");
        batch.clear();
        pump_until_reapable(
            &mut handler,
            &mut server_conn,
            client_addr,
            server_addr,
            &mut batch,
        );
        let event_types: Vec<u8> = batch.iter().map(|e| e.event_type).collect();
        assert!(
            event_types.contains(&EVENT_SESSION_CLOSE),
            "expected a session-close event after close_session, got {event_types:?}"
        );
        assert!(handler.is_reapable());
    }

    #[test]
    fn new_direct_uses_the_caller_supplied_scid_bytes() {
        let (_server_config_a, mut client_config_a) = build_test_configs();
        let (_server_config_b, mut client_config_b) = build_test_configs();
        let (client_addr, server_addr) = test_addrs();

        let handler_a = QuicClientHandler::new_direct(
            vec![0x11_u8; TEST_SCID_LEN],
            client_addr,
            server_addr,
            "localhost",
            None,
            None,
            None,
            &mut client_config_a,
            Arc::new(OutboundAdmission::default()),
        )
        .expect("new_direct should construct a handler");
        let handler_b = QuicClientHandler::new_direct(
            vec![0x22_u8; TEST_SCID_LEN],
            client_addr,
            server_addr,
            "localhost",
            None,
            None,
            None,
            &mut client_config_b,
            Arc::new(OutboundAdmission::default()),
        )
        .expect("new_direct should construct a handler");

        assert_eq!(handler_a.current_dcid(), vec![0x11_u8; TEST_SCID_LEN]);
        assert_eq!(handler_b.current_dcid(), vec![0x22_u8; TEST_SCID_LEN]);
    }

    #[test]
    fn keylog_capture_accumulates_lines_during_handshake() {
        let _guard = setup_metrics();
        let (mut server_config, mut client_config) = build_test_configs();
        client_config.log_keys();
        let (client_addr, server_addr) = test_addrs();
        let scid = vec![0x77_u8; TEST_SCID_LEN];
        let outbound_admission = Arc::new(OutboundAdmission::default());

        let mut handler = QuicClientHandler::new_direct(
            scid,
            client_addr,
            server_addr,
            "localhost",
            None,
            None,
            None,
            &mut client_config,
            outbound_admission,
        )
        .expect("new_direct should construct a handler");
        handler.enable_keylog();

        let mut batch = Vec::new();
        let mut server_conn = None;
        pump_until_established(
            &mut handler,
            &mut server_conn,
            &mut server_config,
            client_addr,
            server_addr,
            &mut batch,
        );

        let lines = handler.take_keylog_lines();
        assert!(
            !lines.is_empty(),
            "expected at least one NSS-format keylog line after handshake"
        );
        assert!(handler.take_keylog_lines().is_empty());
    }
}

/// Lockstep tests for the raw-QUIC server-side direct-call surface
/// (`QuicServerHandler::new_direct` + `process_inbound_packet` +
/// friends), added alongside server-side wasm ABI support. Mirrors
/// `direct_call_quic` above's style and `worker.rs`'s
/// `direct_call_h3_server` sibling module exactly — two real
/// direct-call handlers pumped against each other, no hand-rolled
/// `quiche::Connection` or real UDP socket needed.
mod direct_call_quic_server {
    use super::*;
    use crate::config::{
        JsQuicClientOptions, JsQuicServerOptions, new_quic_client_config_in_memory,
        new_quic_server_config_in_memory,
    };
    use crate::h3_event::{
        EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION, EVENT_NEW_STREAM,
        EVENT_SESSION_CLOSE,
    };
    use std::net::{IpAddr, Ipv4Addr};

    const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

    fn test_addrs() -> (SocketAddr, SocketAddr) {
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44_001), // client
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54_001), // server
        )
    }

    fn generate_self_signed_pem() -> (Vec<u8>, Vec<u8>) {
        use rcgen::{CertificateParams, KeyPair};
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("keypair");
        let mut params = CertificateParams::new(vec!["localhost".into()]).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key_pair).expect("self-signed cert");
        (
            cert.pem().into_bytes(),
            key_pair.serialize_pem().into_bytes(),
        )
    }

    fn build_server_direct() -> QuicServerHandler {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let options = JsQuicServerOptions {
            key: key_pem.into(),
            cert: cert_pem.into(),
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
        let quiche_config = new_quic_server_config_in_memory(&options).expect("server config");
        let server_config = QuicServerConfig {
            qlog_dir: None,
            qlog_level: None,
            max_connections: options.max_connections.unwrap_or(10_000) as usize,
            disable_retry: options.disable_retry.unwrap_or(false),
            client_auth: ClientAuthMode::parse(
                options.client_auth.as_deref(),
                options.ca.is_some(),
            )
            .expect("valid client auth"),
            cid_encoding: CidEncoding::random(),
            runtime_mode: TransportRuntimeMode::Portable,
        };
        QuicServerHandler::new_direct(
            quiche_config,
            server_config,
            [0x66u8; 32],
            Arc::new(OutboundAdmission::default()),
        )
    }

    fn build_client_direct(
        scid_byte: u8,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> QuicClientHandler {
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
        let mut client_config =
            new_quic_client_config_in_memory(&client_options).expect("client config");
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
                client.process_packet_for_handle(
                    &mut reply_buf,
                    server_addr,
                    client_addr,
                    usize::MAX,
                    client_batch,
                    0,
                );
            }
        }

        let mut server_outbound: Vec<TxDatagram> = Vec::new();
        server.flush_all_sends(&mut server_outbound);
        for pkt in server_outbound {
            progressed = true;
            let mut buf = pkt.payload().to_vec();
            client.process_packet_for_handle(
                &mut buf,
                server_addr,
                client_addr,
                usize::MAX,
                client_batch,
                0,
            );
        }

        if client
            .next_timer_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            client.process_timers_for_handle(Instant::now(), usize::MAX, client_batch, 0);
            progressed = true;
        }
        if server
            .soonest_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            server.expire_timers(Instant::now(), usize::MAX, server_batch);
            progressed = true;
        }

        server.collect_drain_events(usize::MAX, server_batch);
        server.flush_all_pending_writes(server_batch);
        client.poll_drain_events_for_handle(usize::MAX, client_batch, 0);
        client.flush_pending_writes_for_handle(client_batch, 0);

        progressed
    }

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
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done(client_batch, server_batch) {
                return;
            }
            let progressed = pump(
                client,
                server,
                client_addr,
                server_addr,
                client_batch,
                server_batch,
            );
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
    fn new_direct_completes_handshake_stream_echo_and_close() {
        let (client_addr, server_addr) = test_addrs();
        let mut server = build_server_direct();
        let mut client = build_client_direct(0x71, client_addr, server_addr);

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
                client_batch
                    .iter()
                    .any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
                    && server_batch
                        .iter()
                        .any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
            },
        );

        assert!(
            server_batch
                .iter()
                .any(|e| e.event_type == EVENT_NEW_SESSION)
        );
        let conn_handle = server_batch
            .iter()
            .find(|e| e.event_type == EVENT_NEW_SESSION)
            .expect("new session event")
            .conn_handle;
        assert_eq!(server.connection_count(), 1);

        // --- Client opens a bidi stream and sends data; server echoes it ---
        let stream_id = client.open_bidi_stream().expect("open_bidi_stream");
        let released = client.queue_stream_send(
            stream_id,
            Chunk::unpooled(b"ping".to_vec()),
            true,
            &mut client_batch,
            0,
        );
        assert!(released > 0, "client stream send should be admitted");

        // Raw QUIC coalesces a new stream's first recv into the
        // `EVENT_NEW_STREAM` event itself (`data` carried right on
        // it — see `QuicConnection::poll_quic_events`'s "Coalesce
        // first recv into NEW_STREAM event" comment), so — exactly
        // like the H3 lockstep test's HEADERS+coalesced-DATA
        // handling — check `.data` on every event for this stream,
        // not just `EVENT_DATA`-typed ones.
        server_batch.clear();
        pump_until(
            &mut client,
            &mut server,
            client_addr,
            server_addr,
            &mut client_batch,
            &mut server_batch,
            |_client_batch, server_batch| {
                server_batch
                    .iter()
                    .any(|e| e.stream_id as u64 == stream_id && e.data.is_some())
            },
        );

        assert!(
            server_batch
                .iter()
                .any(|e| e.event_type == EVENT_NEW_STREAM)
        );
        let received: Vec<u8> = server_batch
            .iter()
            .filter(|e| e.stream_id as u64 == stream_id)
            .filter_map(|e| e.data.as_deref())
            .flatten()
            .copied()
            .collect();
        assert_eq!(received, b"ping");

        let echoed = server.queue_stream_send(
            conn_handle,
            stream_id,
            Chunk::unpooled(b"pong".to_vec()),
            true,
            &mut server_batch,
        );
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
                client_batch
                    .iter()
                    .any(|e| e.event_type == EVENT_DATA && e.stream_id as u64 == stream_id)
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

        // --- Close ---
        server.close_connection(conn_handle, 0, "server done");
        let close_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < close_deadline && !server.connection_is_closed(conn_handle) {
            let progressed = pump(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
            );
            if !progressed {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(server.connection_is_closed(conn_handle));

        server.reap_closed_connections(&mut server_batch);
        assert!(
            server_batch
                .iter()
                .any(|e| e.event_type == EVENT_SESSION_CLOSE)
        );
        assert!(server.is_idle());
        assert_eq!(server.connection_count(), 0);
    }
}
