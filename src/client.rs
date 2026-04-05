//! N-API bindings for the HTTP/3 client (`NativeWorkerClient`). The Rust
//! worker thread owns UDP I/O, polling, timers, and QUIC/H3 processing.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{Http3Config, JsClientOptions, TransportRuntimeMode};
use crate::h3_event::{JsAddressInfo, JsHeader, JsSessionMetrics, JsSetting};

use std::net::SocketAddr;
use std::time::Duration;

/// Worker-thread client: Rust owns UDP I/O, polling, timers, and QUIC/H3 processing.
#[napi]
pub struct NativeWorkerClient {
    handle: Option<crate::worker::ClientWorkerHandle>,
    /// Stored until `connect()` is called, then consumed.
    quiche_config: Option<quiche::Config>,
    tsfn: Option<crate::worker::EventTsfn>,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    keepalive_interval: Option<Duration>,
}

#[napi]
impl NativeWorkerClient {
    #[napi(constructor)]
    pub fn new(
        options: JsClientOptions,
        #[napi(ts_arg_type = "(err: Error | null, events: Array<JsH3Event>) => void")]
        callback: crate::worker::EventTsfn,
    ) -> napi::Result<Self> {
        let quiche_config =
            Http3Config::new_client_quiche_config(&options).map_err(napi::Error::from)?;

        Ok(Self {
            handle: None,
            quiche_config: Some(quiche_config),
            tsfn: Some(callback),
            session_ticket: options.session_ticket.map(|ticket| ticket.to_vec()),
            qlog_dir: options.qlog_dir,
            qlog_level: options.qlog_level,
            runtime_mode: TransportRuntimeMode::parse(options.runtime_mode.as_deref())
                .map_err(napi::Error::from)?,
            keepalive_interval: options
                .keepalive_interval_ms
                .map(|ms| Duration::from_millis(u64::from(ms))),
        })
    }

    #[napi]
    pub fn connect(
        &mut self,
        server_addr: String,
        server_name: String,
    ) -> napi::Result<JsAddressInfo> {
        let addr: SocketAddr = server_addr
            .parse()
            .map_err(|e: std::net::AddrParseError| napi::Error::from_reason(e.to_string()))?;

        let quiche_config = self
            .quiche_config
            .take()
            .ok_or_else(|| napi::Error::from_reason("already connected"))?;
        let tsfn = self
            .tsfn
            .take()
            .ok_or_else(|| napi::Error::from_reason("already connected"))?;

        let handle = crate::worker::spawn_client_worker(
            quiche_config,
            addr,
            server_name,
            self.session_ticket.take(),
            self.qlog_dir.clone(),
            self.qlog_level.clone(),
            self.runtime_mode,
            self.keepalive_interval,
            tsfn,
        )
        .map_err(napi::Error::from)?;
        let local = handle.local_addr();
        self.handle = Some(handle);

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

    #[napi]
    pub fn send_request(&self, headers: Vec<JsHeader>, fin: bool) -> napi::Result<i64> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
        let h: Vec<(String, String)> = headers.into_iter().map(|h| (h.name, h.value)).collect();
        let stream_id = handle.send_request(h, fin).map_err(napi::Error::from)?;
        Ok(stream_id as i64)
    }

    #[napi]
    pub fn stream_send(&self, stream_id: i64, data: Buffer, fin: bool) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.stream_send(stream_id as u64, crate::chunk_pool::Chunk::unpooled(data.to_vec()), fin)
    }

    #[napi]
    pub fn stream_close(&self, stream_id: i64, error_code: u32) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.stream_close(stream_id as u64, error_code)
    }

    #[napi]
    pub fn close(&self, error_code: u32, reason: String) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.close(error_code, reason)
    }

    #[napi]
    pub fn send_datagram(&self, data: Buffer) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_datagram(data.to_vec()).unwrap_or(false)
    }

    #[napi]
    pub fn local_address(&self) -> napi::Result<JsAddressInfo> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
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
    pub fn get_session_metrics(&self) -> napi::Result<JsSessionMetrics> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
        let metrics = handle
            .get_session_metrics()
            .map_err(napi::Error::from)?
            .ok_or_else(|| napi::Error::from_reason("client metrics unavailable"))?;
        Ok(metrics)
    }

    #[napi]
    pub fn get_remote_settings(&self) -> napi::Result<Vec<JsSetting>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
        let settings = handle.get_remote_settings().map_err(napi::Error::from)?;
        Ok(settings
            .into_iter()
            .map(|(id, value)| JsSetting {
                id: id as i64,
                value: value as i64,
            })
            .collect())
    }

    #[napi]
    pub fn ping(&self) -> napi::Result<bool> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
        handle.ping().map_err(napi::Error::from)
    }

    #[napi]
    pub fn get_qlog_path(&self) -> napi::Result<Option<String>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("client worker not running"))?;
        handle.get_qlog_path().map_err(napi::Error::from)
    }

    /// Send the Shutdown command without joining the worker thread.
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

    /// Join the worker thread. Safe to call after `request_shutdown()`.
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
