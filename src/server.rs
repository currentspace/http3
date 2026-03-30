//! N-API bindings for the HTTP/3 server (`NativeWorkerServer`). The Rust
//! worker thread owns UDP I/O, polling, and quiche processing; events are
//! delivered to JS via a `ThreadsafeFunction`.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{Http3Config, JsServerOptions};
use crate::h3_event::{JsAddressInfo, JsHeader, JsSessionMetrics, JsSetting};

use std::net::SocketAddr;

/// Snapshot of server options with owned byte vectors (no napi Buffers)
/// so they can be used to rebuild quiche configs for additional workers.
pub(crate) struct StoredServerOptions {
    key: Vec<u8>,
    cert: Vec<u8>,
    ca: Option<Vec<u8>>,
    max_idle_timeout_ms: Option<u32>,
    max_udp_payload_size: Option<u32>,
    initial_max_data: Option<u32>,
    initial_max_stream_data_bidi_local: Option<u32>,
    initial_max_streams_bidi: Option<u32>,
    disable_active_migration: Option<bool>,
    enable_datagrams: Option<bool>,
    session_ticket_keys: Option<Vec<u8>>,
    keylog: Option<bool>,
}

impl StoredServerOptions {
    fn from_js(options: &JsServerOptions) -> Self {
        Self {
            key: options.key.to_vec(),
            cert: options.cert.to_vec(),
            ca: options.ca.as_ref().map(|ca| ca.to_vec()),
            max_idle_timeout_ms: options.max_idle_timeout_ms,
            max_udp_payload_size: options.max_udp_payload_size,
            initial_max_data: options.initial_max_data,
            initial_max_stream_data_bidi_local: options.initial_max_stream_data_bidi_local,
            initial_max_streams_bidi: options.initial_max_streams_bidi,
            disable_active_migration: options.disable_active_migration,
            enable_datagrams: options.enable_datagrams,
            session_ticket_keys: options.session_ticket_keys.as_ref().map(|t| t.to_vec()),
            keylog: options.keylog,
        }
    }

    pub(crate) fn to_js_server_options(&self) -> JsServerOptions {
        JsServerOptions {
            key: self.key.clone().into(),
            cert: self.cert.clone().into(),
            ca: self.ca.as_ref().map(|ca| ca.clone().into()),
            runtime_mode: None,
            max_idle_timeout_ms: self.max_idle_timeout_ms,
            max_udp_payload_size: self.max_udp_payload_size,
            initial_max_data: self.initial_max_data,
            initial_max_stream_data_bidi_local: self.initial_max_stream_data_bidi_local,
            initial_max_streams_bidi: self.initial_max_streams_bidi,
            disable_active_migration: self.disable_active_migration,
            enable_datagrams: self.enable_datagrams,
            qpack_max_table_capacity: None,
            qpack_blocked_streams: None,
            recv_batch_size: None,
            send_batch_size: None,
            qlog_dir: None,
            qlog_level: None,
            session_ticket_keys: self.session_ticket_keys.as_ref().map(|t| t.clone().into()),
            max_connections: None,
            disable_retry: None,
            reuse_port: None,
            keylog: self.keylog,
            quic_lb: None,
            server_id: None,
        }
    }
}

// ----- Worker Thread Server -----

#[napi]
pub struct NativeWorkerServer {
    handle: Option<crate::worker::WorkerHandle>,
    /// Stored until `listen()` is called, then consumed.
    quiche_config: Option<quiche::Config>,
    http3_config: Option<Http3Config>,
    tsfn: Option<crate::worker::EventTsfn>,
    /// Retained for creating additional quiche configs for sharded workers.
    /// All napi Buffers are copied to Vec<u8> so they can be used across threads.
    server_options_snapshot: Option<StoredServerOptions>,
}

#[napi]
impl NativeWorkerServer {
    #[napi(constructor)]
    pub fn new(
        options: JsServerOptions,
        #[napi(ts_arg_type = "(err: Error | null, events: Array<JsH3Event>) => void")]
        callback: crate::worker::EventTsfn,
    ) -> napi::Result<Self> {
        let quiche_config =
            Http3Config::new_server_quiche_config(&options).map_err(napi::Error::from)?;
        let http3_config = Http3Config::from_server_options(&options).map_err(napi::Error::from)?;
        let tsfn = callback;

        let stored = StoredServerOptions::from_js(&options);
        Ok(Self {
            handle: None,
            quiche_config: Some(quiche_config),
            http3_config: Some(http3_config),
            tsfn: Some(tsfn),
            server_options_snapshot: Some(stored),
        })
    }

    /// Start the worker thread, binding to the given address.
    /// Returns the bound address info.
    #[napi]
    pub fn listen(&mut self, port: u32, host: String) -> napi::Result<JsAddressInfo> {
        let host_for_parse = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let addr: SocketAddr = format!("{host_for_parse}:{port}")
            .parse()
            .map_err(|e: std::net::AddrParseError| napi::Error::from_reason(e.to_string()))?;

        let quiche_config = self
            .quiche_config
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;
        let http3_config = self
            .http3_config
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;
        let tsfn = self
            .tsfn
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;

        let stored = self
            .server_options_snapshot
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;
        let worker_handle =
            crate::worker::spawn_worker(quiche_config, http3_config, addr, tsfn, stored)
                .map_err(napi::Error::from)?;

        let local = worker_handle.local_addr();
        self.handle = Some(worker_handle);

        Ok(JsAddressInfo {
            address: local.ip().to_string(),
            family: if local.is_ipv4() {
                "IPv4".into()
            } else {
                "IPv6".into()
            },
            port: u32::from(local.port()),
        })
    }

    /// Send a command to the worker. Returns false if backpressure (queue full).
    #[napi]
    pub fn send_response_headers(
        &self,
        conn_handle: u32,
        stream_id: i64,
        headers: Vec<JsHeader>,
        fin: bool,
    ) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        let h: Vec<(String, String)> = headers.into_iter().map(|h| (h.name, h.value)).collect();
        handle.send_command(crate::worker::WorkerCommand::SendResponseHeaders {
            conn_handle,
            stream_id: stream_id as u64,
            headers: h,
            fin,
        })
    }

    /// Combined headers + body + FIN in a single NAPI call — avoids 2 extra
    /// FFI boundary crossings for the common respond-then-end pattern.
    #[napi]
    pub fn send_response(
        &self,
        conn_handle: u32,
        stream_id: i64,
        headers: Vec<JsHeader>,
        data: Buffer,
        fin: bool,
    ) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        let h: Vec<(String, String)> = headers.into_iter().map(|h| (h.name, h.value)).collect();
        handle.send_command(crate::worker::WorkerCommand::SendResponse {
            conn_handle,
            stream_id: stream_id as u64,
            headers: h,
            body: crate::chunk_pool::Chunk::unpooled(data.to_vec()),
            fin,
        })
    }

    #[napi]
    pub fn stream_send(&self, conn_handle: u32, stream_id: i64, data: Buffer, fin: bool) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::worker::WorkerCommand::StreamSend {
            conn_handle,
            stream_id: stream_id as u64,
            chunk: crate::chunk_pool::Chunk::unpooled(data.to_vec()),
            fin,
        })
    }

    #[napi]
    pub fn send_trailers(&self, conn_handle: u32, stream_id: i64, headers: Vec<JsHeader>) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        let h: Vec<(String, String)> = headers.into_iter().map(|h| (h.name, h.value)).collect();
        handle.send_command(crate::worker::WorkerCommand::SendTrailers {
            conn_handle,
            stream_id: stream_id as u64,
            headers: h,
        })
    }

    #[napi]
    pub fn stream_close(&self, conn_handle: u32, stream_id: i64, error_code: u32) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::worker::WorkerCommand::StreamClose {
            conn_handle,
            stream_id: stream_id as u64,
            error_code,
        })
    }

    #[napi]
    pub fn close_session(&self, conn_handle: u32, error_code: u32, reason: String) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::worker::WorkerCommand::CloseSession {
            conn_handle,
            error_code,
            reason,
        })
    }

    #[napi]
    pub fn send_datagram(&self, conn_handle: u32, data: Buffer) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle
            .send_datagram(conn_handle, data.to_vec())
            .unwrap_or(false)
    }

    #[napi]
    pub fn local_address(&self) -> napi::Result<JsAddressInfo> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("worker not running"))?;
        let addr = handle.local_addr();
        Ok(JsAddressInfo {
            address: addr.ip().to_string(),
            family: if addr.is_ipv4() {
                "IPv4".into()
            } else {
                "IPv6".into()
            },
            port: u32::from(addr.port()),
        })
    }

    #[napi]
    pub fn get_session_metrics(&self, conn_handle: u32) -> napi::Result<JsSessionMetrics> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("worker not running"))?;
        let metrics = handle
            .get_session_metrics(conn_handle)
            .map_err(napi::Error::from)?
            .ok_or_else(|| napi::Error::from_reason("session metrics unavailable"))?;
        Ok(metrics)
    }

    #[napi]
    pub fn get_remote_settings(&self, conn_handle: u32) -> napi::Result<Vec<JsSetting>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("worker not running"))?;
        let settings = handle
            .get_remote_settings(conn_handle)
            .map_err(napi::Error::from)?;
        Ok(settings
            .into_iter()
            .map(|(id, value)| JsSetting {
                id: id as i64,
                value: value as i64,
            })
            .collect())
    }

    #[napi]
    pub fn ping_session(&self, conn_handle: u32) -> napi::Result<bool> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("worker not running"))?;
        handle.ping_session(conn_handle).map_err(napi::Error::from)
    }

    #[napi]
    pub fn get_qlog_path(&self, conn_handle: u32) -> napi::Result<Option<String>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("worker not running"))?;
        handle.get_qlog_path(conn_handle).map_err(napi::Error::from)
    }

    /// Send the Shutdown command without joining the worker threads.
    #[napi]
    pub fn request_shutdown(&self) -> bool {
        match &self.handle {
            Some(h) => {
                h.request_shutdown();
                true
            }
            None => false,
        }
    }

    /// Join all worker threads. Safe to call after `request_shutdown()`.
    #[napi]
    pub fn join_worker(&mut self) {
        if let Some(mut h) = self.handle.take() {
            h.join();
        }
    }

    #[napi]
    pub fn shutdown(&mut self) -> napi::Result<()> {
        if let Some(mut h) = self.handle.take() {
            h.shutdown();
        }
        Ok(())
    }
}
