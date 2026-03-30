#![allow(clippy::too_many_lines)]

//! Sustained MockDriver workload for profiling with `perf record`.
//!
//! Unlike the existing mock_quic profile (which runs N streams then exits),
//! this module keeps a pool of concurrent streams active and replaces each
//! completed stream with a new one, producing a steady-state hot loop for
//! the specified duration.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use serde::Serialize;

use crate::cid::CidEncoding;
use crate::config::{
    ClientAuthMode, Http3Config, JsQuicClientOptions, JsQuicServerOptions, TransportRuntimeMode,
    new_quic_client_config, new_quic_server_config,
};
use crate::h3_event::{
    EVENT_DATA, EVENT_ERROR, EVENT_FINISHED, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION,
    EVENT_NEW_STREAM, EVENT_SESSION_CLOSE, JsH3Event,
};
use crate::chunk_pool::Chunk;
use crate::profile::event_sink::{TaggedEventBatch, channel_batcher};
use crate::quic_worker::{
    QuicServerCommand, QuicServerConfig, QuicServerHandle, spawn_dedicated_quic_client_on_driver,
    spawn_server_worker_on_driver,
};
use crate::reactor_metrics;
use crate::transport::mock::MockDriver;
use crate::worker::{
    WorkerCommand, WorkerHandle, spawn_h3_client_on_driver, spawn_h3_server_worker_on_driver,
};

const DEFAULT_SERVER_NAME: &str = "localhost";

// ── CLI args ────────────────────────────────────────────────────────

struct CliArgs {
    protocol: Protocol,
    duration_secs: u64,
    streams: usize,
    payload_bytes: usize,
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Quic,
    H3,
}

fn parse_cli(args: Vec<String>) -> Result<CliArgs, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Err(help_text());
    }

    let mut protocol = Protocol::Quic;
    let mut duration_secs = 60u64;
    let mut streams = 10usize;
    let mut payload_bytes = 4096usize;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--protocol" => {
                i += 1;
                protocol = match args.get(i).map(String::as_str) {
                    Some("quic") => Protocol::Quic,
                    Some("h3") => Protocol::H3,
                    other => {
                        return Err(format!(
                            "invalid --protocol value: {other:?} (expected quic or h3)"
                        ))
                    }
                };
            }
            "--duration-secs" => {
                i += 1;
                duration_secs = args
                    .get(i)
                    .ok_or("missing value for --duration-secs")?
                    .parse()
                    .map_err(|e| format!("invalid --duration-secs: {e}"))?;
            }
            "--streams" => {
                i += 1;
                streams = args
                    .get(i)
                    .ok_or("missing value for --streams")?
                    .parse()
                    .map_err(|e| format!("invalid --streams: {e}"))?;
            }
            "--payload-bytes" => {
                i += 1;
                payload_bytes = args
                    .get(i)
                    .ok_or("missing value for --payload-bytes")?
                    .parse()
                    .map_err(|e| format!("invalid --payload-bytes: {e}"))?;
            }
            "--json" => {
                json = true;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    Ok(CliArgs {
        protocol,
        duration_secs,
        streams,
        payload_bytes,
        json,
    })
}

fn help_text() -> String {
    "\
quic_mock_sustained - sustained MockDriver workload for profiling

USAGE:
    quic_mock_sustained [OPTIONS]

OPTIONS:
    --protocol <quic|h3>     Protocol to use (default: quic)
    --duration-secs <N>      Duration in seconds (default: 60)
    --streams <N>            Number of concurrent streams (default: 10)
    --payload-bytes <N>      Bytes per stream payload (default: 4096)
    --json                   Output results as JSON
    -h, --help               Show this help
"
    .to_string()
}

// ── Output ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SustainedResult {
    completed_streams: u64,
    total_bytes: u64,
    throughput_mbps: f64,
    errors: usize,
    elapsed_secs: f64,
}

// ── TLS cert generation ─────────────────────────────────────────────

fn generate_self_signed_cert() -> Result<(Vec<u8>, Vec<u8>), String> {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair =
        KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName(
            "localhost"
                .try_into()
                .map_err(|e: rcgen::Error| e.to_string())?,
        ),
        rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ];
    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;
    Ok((cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes()))
}

// ── Server echo state (QUIC) ────────────────────────────────────────

struct QuicServerEchoState {
    pending: HashMap<(u32, u64), Vec<u8>>,
    echoed_streams: u64,
    echoed_bytes: u64,
    errors: Vec<String>,
}

impl QuicServerEchoState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            echoed_streams: 0,
            echoed_bytes: 0,
            errors: Vec::new(),
        }
    }

    fn handle_batch(&mut self, server: &QuicServerHandle, batch: TaggedEventBatch) {
        for event in batch.events {
            match event.event_type {
                EVENT_NEW_SESSION => {}
                EVENT_NEW_STREAM | EVENT_DATA => {
                    let Some(stream_id) = event_stream_id(&event) else {
                        continue;
                    };
                    let key = (event.conn_handle, stream_id);
                    if let Some(data) = event.data.as_ref() {
                        self.pending.entry(key).or_default().extend_from_slice(data);
                    } else {
                        self.pending.entry(key).or_default();
                    }
                    if event.fin == Some(true) {
                        self.flush_echo_quic(server, key);
                    }
                }
                EVENT_FINISHED => {
                    let Some(stream_id) = event_stream_id(&event) else {
                        continue;
                    };
                    self.flush_echo_quic(server, (event.conn_handle, stream_id));
                }
                EVENT_ERROR => {
                    self.errors.push(format!(
                        "server conn={} stream={} {}",
                        event.conn_handle,
                        event.stream_id,
                        event_error_text(&event)
                    ));
                }
                EVENT_SESSION_CLOSE => {
                    self.pending
                        .retain(|(conn_handle, _), _| *conn_handle != event.conn_handle);
                }
                _ => {}
            }
        }
    }

    fn flush_echo_quic(&mut self, server: &QuicServerHandle, key: (u32, u64)) {
        let body = self.pending.remove(&key).unwrap_or_default();
        let body_len = body.len() as u64;
        let _ = server.send_command(QuicServerCommand::StreamSend {
            conn_handle: key.0,
            stream_id: key.1,
            chunk: Chunk::unpooled(body),
            fin: true,
        });
        self.echoed_streams += 1;
        self.echoed_bytes += body_len;
    }
}

// ── Server echo state (H3) ─────────────────────────────────────────

struct H3ServerEchoState {
    pending: HashMap<(u32, u64), Vec<u8>>,
    echoed_streams: u64,
    echoed_bytes: u64,
    errors: Vec<String>,
}

impl H3ServerEchoState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            echoed_streams: 0,
            echoed_bytes: 0,
            errors: Vec::new(),
        }
    }

    fn handle_batch(&mut self, server: &WorkerHandle, batch: TaggedEventBatch) {
        for event in batch.events {
            match event.event_type {
                EVENT_NEW_SESSION => {}
                EVENT_NEW_STREAM | EVENT_DATA => {
                    let Some(stream_id) = event_stream_id(&event) else {
                        continue;
                    };
                    let key = (event.conn_handle, stream_id);
                    if let Some(data) = event.data.as_ref() {
                        self.pending.entry(key).or_default().extend_from_slice(data);
                    } else {
                        self.pending.entry(key).or_default();
                    }
                    if event.fin == Some(true) {
                        self.flush_echo_h3(server, key);
                    }
                }
                EVENT_FINISHED => {
                    let Some(stream_id) = event_stream_id(&event) else {
                        continue;
                    };
                    self.flush_echo_h3(server, (event.conn_handle, stream_id));
                }
                EVENT_ERROR => {
                    self.errors.push(format!(
                        "server conn={} stream={} {}",
                        event.conn_handle,
                        event.stream_id,
                        event_error_text(&event)
                    ));
                }
                EVENT_SESSION_CLOSE => {
                    self.pending
                        .retain(|(conn_handle, _), _| *conn_handle != event.conn_handle);
                }
                _ => {}
            }
        }
    }

    fn flush_echo_h3(&mut self, server: &WorkerHandle, key: (u32, u64)) {
        let body = self.pending.remove(&key).unwrap_or_default();
        let body_len = body.len() as u64;
        // H3: send response headers first, then body with fin
        let _ = server.send_command(WorkerCommand::SendResponseHeaders {
            conn_handle: key.0,
            stream_id: key.1,
            headers: vec![
                (":status".into(), "200".into()),
                ("content-length".into(), body_len.to_string()),
            ],
            fin: false,
        });
        let _ = server.send_command(WorkerCommand::StreamSend {
            conn_handle: key.0,
            stream_id: key.1,
            chunk: crate::chunk_pool::Chunk::unpooled(body),
            fin: true,
        });
        self.echoed_streams += 1;
        self.echoed_bytes += body_len;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn event_stream_id(event: &JsH3Event) -> Option<u64> {
    if event.stream_id < 0 {
        None
    } else {
        Some(event.stream_id as u64)
    }
}

fn event_error_text(event: &JsH3Event) -> String {
    event
        .meta
        .as_ref()
        .and_then(|meta| meta.error_reason.clone())
        .unwrap_or_else(|| "unknown error".into())
}

// ── QUIC sustained workload ─────────────────────────────────────────

fn run_quic(args: &CliArgs) -> Result<SustainedResult, String> {
    reactor_metrics::reset();

    let (cert_pem, key_pem) = generate_self_signed_cert()?;

    let server_options = JsQuicServerOptions {
        key: key_pem.into(),
        cert: cert_pem.into(),
        ca: None,
        client_auth: None,
        alpn: Some(vec!["quic".into()]),
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(u32::MAX),
        initial_max_stream_data_bidi_local: Some(16_000_000),
        initial_max_streams_bidi: Some(1_000_000),
        disable_active_migration: Some(true),
        enable_datagrams: Some(false),
        max_connections: Some(128),
        disable_retry: Some(true),
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        keylog: Some(false),
    };
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: Some(vec!["quic".into()]),
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(u32::MAX),
        initial_max_stream_data_bidi_local: Some(16_000_000),
        initial_max_streams_bidi: Some(1_000_000),
        session_ticket: None,
        allow_0rtt: Some(false),
        enable_datagrams: Some(false),
        keylog: Some(false),
        qlog_dir: None,
        qlog_level: None,
    };

    let server_quiche = new_quic_server_config(&server_options).map_err(|e| e.to_string())?;
    let client_quiche = new_quic_client_config(&client_options).map_err(|e| e.to_string())?;

    let server_config = QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: 128,
        disable_retry: true,
        client_auth: ClientAuthMode::None,
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
    };

    let client_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_000);
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_001);
    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair(client_addr, server_addr);

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _server_sink_stats) = channel_batcher("server", server_event_tx);
    let (client_batcher, _client_sink_stats) = channel_batcher("client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (client_cmd_tx, client_cmd_rx) = unbounded();
    let server_worker = spawn_server_worker_on_driver(
        server_quiche,
        server_config,
        0,
        server_driver,
        server_waker,
        server_addr,
        server_cmd_tx,
        server_cmd_rx,
        server_batcher,
    );
    let mut server = QuicServerHandle::from_workers(vec![server_worker], server_addr);
    let mut client = spawn_dedicated_quic_client_on_driver(
        client_quiche,
        server_addr,
        DEFAULT_SERVER_NAME.to_string(),
        None,
        None,
        None,
        client_driver,
        client_waker,
        client_addr,
        client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    )
    .map_err(|e| e.to_string())?;

    let payload = vec![b'x'; args.payload_bytes];
    let duration = Duration::from_secs(args.duration_secs);
    let start = Instant::now();
    let mut echo_state = QuicServerEchoState::new();
    let mut handshake_complete = false;
    let mut next_stream_index: u64 = 0;
    let mut active_streams: HashMap<u64, ()> = HashMap::new();
    let mut completed_streams: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut error_count: usize = 0;

    while start.elapsed() < duration {
        crossbeam_channel::select! {
            recv(server_event_rx) -> msg => {
                if let Ok(batch) = msg {
                    echo_state.handle_batch(&server, batch);
                }
            }
            recv(client_event_rx) -> msg => {
                let Ok(batch) = msg else { break };
                for event in batch.events {
                    match event.event_type {
                        EVENT_HANDSHAKE_COMPLETE => {
                            if !handshake_complete {
                                handshake_complete = true;
                                // Open initial pool of streams
                                for _ in 0..args.streams {
                                    let stream_id = next_stream_index * 4;
                                    next_stream_index += 1;
                                    if client.stream_send(stream_id, payload.clone(), true) {
                                        active_streams.insert(stream_id, ());
                                        total_bytes += args.payload_bytes as u64;
                                    } else {
                                        error_count += 1;
                                    }
                                }
                            }
                        }
                        EVENT_NEW_STREAM | EVENT_DATA => {
                            if event.fin == Some(true) {
                                if let Some(stream_id) = event_stream_id(&event) {
                                    if active_streams.remove(&stream_id).is_some() {
                                        completed_streams += 1;
                                        // Replace with a new stream if still within duration
                                        if start.elapsed() < duration {
                                            let new_stream_id = next_stream_index * 4;
                                            next_stream_index += 1;
                                            if client.stream_send(new_stream_id, payload.clone(), true) {
                                                active_streams.insert(new_stream_id, ());
                                                total_bytes += args.payload_bytes as u64;
                                            } else {
                                                error_count += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        EVENT_FINISHED => {
                            if let Some(stream_id) = event_stream_id(&event) {
                                if active_streams.remove(&stream_id).is_some() {
                                    completed_streams += 1;
                                    // Replace with a new stream if still within duration
                                    if start.elapsed() < duration {
                                        let new_stream_id = next_stream_index * 4;
                                        next_stream_index += 1;
                                        if client.stream_send(new_stream_id, payload.clone(), true) {
                                            active_streams.insert(new_stream_id, ());
                                            total_bytes += args.payload_bytes as u64;
                                        } else {
                                            error_count += 1;
                                        }
                                    }
                                }
                            }
                        }
                        EVENT_ERROR => {
                            error_count += 1;
                        }
                        EVENT_SESSION_CLOSE => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
            default(Duration::from_millis(25)) => {}
        }
    }

    error_count += echo_state.errors.len();

    client.shutdown();
    server.shutdown();

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    let throughput_mbps = if elapsed.is_zero() {
        0.0
    } else {
        (total_bytes as f64 * 8.0) / elapsed_secs / 1_000_000.0
    };

    Ok(SustainedResult {
        completed_streams,
        total_bytes,
        throughput_mbps,
        errors: error_count,
        elapsed_secs,
    })
}

// ── H3 sustained workload ───────────────────────────────────────────

fn run_h3(args: &CliArgs) -> Result<SustainedResult, String> {
    reactor_metrics::reset();

    let (cert_pem, key_pem) = generate_self_signed_cert()?;

    let server_js_options = crate::config::JsServerOptions {
        key: key_pem.into(),
        cert: cert_pem.into(),
        ca: None,
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(u32::MAX),
        initial_max_stream_data_bidi_local: Some(16_000_000),
        initial_max_streams_bidi: Some(1_000_000),
        disable_active_migration: Some(true),
        enable_datagrams: Some(false),
        qpack_max_table_capacity: None,
        qpack_blocked_streams: None,
        recv_batch_size: None,
        send_batch_size: None,
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        max_connections: Some(128),
        disable_retry: Some(true),
        reuse_port: Some(false),
        keylog: Some(false),
        quic_lb: None,
        server_id: None,
    };
    let client_js_options = crate::config::JsClientOptions {
        ca: None,
        reject_unauthorized: Some(false),
        runtime_mode: Some("portable".into()),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: None,
        initial_max_data: Some(u32::MAX),
        initial_max_stream_data_bidi_local: Some(16_000_000),
        initial_max_streams_bidi: Some(1_000_000),
        session_ticket: None,
        allow_0rtt: Some(false),
        enable_datagrams: Some(false),
        keylog: Some(false),
        qlog_dir: None,
        qlog_level: None,
    };

    let server_quiche =
        Http3Config::new_server_quiche_config(&server_js_options).map_err(|e| e.to_string())?;
    let client_quiche =
        Http3Config::new_client_quiche_config(&client_js_options).map_err(|e| e.to_string())?;

    let http3_config = Http3Config {
        qlog_dir: None,
        qlog_level: None,
        qpack_max_table_capacity: None,
        qpack_blocked_streams: None,
        max_connections: 128,
        disable_retry: true,
        reuse_port: false,
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
    };

    let client_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_000);
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_001);
    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair(client_addr, server_addr);

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, _server_sink_stats) = channel_batcher("server", server_event_tx);
    let (client_batcher, _client_sink_stats) = channel_batcher("client", client_event_tx);

    let (server_cmd_tx, server_cmd_rx) = unbounded();
    let (client_cmd_tx, client_cmd_rx) = unbounded();

    let server_worker = spawn_h3_server_worker_on_driver(
        server_quiche,
        http3_config,
        0,
        server_driver,
        server_waker,
        server_addr,
        server_cmd_tx,
        server_cmd_rx,
        server_batcher,
    );
    let mut h3_server = WorkerHandle::from_workers(vec![server_worker], server_addr);

    let mut h3_client = spawn_h3_client_on_driver(
        client_quiche,
        server_addr,
        DEFAULT_SERVER_NAME.to_string(),
        None,
        None,
        None,
        client_driver,
        client_waker,
        client_addr,
        client_cmd_tx,
        client_cmd_rx,
        client_batcher,
    );

    let payload = vec![b'x'; args.payload_bytes];
    let duration = Duration::from_secs(args.duration_secs);
    let start = Instant::now();
    let mut echo_state = H3ServerEchoState::new();
    let mut handshake_complete = false;
    let mut active_streams: HashMap<u64, ()> = HashMap::new();
    let mut completed_streams: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut error_count: usize = 0;

    while start.elapsed() < duration {
        crossbeam_channel::select! {
            recv(server_event_rx) -> msg => {
                if let Ok(batch) = msg {
                    echo_state.handle_batch(&h3_server, batch);
                }
            }
            recv(client_event_rx) -> msg => {
                let Ok(batch) = msg else { break };
                for event in batch.events {
                    match event.event_type {
                        EVENT_HANDSHAKE_COMPLETE => {
                            if !handshake_complete {
                                handshake_complete = true;
                                // Open initial pool of streams via H3 request API
                                for _ in 0..args.streams {
                                    match open_h3_stream(&h3_client, &payload) {
                                        Ok(stream_id) => {
                                            active_streams.insert(stream_id, ());
                                            total_bytes += args.payload_bytes as u64;
                                        }
                                        Err(_) => error_count += 1,
                                    }
                                }
                            }
                        }
                        EVENT_NEW_STREAM | EVENT_DATA => {
                            if event.fin == Some(true) {
                                if let Some(stream_id) = event_stream_id(&event) {
                                    if active_streams.remove(&stream_id).is_some() {
                                        completed_streams += 1;
                                        if start.elapsed() < duration {
                                            match open_h3_stream(&h3_client, &payload) {
                                                Ok(new_id) => {
                                                    active_streams.insert(new_id, ());
                                                    total_bytes += args.payload_bytes as u64;
                                                }
                                                Err(_) => error_count += 1,
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        EVENT_FINISHED => {
                            if let Some(stream_id) = event_stream_id(&event) {
                                if active_streams.remove(&stream_id).is_some() {
                                    completed_streams += 1;
                                    if start.elapsed() < duration {
                                        match open_h3_stream(&h3_client, &payload) {
                                            Ok(new_id) => {
                                                active_streams.insert(new_id, ());
                                                total_bytes += args.payload_bytes as u64;
                                            }
                                            Err(_) => error_count += 1,
                                        }
                                    }
                                }
                            }
                        }
                        EVENT_ERROR => {
                            error_count += 1;
                        }
                        EVENT_SESSION_CLOSE => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
            default(Duration::from_millis(25)) => {}
        }
    }

    error_count += echo_state.errors.len();

    h3_client.shutdown();
    h3_server.shutdown();

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    let throughput_mbps = if elapsed.is_zero() {
        0.0
    } else {
        (total_bytes as f64 * 8.0) / elapsed_secs / 1_000_000.0
    };

    Ok(SustainedResult {
        completed_streams,
        total_bytes,
        throughput_mbps,
        errors: error_count,
        elapsed_secs,
    })
}

fn open_h3_stream(
    client: &crate::worker::ClientWorkerHandle,
    payload: &[u8],
) -> Result<u64, String> {
    let headers = vec![
        (":method".into(), "POST".into()),
        (":scheme".into(), "https".into()),
        (":path".into(), "/echo".into()),
        (":authority".into(), DEFAULT_SERVER_NAME.into()),
        ("content-length".into(), payload.len().to_string()),
    ];
    let stream_id = client
        .send_request(headers, false)
        .map_err(|e| e.to_string())?;
    client.stream_send(stream_id, crate::chunk_pool::Chunk::unpooled(payload.to_vec()), true);
    Ok(stream_id)
}

// ── Public entry point ──────────────────────────────────────────────

pub fn cli_main() -> Result<(), String> {
    env_logger::init();

    let args = parse_cli(std::env::args().skip(1).collect())?;

    let proto_name = match args.protocol {
        Protocol::Quic => "quic",
        Protocol::H3 => "h3",
    };

    if !args.json {
        eprintln!(
            "Starting sustained {} workload: {} concurrent streams, {} byte payloads, {}s duration",
            proto_name, args.streams, args.payload_bytes, args.duration_secs
        );
    }

    let result = match args.protocol {
        Protocol::Quic => run_quic(&args),
        Protocol::H3 => run_h3(&args),
    }?;

    if args.json {
        let json = serde_json::to_string(&result).map_err(|e| e.to_string())?;
        println!("{json}");
    } else {
        println!("Completed streams: {}", result.completed_streams);
        println!("Total bytes:       {}", result.total_bytes);
        println!("Throughput:         {:.2} Mbps", result.throughput_mbps);
        println!("Errors:            {}", result.errors);
        println!("Elapsed:           {:.2}s", result.elapsed_secs);
    }

    Ok(())
}
