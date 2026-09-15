use super::*;
use crate::arc_buf::ArcBuf;
use crate::reactor_metrics;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::MutexGuard;

fn setup_metrics() -> MutexGuard<'static, ()> {
    let guard = reactor_metrics::test_metrics_guard();
    reactor_metrics::reset();
    guard
}

/// QUIC path validation happens inside quiche — the worker accepts
/// packets from any peer.
#[test]
fn allows_packets_after_peer_address_change() {
    let _original: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
    let _migrated: SocketAddr = "127.0.0.1:444".parse().expect("valid addr");
    // Acceptance is unconditional; this test verifies compile-time only.
}

#[test]
fn h3_pending_write_byte_accounting_tracks_queue_lifecycle() {
    let _guard = setup_metrics();
    let mut pending = HashMap::new();

    insert_pending_write(
        &mut pending,
        7_u64,
        PendingWrite::new(ArcBuf::from_vec(vec![0; 4]), false),
    );
    assert_eq!(reactor_metrics::snapshot().outboundPendingWriteBytes, 4);

    let write = pending.get_mut(&7).expect("pending write exists");
    reactor_metrics::record_outbound_pending_write_added(
        write.push_chunk(Chunk::unpooled(vec![0; 6])),
    );
    let snap = reactor_metrics::snapshot();
    assert_eq!(snap.outboundPendingWriteBytes, 10);
    assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 10);

    assert_eq!(remove_pending_write(&mut pending, &7), 10);
    let snap = reactor_metrics::snapshot();
    assert_eq!(snap.outboundPendingWriteBytes, 0);
    assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 10);
}

#[test]
fn h3_command_outbound_bytes_reads_unflattened_chunks_client() {
    let client_cmd = ClientWorkerCommand::StreamSend {
        stream_id: 4,
        chunk: Chunk::unpooled(vec![2; 9]),
        fin: true,
    };
    let (client_resp_tx, _client_resp_rx) = crossbeam_channel::bounded(1);
    let client_datagram_cmd = ClientWorkerCommand::SendDatagram {
        data: Chunk::unpooled(vec![4; 13]),
        resp_tx: client_resp_tx,
    };

    assert_eq!(client_worker_command_outbound_bytes(&client_cmd), 9);
    assert_eq!(
        client_worker_command_outbound_bytes(&client_datagram_cmd),
        13
    );
}

#[cfg(feature = "os-runtime")]
#[test]
fn h3_command_outbound_bytes_reads_unflattened_chunks_server() {
    let server_cmd = WorkerCommand::StreamSend {
        conn_handle: 1,
        stream_id: 2,
        chunk: Chunk::unpooled(vec![1; 5]),
        fin: false,
    };
    let (server_resp_tx, _server_resp_rx) = crossbeam_channel::bounded(1);
    let server_datagram_cmd = WorkerCommand::SendDatagram {
        conn_handle: 1,
        data: Chunk::unpooled(vec![3; 11]),
        resp_tx: server_resp_tx,
    };

    assert_eq!(worker_command_outbound_bytes(&server_cmd), 5);
    assert_eq!(worker_command_outbound_bytes(&server_datagram_cmd), 11);
}

// ── A2 task 6: direct-call surface tests (H3ClientHandler::new_direct) ──
//
// Sans-IO packet pump, same shape as connection.rs's
// `exchange_handshake_packets`/`exchange_h3_packets`: no `Driver`, no
// thread, no socket — just `H3ClientHandler` on one side and a raw
// `quiche::Connection` (server role) on the other, datagrams handed off
// in memory. Exercises `new_direct` (A2 task 2), the always-compiled
// direct-call methods (A2 task 1), and keylog capture (A2 task 5).
mod direct_call_h3 {
    use super::*;
    use crate::h3_event::{EVENT_HANDSHAKE_COMPLETE, EVENT_SESSION_CLOSE};
    use std::net::{IpAddr, Ipv4Addr};

    const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

    fn test_addrs() -> (SocketAddr, SocketAddr) {
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_001),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51_001),
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
        let cert_path = std::env::temp_dir().join(format!("h3_direct_test_cert_{id:?}.pem"));
        let key_path = std::env::temp_dir().join(format!("h3_direct_test_key_{id:?}.pem"));
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
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .expect("server h3 alpn");
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
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .expect("client h3 alpn");
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

    /// Pump datagrams between the direct-call `H3ClientHandler` and a
    /// raw server-role `quiche::Connection` until both sides report the
    /// handshake established (or panic after a generous iteration cap).
    fn pump_until_established(
        handler: &mut H3ClientHandler,
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
            for mut pkt in outbound {
                progressed = true;
                let payload_len = pkt.payload_len();
                let mut buf = pkt.payload().to_vec();
                if server_conn.is_none() {
                    let hdr = quiche::Header::from_slice(&mut buf, quiche::MAX_CONN_ID_LEN)
                        .expect("parse initial header");
                    let server_scid = vec![0xcd; quiche::MAX_CONN_ID_LEN];
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
                let _ = &mut pkt; // buffer recycled implicitly on drop
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

    /// Same shape as `pump_until_established`, but for the post-close
    /// drain: keeps exchanging datagrams (CONNECTION_CLOSE +
    /// acknowledgment) until the handler has emitted its session-close
    /// event and become reapable.
    ///
    /// Unlike the handshake pump, this genuinely needs wall-clock time:
    /// `quiche::Connection::on_timeout` (called from
    /// `process_timers_for_handle`) checks its *own* internal draining
    /// deadline against the real `Instant::now()` — there is no
    /// injectable clock on the host target (that seam is wasm-only,
    /// §4.7 of docs/WASM_CLIENT_PLAN.md), so a real (short, bounded)
    /// sleep is the only way to cross the 3×PTO close-drain period in a
    /// unit test. `refresh_timeout_deadline` (private, same-crate/
    /// same-module-tree access) picks up the draining deadline —
    /// crucially *after* draining `try_send_next()` each round, since
    /// quiche only arms `draining_timer` as a side effect of actually
    /// writing the CONNECTION_CLOSE frame into a sent packet (not at
    /// `close()` time), and `close_session` itself doesn't refresh it.
    fn pump_until_reapable(
        handler: &mut H3ClientHandler,
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
                    // Closing connections can legitimately fail to
                    // recv once the server side is already draining;
                    // that's fine, we only need the handler to react
                    // to whatever *does* get through (or to its own
                    // local close, which needs no round trip at all).
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
    fn new_direct_completes_handshake_and_send_request() {
        let _guard = setup_metrics();
        let (mut server_config, mut client_config) = build_test_configs();
        let (client_addr, server_addr) = test_addrs();
        let scid = vec![0x42_u8; TEST_SCID_LEN];
        let outbound_admission = Arc::new(OutboundAdmission::default());

        let mut handler = H3ClientHandler::new_direct(
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
        assert!(!handler.is_reapable(), "handler should not be reapable yet");
        assert!(
            handler.next_timer_deadline().is_some(),
            "expected an armed idle-timeout deadline after handshake"
        );

        let stream_id = handler
            .send_request(
                vec![
                    (":method".into(), "GET".into()),
                    (":scheme".into(), "https".into()),
                    (":authority".into(), "localhost".into()),
                    (":path".into(), "/".into()),
                ],
                true,
            )
            .expect("send_request should succeed after handshake");
        assert_eq!(stream_id, 0, "first client-initiated bidi stream is 0");

        // Drive one more round so the request datagram reaches the server.
        batch.clear();
        pump_until_established(
            &mut handler,
            &mut server_conn,
            &mut server_config,
            client_addr,
            server_addr,
            &mut batch,
        );

        handler.close_session(0, "test done");
        batch.clear();
        pump_until_reapable(
            &mut handler,
            &mut server_conn,
            client_addr,
            server_addr,
            &mut batch,
        );
        assert!(
            batch.iter().any(|e| e.event_type == EVENT_SESSION_CLOSE),
            "expected a session-close event after close_session"
        );
        assert!(
            handler.is_reapable(),
            "handler should be reapable after session close"
        );
    }

    #[test]
    fn new_direct_uses_the_caller_supplied_scid_bytes() {
        // The whole point of `new_direct` (vs. the ring-backed `new`) is
        // that the caller's entropy is what ends up on the wire — prove
        // two handlers built from different SCID bytes report different
        // connection ids, i.e. the bytes are actually threaded through
        // to quiche rather than silently re-randomized internally.
        let (_server_config, mut client_config_a) = build_test_configs();
        let (_server_config_b, mut client_config_b) = build_test_configs();
        let (client_addr, server_addr) = test_addrs();

        let handler_a = H3ClientHandler::new_direct(
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
        let handler_b = H3ClientHandler::new_direct(
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
        // `Connection::set_keylog` (called by `enable_keylog` below)
        // only supplies *where* to write; the underlying BoringSSL
        // callback is only invoked at all once `Config::log_keys` has
        // registered it on the shared SSL_CTX — matching the native
        // `options.keylog` path (config.rs's `log_keys()` calls).
        client_config.log_keys();
        let (client_addr, server_addr) = test_addrs();
        let scid = vec![0x77_u8; TEST_SCID_LEN];
        let outbound_admission = Arc::new(OutboundAdmission::default());

        let mut handler = H3ClientHandler::new_direct(
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
        assert!(
            lines.windows(b"CLIENT_".len()).any(|w| w == b"CLIENT_"),
            "keylog output should contain NSS CLIENT_* labels"
        );
        // Draining again with no new secrets yields nothing.
        assert!(handler.take_keylog_lines().is_empty());
    }
}

/// Lockstep tests for the server-side direct-call surface
/// (`H3ServerHandler::new_direct` + `process_inbound_packet` +
/// friends), added alongside server-side wasm ABI support. Mirrors
/// `direct_call_h3` above's style, but drives **two** real handlers
/// (`H3ServerHandler` and `H3ClientHandler`, both already
/// direct-call-constructible) directly against each other — no
/// hand-rolled `quiche::Connection` or real UDP socket needed, since
/// this repo's own server handler is now available as a peer.
mod direct_call_h3_server {
    use super::*;
    use crate::config::{JsClientOptions, JsServerOptions};
    use crate::h3_event::{
        EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_NEW_SESSION, EVENT_SESSION_CLOSE,
    };
    use std::net::{IpAddr, Ipv4Addr};

    const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

    fn test_addrs() -> (SocketAddr, SocketAddr) {
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43_001), // client
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53_001), // server
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

    /// Builds a real `H3ServerHandler::new_direct` using the new
    /// in-memory config builders (`Http3Config::from_server_options` +
    /// `Http3Config::new_server_quiche_config_in_memory`) — the exact
    /// pair a `hs_new` ABI implementation (Part C) will call.
    fn build_server_direct(client_auth: Option<&str>, ca: Option<Vec<u8>>) -> H3ServerHandler {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let options = JsServerOptions {
            key: key_pem.into(),
            cert: cert_pem.into(),
            ca: ca.map(Into::into),
            client_auth: client_auth.map(str::to_string),
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
        let quiche_config =
            Http3Config::new_server_quiche_config_in_memory(&options).expect("server config");
        let http3_config = Http3Config::from_server_options(&options).expect("http3 config");
        H3ServerHandler::new_direct(
            quiche_config,
            http3_config,
            [0x55u8; 32],
            Arc::new(OutboundAdmission::default()),
        )
    }

    fn build_client_direct(
        scid_byte: u8,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> H3ClientHandler {
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
        let mut client_config = Http3Config::new_client_quiche_config_in_memory(&client_options)
            .expect("client config");
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

    /// One pump round: drain the client's outbound into the server
    /// (via `process_inbound_packet`, delivering any retry/version-
    /// negotiation reply straight back), drain the server's outbound
    /// into the client (via `flush_all_sends`), and process any due
    /// timers on both sides. Returns whether anything happened.
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
        client.poll_drain_events_for_handle(client_batch, 0);
        client.flush_pending_writes_for_handle(client_batch, 0);

        progressed
    }

    /// Pumps until `done` returns `true` or a 5-second wall-clock
    /// deadline elapses (then panics). Sleeps briefly on rounds that
    /// make no packet progress — needed for timer-driven transitions
    /// (idle timeout, the H3 server's deferred GOAWAY-then-close) that
    /// only fire once real time has actually passed; see
    /// `pump_until_reapable` above for the identical rationale (no
    /// injectable clock on the host target — that seam is wasm-only).
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
    fn new_direct_completes_handshake_and_h3_request_response() {
        let _guard = setup_metrics();
        let (client_addr, server_addr) = test_addrs();
        let mut server = build_server_direct(None, None);
        let mut client = build_client_direct(0x61, client_addr, server_addr);

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
                .any(|e| e.event_type == EVENT_NEW_SESSION),
            "server should have observed a new session"
        );
        assert_eq!(
            server.connection_count(),
            1,
            "server should be tracking exactly one connection"
        );
        let conn_handle = server_batch
            .iter()
            .find(|e| e.event_type == EVENT_NEW_SESSION)
            .expect("new session event")
            .conn_handle;
        assert!(!server.connection_is_closed(conn_handle));
        assert!(!server.is_idle());

        // --- GET request ---
        let stream_id = client
            .send_request(
                vec![
                    (":method".into(), "GET".into()),
                    (":scheme".into(), "https".into()),
                    (":authority".into(), "localhost".into()),
                    (":path".into(), "/hello".into()),
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
            |_client_batch, server_batch| {
                server_batch.iter().any(|e| e.event_type == EVENT_HEADERS)
            },
        );

        // --- Respond: headers, then body + FIN ---
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
            Chunk::unpooled(b"hello from the direct-call H3 server".to_vec()),
            true,
            &mut server_batch,
        );
        assert!(
            released > 0,
            "response body should be admitted, not backpressured"
        );

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
                    if let Some(data) = &event.data {
                        if event.event_type == EVENT_HEADERS || event.event_type == EVENT_DATA {
                            body.extend_from_slice(data);
                        }
                    }
                }
                got_headers && !body.is_empty()
            },
        );

        assert!(got_headers, "client should have observed EVENT_HEADERS");
        assert_eq!(body, b"hello from the direct-call H3 server");

        // --- Graceful per-connection close (GOAWAY, then deferred CONNECTION_CLOSE) ---
        // A dedicated loop rather than `pump_until` here: `pump_until`'s
        // `done` closure only sees the two batches (by design, so
        // callers never need to fight the borrow checker over aliasing
        // `server` both as the loop's `&mut` argument and inside the
        // closure) — this is the one assertion in this test that
        // genuinely needs to inspect `server`'s own connection state
        // directly instead.
        server.close_connection(conn_handle, 0, "server done".to_string());
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
        assert!(
            server.connection_is_closed(conn_handle),
            "server connection should close within 5s of close_connection"
        );

        // Don't clear `server_batch` here: since the connection closed
        // via the idle/draining *timer* path above, `expire_timers`
        // already pushed `EVENT_SESSION_CLOSE` for it during the
        // close-wait loop, and `reap_closed_connections` deliberately
        // skips re-pushing a duplicate for any handle already seen via
        // `last_expired` (the exact same dedup native's own
        // `cleanup_closed` relies on).
        server.reap_closed_connections(&mut server_batch);
        assert!(
            server_batch
                .iter()
                .any(|e| e.event_type == EVENT_SESSION_CLOSE),
            "expected a session_close event from either the close-wait pump or reap_closed_connections"
        );
        assert!(
            server.is_idle(),
            "server should be idle after reaping the only connection"
        );
        assert_eq!(server.connection_count(), 0);
    }
}
