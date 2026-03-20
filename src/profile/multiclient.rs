#![allow(clippy::too_many_lines)]

//! In-process multi-client benchmark: one io_uring server + N io_uring
//! clients, all sharing the same process.  No subprocess orchestration.
//!
//! Reports per-client and aggregate stats including congestion control
//! observability (cwnd, bytes_in_flight, delivery_rate from quiche
//! path_stats).

use std::collections::HashMap;
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
use crate::quic_worker::{
    QuicClientHandle, QuicServerCommand, QuicServerConfig, spawn_quic_client_with_batcher,
    spawn_quic_server_with_batcher,
};
use crate::reactor_metrics::{self, JsReactorTelemetrySnapshot};

const DEFAULT_ALPN: &str = "quic";
const DEFAULT_SERVER_NAME: &str = "localhost";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

// ── Output types ────────────────────────────────────────────────────

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
struct CongestionSnapshot {
    cwnd: u64,
    rtt_ms: f64,
    pmtu: i64,
}

#[derive(Serialize)]
struct NamedSinkStats {
    source: String,
    stats: EventBatcherStatsSnapshot,
}

#[derive(Serialize)]
struct PerClientSummary {
    name: String,
    completed_streams: usize,
    response_bytes: u64,
    latency_ms: Option<LatencySummary>,
    congestion: Option<CongestionSnapshot>,
    error_count: usize,
    errors: Vec<String>,
}

#[derive(Serialize)]
struct BenchmarkSummary {
    harness: &'static str,
    runtime_mode: String,
    clients: usize,
    streams_per_client: usize,
    payload_bytes: usize,
    requested_streams: usize,
    completed_streams: usize,
    total_response_bytes: u64,
    elapsed_ms: f64,
    throughput_mbps: f64,
    aggregate_latency_ms: Option<LatencySummary>,
    server_sessions_opened: u64,
    server_echoed_streams: u64,
    server_echoed_bytes: u64,
    per_client: Vec<PerClientSummary>,
    server_errors: Vec<String>,
    server_sink: NamedSinkStats,
    runtime_telemetry: JsReactorTelemetrySnapshot,
}

// ── Server echo state ───────────────────────────────────────────────

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

    fn flush_echo(
        &mut self,
        server: &crate::quic_worker::QuicServerHandle,
        key: (u32, u64),
    ) {
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

// ── Client session state ────────────────────────────────────────────

struct ClientSessionState {
    source: String,
    handle: QuicClientHandle,
    sink_stats: crate::event_loop::EventBatcherStatsHandle,
    handshake_complete: bool,
    workload_sent: bool,
    pending_streams: HashMap<u64, Instant>,
    completed_streams: usize,
    response_bytes: u64,
    latencies_ms: Vec<f64>,
    errors: Vec<String>,
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
            latencies_ms: Vec::new(),
            errors: Vec::new(),
        }
    }
}

// ── Options ─────────────────────────────────────────────────────────

struct BenchOptions {
    runtime_mode: TransportRuntimeMode,
    alpn: String,
    server_name: String,
    clients: usize,
    streams_per_client: usize,
    payload_bytes: usize,
    timeout: Duration,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

// ── Main entry point ────────────────────────────────────────────────

pub fn cli_main() -> Result<(), String> {
    let options = parse_cli(std::env::args().skip(1).collect())?;
    run_benchmark(options)
}

fn run_benchmark(options: BenchOptions) -> Result<(), String> {
    reactor_metrics::reset();
    let payload = vec![b'x'; options.payload_bytes];
    let total_streams = options.clients * options.streams_per_client;

    // ── Start server ────────────────────────────────────────────────
    let (server_event_tx, server_event_rx) = unbounded();
    let (server_batcher, server_sink_stats) = channel_batcher("server", server_event_tx);
    let server_config = QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: 4_096,
        disable_retry: true,
        client_auth: ClientAuthMode::None,
        cid_encoding: CidEncoding::random(),
        runtime_mode: options.runtime_mode,
    };
    let server_quiche_config = build_server_quiche_config(&options)?;
    let mut server = spawn_quic_server_with_batcher(
        server_quiche_config,
        server_config,
        "127.0.0.1:0".parse().expect("valid addr"),
        server_batcher,
    )
    .map_err(|err| err.to_string())?;
    let server_addr = server.local_addr();
    eprintln!("server listening on {server_addr}");

    // ── Start clients ───────────────────────────────────────────────
    let (client_event_tx, client_event_rx) = unbounded();
    let mut sessions = Vec::with_capacity(options.clients);
    for index in 0..options.clients {
        let source = format!("client-{index}");
        let (batcher, sink_stats) = channel_batcher(source.clone(), client_event_tx.clone());
        let quiche_config = build_client_quiche_config(&options)?;
        let handle = spawn_quic_client_with_batcher(
            quiche_config,
            server_addr,
            options.server_name.clone(),
            None,
            None,
            None,
            options.runtime_mode,
            batcher,
        )
        .map_err(|err| err.to_string())?;
        sessions.push(ClientSessionState::new(source, handle, sink_stats));
    }
    drop(client_event_tx);

    // ── Event loop ──────────────────────────────────────────────────
    let start = Instant::now();
    let mut first_send_at: Option<Instant> = None;
    let mut echo_state = ServerEchoState::new();
    let mut completed_streams = 0usize;
    let mut ready_connections = 0usize;

    while completed_streams < total_streams && start.elapsed() < options.timeout {
        // Drain server events
        while let Ok(batch) = server_event_rx.try_recv() {
            echo_state.handle_batch(&server, batch);
        }

        // Block until at least one client event, then drain all pending
        let first_batch = client_event_rx.recv_timeout(Duration::from_millis(10));
        let batches: Vec<TaggedEventBatch> = match first_batch {
            Ok(batch) => {
                let mut all = vec![batch];
                while let Ok(more) = client_event_rx.try_recv() {
                    all.push(more);
                }
                all
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        for batch in batches {
            let Some(session) = sessions
                .iter_mut()
                .find(|c| c.source == batch.source)
            else {
                continue;
            };
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
                            for stream_index in 0..options.streams_per_client {
                                let stream_id = (stream_index as u64) * 4;
                                if session
                                    .handle
                                    .stream_send(stream_id, payload.clone(), true)
                                {
                                    session
                                        .pending_streams
                                        .insert(stream_id, Instant::now());
                                } else {
                                    session.errors.push(format!(
                                        "failed to enqueue stream {stream_id}"
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
                            mark_stream_complete(session, &event);
                        }
                    }
                    EVENT_FINISHED => {
                        mark_stream_complete(session, &event);
                    }
                    EVENT_ERROR => {
                        session.errors.push(format!(
                            "conn={} stream={} {}",
                            event.conn_handle,
                            event.stream_id,
                            event_error_text(&event)
                        ));
                    }
                    EVENT_SESSION_CLOSE => {
                        if session.completed_streams < options.streams_per_client {
                            session.errors.push(format!(
                                "closed early after {} of {} streams",
                                session.completed_streams, options.streams_per_client
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        completed_streams = sessions.iter().map(|s| s.completed_streams).sum();
    }

    // ── Collect CC stats before teardown ─────────────────────────────
    let mut congestion_snapshots: HashMap<String, CongestionSnapshot> = HashMap::new();
    for session in &sessions {
        if let Ok(Some(metrics)) = session.handle.get_session_metrics() {
            congestion_snapshots.insert(
                session.source.clone(),
                CongestionSnapshot {
                    cwnd: metrics.cwnd as u64,
                    rtt_ms: metrics.rtt_ms,
                    pmtu: metrics.pmtu,
                },
            );
        }
    }

    // ── Graceful shutdown ───────────────────────────────────────────
    for session in &sessions {
        let _ = session.handle.close(0, "benchmark-complete".into());
    }
    let close_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < close_deadline {
        // Drain remaining server events
        while let Ok(batch) = server_event_rx.try_recv() {
            echo_state.handle_batch(&server, batch);
        }
        match client_event_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(_) => {}
            Err(_) => break,
        }
    }
    for session in &mut sessions {
        session.handle.shutdown();
    }
    server.shutdown();

    // ── Build summary ───────────────────────────────────────────────
    let elapsed_ms = first_send_at.map_or_else(
        || start.elapsed().as_secs_f64() * 1000.0,
        |sent_at| sent_at.elapsed().as_secs_f64() * 1000.0,
    );
    let total_response_bytes: u64 = sessions.iter().map(|s| s.response_bytes).sum();
    let throughput_mbps = if elapsed_ms > 0.0 {
        (total_response_bytes as f64 * 8.0) / (elapsed_ms / 1000.0) / 1_000_000.0
    } else {
        0.0
    };

    let all_latencies: Vec<f64> = sessions
        .iter()
        .flat_map(|s| s.latencies_ms.iter().copied())
        .collect();

    let per_client: Vec<PerClientSummary> = sessions
        .iter()
        .map(|s| PerClientSummary {
            name: s.source.clone(),
            completed_streams: s.completed_streams,
            response_bytes: s.response_bytes,
            latency_ms: summarize_latencies(&s.latencies_ms),
            congestion: congestion_snapshots.remove(&s.source),
            error_count: s.errors.len(),
            errors: s.errors.clone(),
        })
        .collect();

    let summary = BenchmarkSummary {
        harness: "quic-multiclient",
        runtime_mode: runtime_mode_as_str(options.runtime_mode).to_string(),
        clients: options.clients,
        streams_per_client: options.streams_per_client,
        payload_bytes: options.payload_bytes,
        requested_streams: total_streams,
        completed_streams,
        total_response_bytes,
        elapsed_ms,
        throughput_mbps,
        aggregate_latency_ms: summarize_latencies(&all_latencies),
        server_sessions_opened: echo_state.sessions_opened,
        server_echoed_streams: echo_state.echoed_streams,
        server_echoed_bytes: echo_state.echoed_bytes,
        per_client,
        server_errors: echo_state.errors,
        server_sink: NamedSinkStats {
            source: "server".into(),
            stats: server_sink_stats.snapshot(),
        },
        runtime_telemetry: reactor_metrics::snapshot(),
    };
    println!(
        "RESULT {}",
        serde_json::to_string_pretty(&summary).map_err(|err| err.to_string())?
    );
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

fn mark_stream_complete(session: &mut ClientSessionState, event: &JsH3Event) {
    let Some(stream_id) = event_stream_id(event) else {
        return;
    };
    let Some(sent_at) = session.pending_streams.remove(&stream_id) else {
        return;
    };
    session
        .latencies_ms
        .push(sent_at.elapsed().as_secs_f64() * 1000.0);
    session.completed_streams += 1;
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

fn generate_self_signed_cert() -> Result<(Vec<u8>, Vec<u8>), String> {
    use rcgen::{CertificateParams, KeyPair};
    let key_pair =
        KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".try_into().map_err(|e: rcgen::Error| e.to_string())?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| e.to_string())?;
    Ok((cert.pem().into_bytes(), key_pair.serialize_pem().into_bytes()))
}

fn build_server_quiche_config(options: &BenchOptions) -> Result<quiche::Config, String> {
    let server_options = JsQuicServerOptions {
        key: options.key_pem.clone().into(),
        cert: options.cert_pem.clone().into(),
        ca: None,
        client_auth: None,
        alpn: Some(vec![options.alpn.clone()]),
        runtime_mode: Some(runtime_mode_as_str(options.runtime_mode).into()),
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

fn build_client_quiche_config(options: &BenchOptions) -> Result<quiche::Config, String> {
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: Some(vec![options.alpn.clone()]),
        runtime_mode: Some(runtime_mode_as_str(options.runtime_mode).into()),
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

// ── CLI parsing ─────────────────────────────────────────────────────

fn parse_cli(args: Vec<String>) -> Result<BenchOptions, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Err(help_text());
    }
    let mut runtime_mode = "fast".to_string();
    let mut alpn = DEFAULT_ALPN.to_string();
    let mut server_name = DEFAULT_SERVER_NAME.to_string();
    let mut clients = 4usize;
    let mut streams_per_client = 100usize;
    let mut payload_bytes = 16 * 1024usize;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut cert_path: Option<String> = None;
    let mut key_path: Option<String> = None;

    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--runtime-mode" => {
                runtime_mode = next_value(&args, &mut index, "--runtime-mode")?;
            }
            "--alpn" => {
                alpn = next_value(&args, &mut index, "--alpn")?;
            }
            "--server-name" => {
                server_name = next_value(&args, &mut index, "--server-name")?;
            }
            "--clients" => {
                clients = next_value(&args, &mut index, "--clients")?
                    .parse()
                    .map_err(|err| format!("invalid --clients value: {err}"))?;
            }
            "--streams-per-client" => {
                streams_per_client = next_value(&args, &mut index, "--streams-per-client")?
                    .parse()
                    .map_err(|err| format!("invalid --streams-per-client value: {err}"))?;
            }
            "--payload-bytes" => {
                payload_bytes = next_value(&args, &mut index, "--payload-bytes")?
                    .parse()
                    .map_err(|err| format!("invalid --payload-bytes value: {err}"))?;
            }
            "--timeout-ms" => {
                timeout_ms = next_value(&args, &mut index, "--timeout-ms")?
                    .parse()
                    .map_err(|err| format!("invalid --timeout-ms value: {err}"))?;
            }
            "--cert-path" => {
                cert_path = Some(next_value(&args, &mut index, "--cert-path")?);
            }
            "--key-path" => {
                key_path = Some(next_value(&args, &mut index, "--key-path")?);
            }
            "--help" | "-h" => return Err(help_text()),
            flag => return Err(format!("unknown flag: {flag}\n\n{}", help_text())),
        }
        index += 1;
    }

    let (cert_pem, key_pem) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            let cert_pem =
                std::fs::read(&cert).map_err(|err| format!("reading cert: {err}"))?;
            let key_pem = std::fs::read(&key).map_err(|err| format!("reading key: {err}"))?;
            (cert_pem, key_pem)
        }
        (None, None) => {
            eprintln!("no --cert-path/--key-path, generating self-signed certificate");
            generate_self_signed_cert()?
        }
        _ => {
            return Err("--cert-path and --key-path must both be provided or both omitted".into());
        }
    };

    Ok(BenchOptions {
        runtime_mode: TransportRuntimeMode::parse(Some(&runtime_mode))
            .map_err(|err| err.to_string())?,
        alpn,
        server_name,
        clients,
        streams_per_client,
        payload_bytes,
        timeout: Duration::from_millis(timeout_ms),
        cert_pem,
        key_pem,
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
    r#"quic_multiclient_bench

In-process multi-client benchmark: one server + N clients, all io_uring.
Generates a self-signed certificate if --cert-path/--key-path are not provided.

Usage:
  quic_multiclient_bench [options]

Options:
  --runtime-mode fast|portable    Driver mode (default: fast)
  --clients N                     Number of client sessions (default: 4)
  --streams-per-client N          Streams per client (default: 100)
  --payload-bytes N               Bytes per request stream (default: 16384)
  --timeout-ms N                  End-to-end timeout (default: 30000)
  --server-name VALUE             SNI / hostname (default: localhost)
  --alpn VALUE                    ALPN string (default: quic)
  --cert-path FILE                PEM certificate path (optional)
  --key-path FILE                 PEM private key path (optional)

Output:
  Prints `RESULT <json>` on completion with per-client and aggregate stats,
  including congestion control snapshots (cwnd, rtt_ms)."#
        .into()
}
