#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::Serialize;

use crate::cid::CidEncoding;
use crate::config::{
    ClientAuthMode, JsQuicClientOptions, JsQuicServerOptions, TransportRuntimeMode,
    new_quic_client_config, new_quic_server_config,
};
use crate::event_loop::EventBatcherStatsSnapshot;
#[cfg(feature = "node-api")]
use crate::event_loop::EventTsfn;
use crate::h3_event::{
    EVENT_DATA, EVENT_ERROR, EVENT_FINISHED, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION,
    EVENT_NEW_STREAM, EVENT_SESSION_CLOSE, JsH3Event,
};
use crate::profile::event_sink::{
    TaggedEventBatch, channel_and_counting_batcher, channel_batcher,
};
#[cfg(feature = "node-api")]
use crate::profile::event_sink::channel_and_tsfn_batcher;
use crate::profile::mock_trace::MockReplayTrace;
use crate::quic_worker::{
    QuicClientHandle, QuicServerCommand, QuicServerConfig, QuicServerHandle, QuicServerWorker,
    spawn_dedicated_quic_client_on_driver, spawn_server_worker_on_driver,
};
use crate::reactor_metrics::{self, JsReactorTelemetrySnapshot};
use crate::transport::mock::{MockDriver, MockTraceRecorder};

const DEFAULT_SERVER_NAME: &str = "localhost";

#[derive(Clone, Copy, Debug)]
pub(crate) enum MockClientSinkMode {
    Counting,
    #[cfg(feature = "node-api")]
    Tsfn,
}

impl MockClientSinkMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counting => "channel+counting",
            #[cfg(feature = "node-api")]
            Self::Tsfn => "channel+tsfn",
        }
    }
}

pub(crate) struct MockQuicProfileConfig {
    pub server_options: JsQuicServerOptions,
    pub client_options: JsQuicClientOptions,
    pub server_name: String,
    pub configured_streams_per_connection: usize,
    pub configured_payload_bytes: usize,
    pub streams_per_connection: usize,
    pub payload_bytes: usize,
    pub response_fragment_count: usize,
    pub response_fragment_size: Option<usize>,
    pub timeout: Duration,
    pub client_sink_mode: MockClientSinkMode,
    pub replay_trace: Option<MockReplayTrace>,
}

pub(crate) struct MockQuicProfileRunner {
    stop_tx: Sender<()>,
    result_rx: Receiver<Result<String, String>>,
    join_handle: Option<JoinHandle<()>>,
    cached_result: Option<Result<String, String>>,
}

impl MockQuicProfileRunner {
    pub(crate) fn poll_result_json(&mut self) -> Option<Result<String, String>> {
        if self.cached_result.is_none() {
            match self.result_rx.try_recv() {
                Ok(result) => {
                    self.cached_result = Some(result);
                    if let Some(join_handle) = self.join_handle.take() {
                        let _ = join_handle.join();
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.cached_result = Some(Err("mock profile worker disconnected".into()));
                    if let Some(join_handle) = self.join_handle.take() {
                        let _ = join_handle.join();
                    }
                }
            }
        }

        self.cached_result.clone()
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub(crate) fn spawn_mock_quic_profile(
    config: MockQuicProfileConfig,
    #[cfg(feature = "node-api")] tsfn: Option<EventTsfn>,
) -> MockQuicProfileRunner {
    let (stop_tx, stop_rx) = bounded(1);
    let (result_tx, result_rx) = bounded(1);
    let join_handle = thread::spawn(move || {
        let result = run_mock_quic_profile(
            config,
            stop_rx,
            #[cfg(feature = "node-api")]
            tsfn,
        );
        let _ = result_tx.send(result);
    });

    MockQuicProfileRunner {
        stop_tx,
        result_rx,
        join_handle: Some(join_handle),
        cached_result: None,
    }
}

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
struct MockTraceSummary {
    datagrams: usize,
    bytes: usize,
}

#[derive(Serialize)]
struct MockQuicProfileSummary {
    harness: &'static str,
    role: &'static str,
    transport_driver: &'static str,
    sink_type: &'static str,
    workload_mode: &'static str,
    server_name: String,
    configured_streams_per_connection: usize,
    configured_payload_bytes: usize,
    streams_per_connection: usize,
    payload_bytes: usize,
    requested_streams: usize,
    response_fragment_count: usize,
    response_fragment_size: Option<usize>,
    completed_streams: usize,
    total_response_bytes: u64,
    ready_connections: usize,
    elapsed_ms: f64,
    throughput_mbps: f64,
    latency_ms: Option<LatencySummary>,
    error_count: usize,
    errors: Vec<String>,
    trace: MockTraceSummary,
    sinks: Vec<NamedSinkStats>,
    runtime_telemetry: JsReactorTelemetrySnapshot,
}

struct ServerEchoState {
    pending: HashMap<(u32, u64), Vec<u8>>,
    response_fragment_count: usize,
    response_fragment_size: Option<usize>,
    sessions_opened: u64,
    sessions_closed: u64,
    echoed_streams: u64,
    echoed_bytes: u64,
    errors: Vec<String>,
}

impl ServerEchoState {
    fn new(response_fragment_count: usize, response_fragment_size: Option<usize>) -> Self {
        Self {
            pending: HashMap::new(),
            response_fragment_count,
            response_fragment_size,
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
        let fragments = fragment_response(
            body,
            self.response_fragment_count,
            self.response_fragment_size,
        );
        let last_index = fragments.len().saturating_sub(1);
        for (index, fragment) in fragments.into_iter().enumerate() {
            let _ = server.send_command(QuicServerCommand::StreamSend {
                conn_handle: key.0,
                stream_id: key.1,
                data: fragment,
                fin: index == last_index,
            });
        }
        self.echoed_streams += 1;
        self.echoed_bytes += body_len;
    }
}

struct ClientSessionState {
    handle: QuicClientHandle,
    sink_source: String,
    sink_stats: crate::event_loop::EventBatcherStatsHandle,
    handshake_complete: bool,
    workload_sent: bool,
    pending_streams: HashMap<u64, Instant>,
    completed_streams: usize,
    response_bytes: u64,
    next_trace_command: usize,
    trace_started_at: Option<Instant>,
}

impl ClientSessionState {
    fn new(
        handle: QuicClientHandle,
        sink_source: String,
        sink_stats: crate::event_loop::EventBatcherStatsHandle,
    ) -> Self {
        Self {
            handle,
            sink_source,
            sink_stats,
            handshake_complete: false,
            workload_sent: false,
            pending_streams: HashMap::new(),
            completed_streams: 0,
            response_bytes: 0,
            next_trace_command: 0,
            trace_started_at: None,
        }
    }
}

fn run_mock_quic_profile(
    config: MockQuicProfileConfig,
    stop_rx: Receiver<()>,
    #[cfg(feature = "node-api")] tsfn: Option<EventTsfn>,
) -> Result<String, String> {
    reactor_metrics::reset();

    let mut server_options = config.server_options;
    server_options.runtime_mode = Some("portable".into());
    let server_quiche = new_quic_server_config(&server_options).map_err(|err| err.to_string())?;
    let mut client_options = config.client_options;
    client_options.runtime_mode = Some("portable".into());
    let client_quiche = new_quic_client_config(&client_options).map_err(|err| err.to_string())?;

    let server_config = QuicServerConfig {
        qlog_dir: None,
        qlog_level: None,
        max_connections: 128,
        disable_retry: true,
        client_auth: ClientAuthMode::None,
        cid_encoding: CidEncoding::random(),
        runtime_mode: TransportRuntimeMode::Portable,
    };

    let client_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_000);
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_001);
    let trace = MockTraceRecorder::default();
    let ((client_driver, client_waker), (server_driver, server_waker)) =
        MockDriver::pair_with_trace(client_addr, server_addr, Some(trace.clone()));

    let (server_event_tx, server_event_rx) = unbounded();
    let (client_event_tx, client_event_rx) = unbounded();
    let (server_batcher, server_sink_stats) = channel_batcher("server", server_event_tx);
    let (client_batcher, client_sink_stats) = match config.client_sink_mode {
        MockClientSinkMode::Counting => {
            channel_and_counting_batcher("client-0", client_event_tx)
        }
        #[cfg(feature = "node-api")]
        MockClientSinkMode::Tsfn => channel_and_tsfn_batcher(
            "client-0",
            client_event_tx,
            tsfn.ok_or_else(|| "tsfn sink requested without callback".to_string())?,
        ),
    };

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
    let client = spawn_dedicated_quic_client_on_driver(
        client_quiche,
        server_addr,
        if config.server_name.is_empty() {
            DEFAULT_SERVER_NAME.to_string()
        } else {
            config.server_name.clone()
        },
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
    .map_err(|err| err.to_string())?;
    let mut session = ClientSessionState::new(client, "client-0".into(), client_sink_stats);

    let payload = vec![b'x'; config.payload_bytes];
    let replay_trace = config.replay_trace;
    let start = Instant::now();
    let mut first_send_at = None;
    let mut latencies_ms = Vec::with_capacity(config.streams_per_connection);
    let mut errors = Vec::new();
    let mut ready_connections = 0usize;
    let mut server_echo_state =
        ServerEchoState::new(config.response_fragment_count, config.response_fragment_size);

    while session.completed_streams < config.streams_per_connection && start.elapsed() < config.timeout
    {
        crossbeam_channel::select! {
            recv(stop_rx) -> _ => {
                errors.push("mock profile stopped before completion".into());
                break;
            }
            recv(server_event_rx) -> msg => {
                match msg {
                    Ok(batch) => server_echo_state.handle_batch(&server, batch),
                    Err(_) => break,
                }
            }
            recv(client_event_rx) -> msg => {
                let Ok(batch) = msg else {
                    break;
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
                                if let Some(trace) = replay_trace.as_ref() {
                                    session.trace_started_at = Some(send_started);
                                    drain_trace_commands(
                                        &mut session,
                                        trace,
                                        &mut errors,
                                    );
                                } else {
                                    for stream_index in 0..config.streams_per_connection {
                                        let stream_id = (stream_index as u64) * 4;
                                        if session.handle.stream_send(stream_id, payload.clone(), true) {
                                            session.pending_streams.insert(stream_id, Instant::now());
                                        } else {
                                            errors.push(format!(
                                                "{} failed to enqueue stream {stream_id}",
                                                session.sink_source
                                            ));
                                        }
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
                                mark_stream_complete(&mut session, &event, &mut latencies_ms);
                            }
                        }
                        EVENT_FINISHED => {
                            mark_stream_complete(&mut session, &event, &mut latencies_ms);
                        }
                        EVENT_ERROR => {
                            errors.push(format!(
                                "{} conn={} stream={} {}",
                                session.sink_source,
                                event.conn_handle,
                                event.stream_id,
                                event_error_text(&event)
                            ));
                        }
                        EVENT_SESSION_CLOSE => {
                            if session.completed_streams < config.streams_per_connection {
                                errors.push(format!(
                                    "{} closed early after {} of {} streams",
                                    session.sink_source,
                                    session.completed_streams,
                                    config.streams_per_connection
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            default(Duration::from_millis(25)) => {}
        }

        if let Some(trace) = replay_trace.as_ref() {
            if session.handshake_complete {
                drain_trace_commands(&mut session, trace, &mut errors);
            }
        }
    }

    if session.completed_streams < config.streams_per_connection {
        errors.push(format!(
            "completed {} of {} streams before timeout",
            session.completed_streams, config.streams_per_connection
        ));
    }

    session.handle.shutdown();
    server.shutdown();

    let send_started = first_send_at.unwrap_or(start);
    let elapsed = send_started.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    let throughput_mbps = if elapsed.is_zero() {
        0.0
    } else {
        (session.response_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0
    };
    let trace_snapshot = trace.snapshot();
    let trace_bytes = trace_snapshot.iter().map(|packet| packet.bytes.len()).sum();
    let summary = MockQuicProfileSummary {
        harness: "quic-mock",
        role: "pair",
        transport_driver: "mock",
        sink_type: config.client_sink_mode.as_str(),
        workload_mode: if replay_trace.is_some() {
            "replay"
        } else {
            "synthetic"
        },
        server_name: config.server_name,
        configured_streams_per_connection: config.configured_streams_per_connection,
        configured_payload_bytes: config.configured_payload_bytes,
        streams_per_connection: config.streams_per_connection,
        payload_bytes: config.payload_bytes,
        requested_streams: config.streams_per_connection,
        response_fragment_count: config.response_fragment_count,
        response_fragment_size: config.response_fragment_size,
        completed_streams: session.completed_streams,
        total_response_bytes: session.response_bytes,
        ready_connections,
        elapsed_ms,
        throughput_mbps,
        latency_ms: summarize_latencies(&latencies_ms),
        error_count: errors.len() + server_echo_state.errors.len(),
        errors: errors
            .into_iter()
            .chain(server_echo_state.errors)
            .collect(),
        trace: MockTraceSummary {
            datagrams: trace_snapshot.len(),
            bytes: trace_bytes,
        },
        sinks: vec![
            NamedSinkStats {
                source: "server".into(),
                stats: server_sink_stats.snapshot(),
            },
            NamedSinkStats {
                source: session.sink_source,
                stats: session.sink_stats.snapshot(),
            },
        ],
        runtime_telemetry: reactor_metrics::snapshot(),
    };

    serde_json::to_string(&summary).map_err(|err| err.to_string())
}

fn mark_stream_complete(
    session: &mut ClientSessionState,
    event: &JsH3Event,
    latencies_ms: &mut Vec<f64>,
) {
    let Some(stream_id) = event_stream_id(event) else {
        return;
    };
    if let Some(started_at) = session.pending_streams.remove(&stream_id) {
        latencies_ms.push(started_at.elapsed().as_secs_f64() * 1000.0);
        session.completed_streams += 1;
    }
}

fn drain_trace_commands(
    session: &mut ClientSessionState,
    trace: &MockReplayTrace,
    errors: &mut Vec<String>,
) {
    let Some(trace_started_at) = session.trace_started_at else {
        return;
    };
    let elapsed_ms = trace_started_at.elapsed().as_millis() as u64;
    while let Some(command) = trace.commands.get(session.next_trace_command) {
        if command.at_ms > elapsed_ms {
            break;
        }
        let data = vec![b'x'; command.bytes];
        if session
            .handle
            .stream_send(command.stream_id, data, command.fin)
        {
            session
                .pending_streams
                .insert(command.stream_id, Instant::now());
        } else {
            errors.push(format!(
                "{} failed to enqueue replayed stream {}",
                session.sink_source, command.stream_id
            ));
        }
        session.next_trace_command += 1;
    }
}

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

fn summarize_latencies(samples_ms: &[f64]) -> Option<LatencySummary> {
    if samples_ms.is_empty() {
        return None;
    }
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let sum: f64 = sorted.iter().copied().sum();
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
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    let last = sorted.len() - 1;
    sorted[index.min(last)]
}

fn fragment_response(
    body: Vec<u8>,
    fragment_count: usize,
    fragment_size: Option<usize>,
) -> Vec<Vec<u8>> {
    if body.is_empty() {
        return vec![Vec::new()];
    }

    if let Some(fragment_size) = fragment_size.filter(|value| *value > 0) {
        return body
            .chunks(fragment_size)
            .map(|chunk| chunk.to_vec())
            .collect();
    }

    if fragment_count <= 1 || fragment_count >= body.len() {
        return vec![body];
    }

    let chunk_size = body.len().div_ceil(fragment_count);
    body.chunks(chunk_size).map(|chunk| chunk.to_vec()).collect()
}
