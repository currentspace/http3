#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use serde::Serialize;

use crate::cid::CidEncoding;
use crate::config::{
    ClientAuthMode, JsQuicClientOptions, JsQuicServerOptions, TransportRuntimeMode,
    new_quic_client_config, new_quic_server_config,
};
use crate::event_loop::EventBatcherStatsSnapshot;
use crate::h3_event::{
    EVENT_DATA, EVENT_ERROR, EVENT_FINISHED, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION,
    EVENT_NEW_STREAM, EVENT_SESSION_CLOSE, JsH3Event,
};
use crate::profile::event_sink::{TaggedEventBatch, channel_batcher};
use crate::profile::mock_trace::{MockReplayTrace, MockTraceCommand, MockTraceEventBatch};
use crate::quic_worker::{
    QuicClientHandle, QuicServerCommand, QuicServerConfig, spawn_quic_client_with_batcher,
    spawn_quic_server_with_batcher,
};
use crate::reactor_metrics::{self, JsReactorTelemetrySnapshot};

const DEFAULT_ALPN: &str = "quic";
const DEFAULT_SERVER_NAME: &str = "localhost";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Serialize)]
struct NamedSinkStats {
    source: String,
    stats: EventBatcherStatsSnapshot,
}

#[derive(Serialize)]
struct LatencySummary {
    min_ms: f64,
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
struct ServerSummary {
    harness: &'static str,
    role: &'static str,
    bind_addr: String,
    runtime_mode: String,
    alpn: String,
    elapsed_ms: f64,
    sessions_opened: u64,
    sessions_closed: u64,
    echoed_streams: u64,
    echoed_bytes: u64,
    error_count: usize,
    errors: Vec<String>,
    sink: NamedSinkStats,
    runtime_telemetry: JsReactorTelemetrySnapshot,
}

#[derive(Serialize)]
struct ClientSummary {
    harness: &'static str,
    role: &'static str,
    server_addr: String,
    runtime_mode: String,
    alpn: String,
    connections: usize,
    streams_per_connection: usize,
    payload_bytes: usize,
    requested_streams: usize,
    completed_streams: usize,
    ready_connections: usize,
    total_response_bytes: u64,
    elapsed_ms: f64,
    throughput_mbps: f64,
    latency_ms: Option<LatencySummary>,
    error_count: usize,
    errors: Vec<String>,
    sinks: Vec<NamedSinkStats>,
    runtime_telemetry: JsReactorTelemetrySnapshot,
}

struct CommonOptions {
    runtime_mode: TransportRuntimeMode,
    alpn: String,
}

struct ServerOptions {
    common: CommonOptions,
    bind_addr: SocketAddr,
    cert_path: String,
    key_path: String,
}

struct ClientOptions {
    common: CommonOptions,
    server_addr: SocketAddr,
    server_name: String,
    connections: usize,
    streams_per_connection: usize,
    payload_bytes: usize,
    timeout: Duration,
    trace_path: Option<String>,
}

enum CliCommand {
    Server(ServerOptions),
    Client(ClientOptions),
}

struct ServerEchoState {
    pending: HashMap<(u32, u64), Vec<u8>>,
    sessions_opened: u64,
    sessions_closed: u64,
    echoed_streams: u64,
    echoed_bytes: u64,
    errors: Vec<String>,
}

impl ServerEchoState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            sessions_opened: 0,
            sessions_closed: 0,
            echoed_streams: 0,
            echoed_bytes: 0,
            errors: Vec::new(),
        }
    }

    fn handle_batch(
        &mut self,
        server: &crate::quic_worker::QuicServerHandle,
        batch: TaggedEventBatch,
    ) {
        for event in batch.events {
            match event.event_type {
                EVENT_NEW_SESSION => {
                    self.sessions_opened += 1;
                }
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
                        self.flush_echo(server, key);
                    }
                }
                EVENT_FINISHED => {
                    let Some(stream_id) = event_stream_id(&event) else {
                        continue;
                    };
                    self.flush_echo(server, (event.conn_handle, stream_id));
                }
                EVENT_ERROR => {
                    self.errors.push(format!(
                        "{} conn={} stream={} {}",
                        batch.source,
                        event.conn_handle,
                        event.stream_id,
                        event_error_text(&event)
                    ));
                }
                EVENT_SESSION_CLOSE => {
                    self.sessions_closed += 1;
                    self.pending
                        .retain(|(conn_handle, _), _| *conn_handle != event.conn_handle);
                }
                _ => {}
            }
        }
    }

    fn flush_echo(&mut self, server: &crate::quic_worker::QuicServerHandle, key: (u32, u64)) {
        let body = self.pending.remove(&key).unwrap_or_default();
        let body_len = body.len() as u64;
        let _ = server.send_command(QuicServerCommand::StreamSend {
            conn_handle: key.0,
            stream_id: key.1,
            data: body,
            fin: true,
        });
        self.echoed_streams += 1;
        self.echoed_bytes += body_len;
    }
}

struct ClientSessionState {
    source: String,
    handle: QuicClientHandle,
    sink_stats: crate::event_loop::EventBatcherStatsHandle,
    handshake_complete: bool,
    workload_sent: bool,
    pending_streams: HashMap<u64, Instant>,
    completed_streams: usize,
    response_bytes: u64,
}

impl ClientSessionState {
    fn new(
        source: String,
        handle: QuicClientHandle,
        sink_stats: crate::event_loop::EventBatcherStatsHandle,
    ) -> Self {
        Self {
            source,
            handle,
            sink_stats,
            handshake_complete: false,
            workload_sent: false,
            pending_streams: HashMap::new(),
            completed_streams: 0,
            response_bytes: 0,
        }
    }
}

pub fn cli_main() -> Result<(), String> {
    match parse_cli(std::env::args().skip(1).collect())? {
        CliCommand::Server(options) => run_server(options),
        CliCommand::Client(options) => run_client(options),
    }
}

fn run_server(options: ServerOptions) -> Result<(), String> {
    reactor_metrics::reset();
    let server_config = build_server_config(&options)?;
    let quiche_config = build_server_quiche_config(&options)?;
    let (event_tx, event_rx) = unbounded();
    let (batcher, sink_stats) = channel_batcher("server", event_tx);
    let mut server =
        spawn_quic_server_with_batcher(quiche_config, server_config, options.bind_addr, batcher)
            .map_err(|err| err.to_string())?;
    let local_addr = server.local_addr();
    println!("READY {local_addr}");

    let start = Instant::now();
    let mut echo_state = ServerEchoState::new();
    let (control_tx, control_rx) = unbounded();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if control_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        if let Ok(batch) = event_rx.recv_timeout(Duration::from_millis(25)) {
            echo_state.handle_batch(&server, batch);
        }
        match control_rx.try_recv() {
            Ok(line) if line.trim().eq_ignore_ascii_case("stop") => break,
            Ok(_) => {}
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {}
        }
    }

    server.shutdown();
    let summary = ServerSummary {
        harness: "quic-loopback",
        role: "server",
        bind_addr: local_addr.to_string(),
        runtime_mode: runtime_mode_as_str(options.common.runtime_mode).to_string(),
        alpn: options.common.alpn,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        sessions_opened: echo_state.sessions_opened,
        sessions_closed: echo_state.sessions_closed,
        echoed_streams: echo_state.echoed_streams,
        echoed_bytes: echo_state.echoed_bytes,
        error_count: echo_state.errors.len(),
        errors: echo_state.errors,
        sink: NamedSinkStats {
            source: "server".into(),
            stats: sink_stats.snapshot(),
        },
        runtime_telemetry: reactor_metrics::snapshot(),
    };
    println!(
        "RESULT {}",
        serde_json::to_string(&summary).map_err(|err| err.to_string())?
    );
    Ok(())
}

fn run_client(options: ClientOptions) -> Result<(), String> {
    reactor_metrics::reset();
    if options.trace_path.is_some() && options.connections != 1 {
        return Err("trace capture currently requires --connections 1".into());
    }
    let payload = vec![b'x'; options.payload_bytes];
    let (event_tx, event_rx) = unbounded();
    let mut sessions = Vec::with_capacity(options.connections);
    for index in 0..options.connections {
        let source = format!("client-{index}");
        let (batcher, sink_stats) = channel_batcher(source.clone(), event_tx.clone());
        let quiche_config = build_client_quiche_config(&options)?;
        let handle = spawn_quic_client_with_batcher(
            quiche_config,
            options.server_addr,
            options.server_name.clone(),
            None,
            None,
            None,
            options.common.runtime_mode,
            batcher,
        )
        .map_err(|err| err.to_string())?;
        sessions.push(ClientSessionState::new(source, handle, sink_stats));
    }
    drop(event_tx);

    let total_streams = options.connections * options.streams_per_connection;
    let start = Instant::now();
    let mut first_send_at: Option<Instant> = None;
    let mut latencies_ms = Vec::with_capacity(total_streams);
    let mut errors = Vec::new();
    let mut completed_streams = 0usize;
    let mut ready_connections = 0usize;
    let mut trace_commands = Vec::new();
    let mut trace_event_batches = Vec::new();

    while completed_streams < total_streams && start.elapsed() < options.timeout {
        match event_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(batch) => {
                let Some(session) = sessions
                    .iter_mut()
                    .find(|candidate| candidate.source == batch.source)
                else {
                    continue;
                };
                if let Some(trace_started_at) = first_send_at {
                    trace_event_batches.push(MockTraceEventBatch {
                        at_ms: trace_started_at.elapsed().as_millis() as u64,
                        event_count: batch.events.len(),
                        stream_bytes: batch
                            .events
                            .iter()
                            .filter_map(|event| event.data.as_ref())
                            .map(|data| data.len())
                            .sum(),
                    });
                }
                for event in batch.events {
                    match event.event_type {
                        EVENT_HANDSHAKE_COMPLETE => {
                            if !session.handshake_complete {
                                session.handshake_complete = true;
                                ready_connections += 1;
                            }
                            if !session.workload_sent {
                                let send_started = Instant::now();
                                if first_send_at.is_none() {
                                    first_send_at = Some(send_started);
                                }
                                for stream_index in 0..options.streams_per_connection {
                                    let stream_id = (stream_index as u64) * 4;
                                    if options.trace_path.is_some() {
                                        trace_commands.push(MockTraceCommand {
                                            at_ms: send_started.elapsed().as_millis() as u64,
                                            stream_id,
                                            bytes: payload.len(),
                                            fin: true,
                                        });
                                    }
                                    if session.handle.stream_send(stream_id, payload.clone(), true)
                                    {
                                        session.pending_streams.insert(stream_id, Instant::now());
                                    } else {
                                        errors.push(format!(
                                            "{} failed to enqueue stream {}",
                                            session.source, stream_id
                                        ));
                                    }
                                }
                                session.workload_sent = true;
                            }
                        }
                        EVENT_NEW_STREAM | EVENT_DATA => {
                            if let Some(data) = event.data.as_ref() {
                                session.response_bytes += data.len() as u64;
                            }
                            if event.fin == Some(true) {
                                completed_streams +=
                                    mark_stream_complete(session, &event, &mut latencies_ms);
                            }
                        }
                        EVENT_FINISHED => {
                            completed_streams +=
                                mark_stream_complete(session, &event, &mut latencies_ms);
                        }
                        EVENT_ERROR => {
                            errors.push(format!(
                                "{} conn={} stream={} {}",
                                session.source,
                                event.conn_handle,
                                event.stream_id,
                                event_error_text(&event)
                            ));
                        }
                        EVENT_SESSION_CLOSE => {
                            if session.completed_streams < options.streams_per_connection {
                                errors.push(format!(
                                    "{} closed early after {} of {} streams",
                                    session.source,
                                    session.completed_streams,
                                    options.streams_per_connection
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    for session in &sessions {
        let _ = session.handle.close(0, "profile-complete".into());
    }
    let close_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < close_deadline {
        match event_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(batch) => {
                for event in batch.events {
                    if event.event_type == EVENT_ERROR {
                        errors.push(format!(
                            "{} conn={} stream={} {}",
                            batch.source,
                            event.conn_handle,
                            event.stream_id,
                            event_error_text(&event)
                        ));
                    }
                }
            }
            Err(_) => break,
        }
    }
    for session in &mut sessions {
        session.handle.shutdown();
    }

    if let Some(trace_path) = options.trace_path.as_ref() {
        let trace = MockReplayTrace {
            trace_type: "mock-replay-trace".into(),
            harness: "quic-loopback".into(),
            protocol: "quic".into(),
            server_name: options.server_name.clone(),
            payload_bytes: options.payload_bytes,
            requested_streams: total_streams,
            commands: trace_commands,
            event_batches: trace_event_batches,
        };
        fs::write(
            trace_path,
            serde_json::to_vec_pretty(&trace).map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?;
    }

    let elapsed_ms = first_send_at.map_or_else(
        || start.elapsed().as_secs_f64() * 1000.0,
        |sent_at| sent_at.elapsed().as_secs_f64() * 1000.0,
    );
    let total_response_bytes = sessions.iter().map(|session| session.response_bytes).sum();
    let throughput_mbps = if elapsed_ms > 0.0 {
        (total_response_bytes as f64 * 8.0) / (elapsed_ms / 1000.0) / 1_000_000.0
    } else {
        0.0
    };
    let summary = ClientSummary {
        harness: "quic-loopback",
        role: "client",
        server_addr: options.server_addr.to_string(),
        runtime_mode: runtime_mode_as_str(options.common.runtime_mode).to_string(),
        alpn: options.common.alpn,
        connections: options.connections,
        streams_per_connection: options.streams_per_connection,
        payload_bytes: options.payload_bytes,
        requested_streams: total_streams,
        completed_streams,
        ready_connections,
        total_response_bytes,
        elapsed_ms,
        throughput_mbps,
        latency_ms: summarize_latencies(&latencies_ms),
        error_count: errors.len(),
        errors,
        sinks: sessions
            .iter()
            .map(|session| NamedSinkStats {
                source: session.source.clone(),
                stats: session.sink_stats.snapshot(),
            })
            .collect(),
        runtime_telemetry: reactor_metrics::snapshot(),
    };
    println!(
        "RESULT {}",
        serde_json::to_string(&summary).map_err(|err| err.to_string())?
    );
    Ok(())
}

fn mark_stream_complete(
    session: &mut ClientSessionState,
    event: &JsH3Event,
    latencies_ms: &mut Vec<f64>,
) -> usize {
    let Some(stream_id) = event_stream_id(event) else {
        return 0;
    };
    let Some(sent_at) = session.pending_streams.remove(&stream_id) else {
        return 0;
    };
    latencies_ms.push(sent_at.elapsed().as_secs_f64() * 1000.0);
    session.completed_streams += 1;
    1
}

fn event_stream_id(event: &JsH3Event) -> Option<u64> {
    (event.stream_id >= 0).then_some(event.stream_id as u64)
}

fn event_error_text(event: &JsH3Event) -> String {
    event
        .meta
        .as_ref()
        .and_then(|meta| meta.error_reason.clone())
        .unwrap_or_else(|| "unknown error".into())
}

fn summarize_latencies(samples: &[f64]) -> Option<LatencySummary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    Some(LatencySummary {
        min_ms: *sorted.first().unwrap_or(&0.0),
        avg_ms: sum / sorted.len() as f64,
        p50_ms: percentile(&sorted, 0.50),
        p95_ms: percentile(&sorted, 0.95),
        p99_ms: percentile(&sorted, 0.99),
        max_ms: *sorted.last().unwrap_or(&0.0),
    })
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    let last = sorted.len().saturating_sub(1);
    let index = ((last as f64) * percentile).round() as usize;
    sorted[index.min(last)]
}

fn runtime_mode_as_str(mode: TransportRuntimeMode) -> &'static str {
    match mode {
        TransportRuntimeMode::Fast => "fast",
        TransportRuntimeMode::Portable => "portable",
    }
}

fn build_server_config(options: &ServerOptions) -> Result<QuicServerConfig, String> {
    Ok(QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: 4_096,
        disable_retry: true,
        client_auth: ClientAuthMode::None,
        cid_encoding: CidEncoding::random(),
        runtime_mode: options.common.runtime_mode,
    })
}

fn build_server_quiche_config(options: &ServerOptions) -> Result<quiche::Config, String> {
    let cert = fs::read(&options.cert_path).map_err(|err| err.to_string())?;
    let key = fs::read(&options.key_path).map_err(|err| err.to_string())?;
    let server_options = JsQuicServerOptions {
        key: key.into(),
        cert: cert.into(),
        ca: None,
        client_auth: None,
        alpn: Some(vec![options.common.alpn.clone()]),
        runtime_mode: Some(runtime_mode_as_str(options.common.runtime_mode).into()),
        max_idle_timeout_ms: Some(DEFAULT_TIMEOUT_MS as u32),
        max_udp_payload_size: None,
        initial_max_data: None,
        initial_max_stream_data_bidi_local: None,
        initial_max_streams_bidi: None,
        disable_active_migration: Some(true),
        enable_datagrams: Some(false),
        max_connections: Some(4_096),
        disable_retry: Some(true),
        qlog_dir: None,
        qlog_level: None,
        session_ticket_keys: None,
        keylog: Some(false),
    };
    new_quic_server_config(&server_options).map_err(|err| err.to_string())
}

fn build_client_quiche_config(options: &ClientOptions) -> Result<quiche::Config, String> {
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: Some(vec![options.common.alpn.clone()]),
        runtime_mode: Some(runtime_mode_as_str(options.common.runtime_mode).into()),
        max_idle_timeout_ms: Some(DEFAULT_TIMEOUT_MS as u32),
        max_udp_payload_size: None,
        initial_max_data: None,
        initial_max_stream_data_bidi_local: None,
        initial_max_streams_bidi: None,
        session_ticket: None,
        allow_0rtt: Some(false),
        enable_datagrams: Some(false),
        keylog: Some(false),
        qlog_dir: None,
        qlog_level: None,
    };
    new_quic_client_config(&client_options).map_err(|err| err.to_string())
}

fn parse_cli(args: Vec<String>) -> Result<CliCommand, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(help_text());
    };
    match command {
        "server" => parse_server_args(&args[1..]).map(CliCommand::Server),
        "client" => parse_client_args(&args[1..]).map(CliCommand::Client),
        "--help" | "-h" | "help" => Err(help_text()),
        other => Err(format!("unknown subcommand: {other}\n\n{}", help_text())),
    }
}

fn parse_server_args(args: &[String]) -> Result<ServerOptions, String> {
    let mut bind_addr = "127.0.0.1:0".to_string();
    let mut runtime_mode = "fast".to_string();
    let mut alpn = DEFAULT_ALPN.to_string();
    let mut cert_path = None;
    let mut key_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--bind" => {
                bind_addr = next_value(args, &mut index, "--bind")?;
            }
            "--runtime-mode" => {
                runtime_mode = next_value(args, &mut index, "--runtime-mode")?;
            }
            "--alpn" => {
                alpn = next_value(args, &mut index, "--alpn")?;
            }
            "--cert-path" => {
                cert_path = Some(next_value(args, &mut index, "--cert-path")?);
            }
            "--key-path" => {
                key_path = Some(next_value(args, &mut index, "--key-path")?);
            }
            "--help" | "-h" => return Err(help_text()),
            flag => return Err(format!("unknown server flag: {flag}\n\n{}", help_text())),
        }
        index += 1;
    }
    Ok(ServerOptions {
        common: CommonOptions {
            runtime_mode: TransportRuntimeMode::parse(Some(&runtime_mode))
                .map_err(|err| err.to_string())?,
            alpn,
        },
        bind_addr: bind_addr
            .parse()
            .map_err(|err| format!("invalid --bind value: {err}"))?,
        cert_path: cert_path.ok_or_else(|| "--cert-path is required".to_string())?,
        key_path: key_path.ok_or_else(|| "--key-path is required".to_string())?,
    })
}

fn parse_client_args(args: &[String]) -> Result<ClientOptions, String> {
    let mut server_addr = None;
    let mut runtime_mode = "fast".to_string();
    let mut alpn = DEFAULT_ALPN.to_string();
    let mut server_name = DEFAULT_SERVER_NAME.to_string();
    let mut connections = 1usize;
    let mut streams_per_connection = 100usize;
    let mut payload_bytes = 16 * 1024usize;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut trace_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--server-addr" => {
                server_addr = Some(next_value(args, &mut index, "--server-addr")?);
            }
            "--runtime-mode" => {
                runtime_mode = next_value(args, &mut index, "--runtime-mode")?;
            }
            "--alpn" => {
                alpn = next_value(args, &mut index, "--alpn")?;
            }
            "--server-name" => {
                server_name = next_value(args, &mut index, "--server-name")?;
            }
            "--connections" => {
                connections = next_value(args, &mut index, "--connections")?
                    .parse()
                    .map_err(|err| format!("invalid --connections value: {err}"))?;
            }
            "--streams-per-connection" => {
                streams_per_connection = next_value(args, &mut index, "--streams-per-connection")?
                    .parse()
                    .map_err(|err| format!("invalid --streams-per-connection value: {err}"))?;
            }
            "--payload-bytes" => {
                payload_bytes = next_value(args, &mut index, "--payload-bytes")?
                    .parse()
                    .map_err(|err| format!("invalid --payload-bytes value: {err}"))?;
            }
            "--timeout-ms" => {
                timeout_ms = next_value(args, &mut index, "--timeout-ms")?
                    .parse()
                    .map_err(|err| format!("invalid --timeout-ms value: {err}"))?;
            }
            "--trace-path" => {
                trace_path = Some(next_value(args, &mut index, "--trace-path")?);
            }
            "--help" | "-h" => return Err(help_text()),
            flag => return Err(format!("unknown client flag: {flag}\n\n{}", help_text())),
        }
        index += 1;
    }
    Ok(ClientOptions {
        common: CommonOptions {
            runtime_mode: TransportRuntimeMode::parse(Some(&runtime_mode))
                .map_err(|err| err.to_string())?,
            alpn,
        },
        server_addr: server_addr
            .ok_or_else(|| "--server-addr is required".to_string())?
            .parse()
            .map_err(|err| format!("invalid --server-addr value: {err}"))?,
        server_name,
        connections,
        streams_per_connection,
        payload_bytes,
        timeout: Duration::from_millis(timeout_ms),
        trace_path,
    })
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    let next_index = *index + 1;
    let Some(value) = args.get(next_index) else {
        return Err(format!("{flag} requires a value"));
    };
    *index = next_index;
    Ok(value.clone())
}

fn help_text() -> String {
    r#"quic_loopback_profile

Usage:
  quic_loopback_profile server --cert-path FILE --key-path FILE [options]
  quic_loopback_profile client --server-addr HOST:PORT [options]

Server options:
  --bind HOST:PORT                Bind address (default: 127.0.0.1:0)
  --runtime-mode fast|portable    Driver mode (default: fast)
  --alpn VALUE                    ALPN string (default: quic)
  --cert-path FILE                PEM certificate path
  --key-path FILE                 PEM private key path

Client options:
  --server-addr HOST:PORT         Target server address
  --server-name VALUE             SNI / hostname (default: localhost)
  --runtime-mode fast|portable    Driver mode (default: fast)
  --alpn VALUE                    ALPN string (default: quic)
  --connections N                 Number of client sessions (default: 1)
  --streams-per-connection N      Streams per session (default: 100)
  --payload-bytes N               Bytes per request stream (default: 16384)
  --timeout-ms N                  End-to-end timeout (default: 30000)
  --trace-path FILE               Write replayable command/event trace JSON

Protocol:
  Server prints `READY <addr>` once the worker is listening.
  Send `stop\n` on stdin to terminate the server gracefully.
  Both modes print `RESULT <json>` on completion."#
        .into()
}
