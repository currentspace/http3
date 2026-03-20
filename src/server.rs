//! N-API bindings for the HTTP/3 server (`NativeWorkerServer`). The Rust
//! worker thread owns UDP I/O, polling, and quiche processing; events are
//! delivered to JS via a `ThreadsafeFunction`.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{Http3Config, JsServerOptions};
use crate::h3_event::{JsAddressInfo, JsHeader, JsSessionMetrics, JsSetting};

use std::net::SocketAddr;

// ----- Worker Thread Server -----

#[napi]
pub struct NativeWorkerServer {
    handle: Option<crate::worker::WorkerHandle>,
    /// Stored until `listen()` is called, then consumed.
    quiche_config: Option<quiche::Config>,
    http3_config: Option<Http3Config>,
    tsfn: Option<crate::worker::EventTsfn>,
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

        Ok(Self {
            handle: None,
            quiche_config: Some(quiche_config),
            http3_config: Some(http3_config),
            tsfn: Some(tsfn),
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

        let worker_handle =
            crate::worker::spawn_worker(quiche_config, http3_config, addr, tsfn)
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

    #[napi]
    pub fn stream_send(&self, conn_handle: u32, stream_id: i64, data: Buffer, fin: bool) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::worker::WorkerCommand::StreamSend {
            conn_handle,
            stream_id: stream_id as u64,
            data: data.to_vec(),
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

    #[napi]
    pub fn shutdown(&mut self) -> napi::Result<()> {
        if let Some(mut h) = self.handle.take() {
            h.shutdown();
        }
        Ok(())
    }
}
