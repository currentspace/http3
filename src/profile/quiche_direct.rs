#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{
    JsQuicClientOptions, JsQuicServerOptions, new_quic_client_config, new_quic_server_config,
};

const DEFAULT_ALPN: &str = "quic";
const DEFAULT_SERVER_NAME: &str = "localhost";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

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
struct DirectSummary {
    harness: &'static str,
    connections: usize,
    streams_per_connection: usize,
    payload_bytes: usize,
    requested_streams: usize,
    completed_streams: usize,
    total_response_bytes: u64,
    elapsed_ms: f64,
    throughput_mbps: f64,
    latency_ms: Option<LatencySummary>,
    error_count: usize,
    errors: Vec<String>,
}

struct DirectOptions {
    cert_path: String,
    key_path: String,
    server_name: String,
    alpn: String,
    connections: usize,
    streams_per_connection: usize,
    payload_bytes: usize,
    timeout: Duration,
}

struct DirectPair {
    client_conn: quiche::Connection,
    server_conn: Option<quiche::Connection>,
    server_config: quiche::Config,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    server_buffers: HashMap<u64, Vec<u8>>,
    client_sent_at: HashMap<u64, Instant>,
    client_response_bytes: u64,
    completed_streams: usize,
    workload_sent: bool,
}

impl DirectPair {
    fn new(index: usize, options: &DirectOptions) -> Result<Self, String> {
        let client_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            40_000u16.saturating_add((index * 2) as u16),
        );
        let server_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            50_000u16.saturating_add((index * 2) as u16),
        );
        let mut client_config = build_client_config(options)?;
        let server_config = build_server_config(options)?;
        let scid = vec![0xba; quiche::MAX_CONN_ID_LEN];
        let scid_ref = quiche::ConnectionId::from_ref(&scid);
        let client_conn = quiche::connect(
            Some(&options.server_name),
            &scid_ref,
            client_addr,
            server_addr,
            &mut client_config,
        )
        .map_err(|err| err.to_string())?;
        Ok(Self {
            client_conn,
            server_conn: None,
            server_config,
            client_addr,
            server_addr,
            server_buffers: HashMap::new(),
            client_sent_at: HashMap::new(),
            client_response_bytes: 0,
            completed_streams: 0,
            workload_sent: false,
        })
    }
}

pub fn cli_main() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1).collect())?;
    let start = Instant::now();
    let payload = vec![b'x'; options.payload_bytes];
    let mut pairs = (0..options.connections)
        .map(|index| DirectPair::new(index, &options))
        .collect::<Result<Vec<_>, _>>()?;
    let requested_streams = options.connections * options.streams_per_connection;
    let mut completed_streams = 0usize;
    let mut response_bytes = 0u64;
    let mut latencies_ms = Vec::with_capacity(requested_streams);
    let mut errors = Vec::new();

    while completed_streams < requested_streams && start.elapsed() < options.timeout {
        let mut made_progress = false;
        for pair in &mut pairs {
            made_progress |= advance_pair(pair, &options, &payload, &mut latencies_ms, &mut errors)?;
        }
        completed_streams = pairs.iter().map(|pair| pair.completed_streams).sum();
        response_bytes = pairs.iter().map(|pair| pair.client_response_bytes).sum();
        if !made_progress {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    if completed_streams < requested_streams {
        errors.push(format!(
            "timed out after {} ms with {completed_streams}/{requested_streams} streams completed",
            options.timeout.as_millis()
        ));
    }

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let throughput_mbps = if elapsed_ms > 0.0 {
        (response_bytes as f64 * 8.0) / (elapsed_ms / 1000.0) / 1_000_000.0
    } else {
        0.0
    };
    let summary = DirectSummary {
        harness: "quiche-direct-floor",
        connections: options.connections,
        streams_per_connection: options.streams_per_connection,
        payload_bytes: options.payload_bytes,
        requested_streams,
        completed_streams,
        total_response_bytes: response_bytes,
        elapsed_ms,
        throughput_mbps,
        latency_ms: summarize_latencies(&latencies_ms),
        error_count: errors.len(),
        errors,
    };
    println!(
        "RESULT {}",
        serde_json::to_string(&summary).map_err(|err| err.to_string())?
    );
    Ok(())
}

fn advance_pair(
    pair: &mut DirectPair,
    options: &DirectOptions,
    payload: &[u8],
    latencies_ms: &mut Vec<f64>,
    errors: &mut Vec<String>,
) -> Result<bool, String> {
    let mut progressed = false;
    progressed |= flush_client_packets(pair, latencies_ms)?;
    progressed |= flush_server_packets(pair, latencies_ms)?;
    progressed |= handle_timeouts(pair);

    if pair.client_conn.is_established()
        && pair.server_conn.as_ref().is_some_and(quiche::Connection::is_established)
        && !pair.workload_sent
    {
        for stream_index in 0..options.streams_per_connection {
            let stream_id = (stream_index as u64) * 4;
            let written = pair
                .client_conn
                .stream_send(stream_id, payload, true)
                .map_err(|err| err.to_string())?;
            if written != payload.len() {
                errors.push(format!(
                    "pair {} short write on stream {}: wrote {} of {} bytes",
                    pair.client_addr.port(),
                    stream_id,
                    written,
                    payload.len()
                ));
            }
            pair.client_sent_at.insert(stream_id, Instant::now());
        }
        pair.workload_sent = true;
        progressed = true;
    }

    Ok(progressed)
}

fn flush_client_packets(pair: &mut DirectPair, latencies_ms: &mut Vec<f64>) -> Result<bool, String> {
    let mut buf = vec![0u8; 65535];
    let mut progressed = false;
    loop {
        match pair.client_conn.send(&mut buf) {
            Ok((len, send_info)) => {
                progressed = true;
                if pair.server_conn.is_none() {
                    let header = quiche::Header::from_slice(&mut buf[..len], quiche::MAX_CONN_ID_LEN)
                        .map_err(|err| err.to_string())?;
                    let server_scid = vec![0xab; quiche::MAX_CONN_ID_LEN];
                    let server_scid_ref = quiche::ConnectionId::from_ref(&server_scid);
                    let server_conn = quiche::accept(
                        &server_scid_ref,
                        Some(&header.dcid),
                        pair.server_addr,
                        pair.client_addr,
                        &mut pair.server_config,
                    )
                    .map_err(|err| err.to_string())?;
                    pair.server_conn = Some(server_conn);
                }
                if let Some(server_conn) = pair.server_conn.as_mut() {
                    let recv_info = quiche::RecvInfo {
                        from: pair.client_addr,
                        to: send_info.to,
                    };
                    server_conn
                        .recv(&mut buf[..len], recv_info)
                        .map_err(|err| err.to_string())?;
                    pump_server_echo(pair)?;
                }
            }
            Err(quiche::Error::Done) => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    if progressed {
        let _ = pump_server_echo(pair)?;
        let _ = pump_client_events(pair, latencies_ms);
    }
    Ok(progressed)
}

fn flush_server_packets(pair: &mut DirectPair, latencies_ms: &mut Vec<f64>) -> Result<bool, String> {
    if pair.server_conn.is_none() {
        return Ok(false);
    }
    let mut buf = vec![0u8; 65535];
    let mut progressed = false;
    loop {
        let send_result = {
            let server_conn = pair.server_conn.as_mut().expect("server conn checked above");
            server_conn.send(&mut buf)
        };
        match send_result {
            Ok((len, send_info)) => {
                progressed = true;
                let recv_info = quiche::RecvInfo {
                    from: pair.server_addr,
                    to: send_info.to,
                };
                pair.client_conn
                    .recv(&mut buf[..len], recv_info)
                    .map_err(|err| err.to_string())?;
                progressed |= pump_client_events(pair, latencies_ms);
            }
            Err(quiche::Error::Done) => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(progressed)
}

fn pump_server_echo(pair: &mut DirectPair) -> Result<bool, String> {
    let Some(server_conn) = pair.server_conn.as_mut() else {
        return Ok(false);
    };
    let mut recv_buf = [0u8; 65535];
    let mut progressed = false;
    let readable: Vec<u64> = server_conn.readable().collect();
    for stream_id in readable {
        loop {
            match server_conn.stream_recv(stream_id, &mut recv_buf) {
                Ok((len, fin)) => {
                    progressed = true;
                    pair.server_buffers
                        .entry(stream_id)
                        .or_default()
                        .extend_from_slice(&recv_buf[..len]);
                    if fin {
                        let body = pair.server_buffers.remove(&stream_id).unwrap_or_default();
                        let written = server_conn
                            .stream_send(stream_id, &body, true)
                            .map_err(|err| err.to_string())?;
                        if written != body.len() {
                            return Err(format!(
                                "server short write on stream {stream_id}: wrote {written} of {} bytes",
                                body.len()
                            ));
                        }
                        break;
                    }
                    if len == 0 {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(err) => return Err(err.to_string()),
            }
        }
    }
    Ok(progressed)
}

fn pump_client_events(pair: &mut DirectPair, latencies_ms: &mut Vec<f64>) -> bool {
    let mut recv_buf = [0u8; 65535];
    let mut progressed = false;
    let readable: Vec<u64> = pair.client_conn.readable().collect();
    for stream_id in readable {
        loop {
            match pair.client_conn.stream_recv(stream_id, &mut recv_buf) {
                Ok((len, fin)) => {
                    progressed = true;
                    pair.client_response_bytes += len as u64;
                    if fin {
                        if let Some(sent_at) = pair.client_sent_at.remove(&stream_id) {
                            latencies_ms.push(sent_at.elapsed().as_secs_f64() * 1000.0);
                            pair.completed_streams += 1;
                        }
                        break;
                    }
                    if len == 0 {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(_) => break,
            }
        }
    }
    progressed
}

fn handle_timeouts(pair: &mut DirectPair) -> bool {
    let mut progressed = false;
    if pair.client_conn.timeout() == Some(Duration::ZERO) {
        pair.client_conn.on_timeout();
        progressed = true;
    }
    if let Some(server_conn) = pair.server_conn.as_mut() {
        if server_conn.timeout() == Some(Duration::ZERO) {
            server_conn.on_timeout();
            progressed = true;
        }
    }
    progressed
}

fn build_server_config(options: &DirectOptions) -> Result<quiche::Config, String> {
    let cert = fs::read(&options.cert_path).map_err(|err| err.to_string())?;
    let key = fs::read(&options.key_path).map_err(|err| err.to_string())?;
    let server_options = JsQuicServerOptions {
        key: key.into(),
        cert: cert.into(),
        ca: None,
        client_auth: None,
        alpn: Some(vec![options.alpn.clone()]),
        runtime_mode: None,
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

fn build_client_config(options: &DirectOptions) -> Result<quiche::Config, String> {
    let client_options = JsQuicClientOptions {
        ca: None,
        cert: None,
        key: None,
        reject_unauthorized: Some(false),
        alpn: Some(vec![options.alpn.clone()]),
        runtime_mode: None,
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

fn parse_args(args: Vec<String>) -> Result<DirectOptions, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Err(help_text());
    }
    let mut cert_path = None;
    let mut key_path = None;
    let mut server_name = DEFAULT_SERVER_NAME.to_string();
    let mut alpn = DEFAULT_ALPN.to_string();
    let mut connections = 1usize;
    let mut streams_per_connection = 100usize;
    let mut payload_bytes = 16 * 1024usize;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;

    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--cert-path" => cert_path = Some(next_value(&args, &mut index, "--cert-path")?),
            "--key-path" => key_path = Some(next_value(&args, &mut index, "--key-path")?),
            "--server-name" => server_name = next_value(&args, &mut index, "--server-name")?,
            "--alpn" => alpn = next_value(&args, &mut index, "--alpn")?,
            "--connections" => {
                connections = next_value(&args, &mut index, "--connections")?
                    .parse()
                    .map_err(|err| format!("invalid --connections value: {err}"))?;
            }
            "--streams-per-connection" => {
                streams_per_connection = next_value(&args, &mut index, "--streams-per-connection")?
                    .parse()
                    .map_err(|err| format!("invalid --streams-per-connection value: {err}"))?;
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
            flag => return Err(format!("unknown flag: {flag}\n\n{}", help_text())),
        }
        index += 1;
    }

    Ok(DirectOptions {
        cert_path: cert_path.ok_or_else(|| "--cert-path is required".to_string())?,
        key_path: key_path.ok_or_else(|| "--key-path is required".to_string())?,
        server_name,
        alpn,
        connections,
        streams_per_connection,
        payload_bytes,
        timeout: Duration::from_millis(timeout_ms),
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

fn help_text() -> String {
    r#"quic_direct_profile

Usage:
  quic_direct_profile --cert-path FILE --key-path FILE [options]

Options:
  --cert-path FILE                 PEM certificate path
  --key-path FILE                  PEM private key path
  --server-name VALUE              SNI / hostname (default: localhost)
  --alpn VALUE                     ALPN string (default: quic)
  --connections N                  Number of in-memory client/server pairs (default: 1)
  --streams-per-connection N       Streams per pair (default: 100)
  --payload-bytes N                Bytes per request stream (default: 16384)
  --timeout-ms N                   End-to-end timeout (default: 30000)

Output:
  Prints `RESULT <json>` on completion."#
        .into()
}
