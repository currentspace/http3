use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{JsQuicClientOptions, JsQuicServerOptions};
use crate::profile::mock_quic::{
    MockClientSinkMode, MockQuicProfileConfig, MockQuicProfileRunner, spawn_mock_quic_profile,
};
use crate::profile::mock_trace::MockReplayTrace;

const DEFAULT_STREAMS_PER_CONNECTION: u32 = 256;
const DEFAULT_PAYLOAD_BYTES: u32 = 1024;
const DEFAULT_TIMEOUT_MS: u32 = 30_000;

#[napi(object)]
pub struct JsMockQuicProfileOptions {
    pub key: Buffer,
    pub cert: Buffer,
    pub ca: Option<Buffer>,
    pub alpn: Option<Vec<String>>,
    pub reject_unauthorized: Option<bool>,
    pub server_name: Option<String>,
    pub streams_per_connection: Option<u32>,
    pub payload_bytes: Option<u32>,
    pub response_fragment_count: Option<u32>,
    pub response_fragment_size: Option<u32>,
    pub timeout_ms: Option<u32>,
    pub sink_mode: Option<String>,
    pub replay_trace_json: Option<String>,
}

#[napi]
pub struct NativeMockQuicProfiler {
    runner: Option<MockQuicProfileRunner>,
}

#[napi]
impl NativeMockQuicProfiler {
    #[napi(constructor)]
    pub fn new(
        options: JsMockQuicProfileOptions,
        #[napi(ts_arg_type = "(err: Error | null, events: Array<JsH3Event>) => void")]
        callback: crate::worker::EventTsfn,
    ) -> napi::Result<Self> {
        let JsMockQuicProfileOptions {
            key,
            cert,
            ca,
            alpn,
            reject_unauthorized,
            server_name,
            streams_per_connection,
            payload_bytes,
            response_fragment_count,
            response_fragment_size,
            timeout_ms,
            sink_mode,
            replay_trace_json,
        } = options;
        let sink_mode = parse_sink_mode(sink_mode.as_deref())?;
        let replay_trace = replay_trace_json
            .as_deref()
            .map(serde_json::from_str::<MockReplayTrace>)
            .transpose()
            .map_err(|err| napi::Error::from_reason(err.to_string()))?;
        let configured_streams_per_connection =
            streams_per_connection.unwrap_or(DEFAULT_STREAMS_PER_CONNECTION);
        let configured_payload_bytes = payload_bytes.unwrap_or(DEFAULT_PAYLOAD_BYTES);
        let effective_streams_per_connection = replay_trace
            .as_ref()
            .map_or(configured_streams_per_connection, |trace| {
                trace.requested_streams.try_into().unwrap_or(u32::MAX)
            });
        let effective_payload_bytes = replay_trace
            .as_ref()
            .map_or(configured_payload_bytes, |trace| {
                trace.payload_bytes.try_into().unwrap_or(u32::MAX)
            });
        let effective_server_name = server_name.unwrap_or_else(|| {
            replay_trace
                .as_ref()
                .map_or_else(|| "localhost".into(), |trace| trace.server_name.clone())
        });
        let server_ca = ca.as_ref().map(|value| value.to_vec().into());
        let server_options = JsQuicServerOptions {
            key,
            cert,
            ca: server_ca,
            alpn: alpn.clone(),
            runtime_mode: Some("portable".into()),
            max_idle_timeout_ms: timeout_ms.or(Some(DEFAULT_TIMEOUT_MS)),
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: Some(effective_streams_per_connection.saturating_add(32)),
            disable_active_migration: Some(true),
            enable_datagrams: Some(false),
            max_connections: Some(32),
            disable_retry: Some(true),
            qlog_dir: None,
            qlog_level: None,
            session_ticket_keys: None,
            keylog: Some(false),
        };
        let client_options = JsQuicClientOptions {
            ca,
            reject_unauthorized: reject_unauthorized.or(Some(false)),
            alpn,
            runtime_mode: Some("portable".into()),
            max_idle_timeout_ms: timeout_ms.or(Some(DEFAULT_TIMEOUT_MS)),
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: Some(effective_streams_per_connection.saturating_add(32)),
            session_ticket: None,
            allow_0rtt: Some(false),
            enable_datagrams: Some(false),
            keylog: Some(false),
            qlog_dir: None,
            qlog_level: None,
        };
        let runner = spawn_mock_quic_profile(
            MockQuicProfileConfig {
                server_options,
                client_options,
                server_name: effective_server_name,
                configured_streams_per_connection: configured_streams_per_connection as usize,
                configured_payload_bytes: configured_payload_bytes as usize,
                streams_per_connection: effective_streams_per_connection as usize,
                payload_bytes: effective_payload_bytes as usize,
                response_fragment_count: response_fragment_count.unwrap_or(1) as usize,
                response_fragment_size: response_fragment_size.map(|value| value as usize),
                timeout: Duration::from_millis(
                    u64::from(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
                ),
                client_sink_mode: sink_mode,
                replay_trace,
            },
            Some(callback),
        );
        Ok(Self {
            runner: Some(runner),
        })
    }

    #[napi]
    pub fn poll_result_json(&mut self) -> napi::Result<Option<String>> {
        let Some(runner) = self.runner.as_mut() else {
            return Ok(None);
        };
        match runner.poll_result_json() {
            Some(Ok(json)) => Ok(Some(json)),
            Some(Err(err)) => Err(napi::Error::from_reason(err)),
            None => Ok(None),
        }
    }

    #[napi]
    pub fn shutdown(&mut self) -> napi::Result<()> {
        if let Some(mut runner) = self.runner.take() {
            runner.shutdown();
        }
        Ok(())
    }
}

impl Drop for NativeMockQuicProfiler {
    fn drop(&mut self) {
        if let Some(mut runner) = self.runner.take() {
            runner.shutdown();
        }
    }
}

fn parse_sink_mode(value: Option<&str>) -> napi::Result<MockClientSinkMode> {
    match value.unwrap_or("counting") {
        "counting" => Ok(MockClientSinkMode::Counting),
        "tsfn" => Ok(MockClientSinkMode::Tsfn),
        other => Err(napi::Error::from_reason(format!(
            "unsupported mock QUIC sink mode `{other}`"
        ))),
    }
}
