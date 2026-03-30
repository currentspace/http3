//! N-API bindings for the raw QUIC client (`NativeQuicClient`), providing
//! bidirectional streams without HTTP/3 framing.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{JsQuicClientOptions, TransportRuntimeMode};
use crate::h3_event::{JsAddressInfo, JsSessionMetrics};

use std::net::SocketAddr;
use std::sync::Arc;

#[napi]
pub struct NativeQuicClient {
    handle: Option<crate::quic_worker::QuicClientHandle>,
    quiche_config: Option<quiche::Config>,
    /// Kept alive so the TSFN reference count stays > 0 while the worker
    /// thread is shutting down, ensuring pending callbacks are delivered.
    tsfn: Option<Arc<crate::worker::EventTsfn>>,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
}

#[napi]
impl NativeQuicClient {
    #[napi(constructor)]
    pub fn new(
        options: JsQuicClientOptions,
        #[napi(ts_arg_type = "(err: Error | null, events: Array<JsH3Event>) => void")]
        callback: crate::worker::EventTsfn,
    ) -> napi::Result<Self> {
        let quiche_config =
            crate::config::new_quic_client_config(&options).map_err(napi::Error::from)?;

        Ok(Self {
            handle: None,
            quiche_config: Some(quiche_config),
            tsfn: Some(Arc::new(callback)),
            session_ticket: options.session_ticket.map(|t| t.to_vec()),
            qlog_dir: options.qlog_dir,
            qlog_level: options.qlog_level,
            runtime_mode: TransportRuntimeMode::parse(options.runtime_mode.as_deref())
                .map_err(napi::Error::from)?,
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
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("already connected"))?;

        let handle = crate::quic_worker::spawn_quic_client(
            quiche_config,
            addr,
            server_name,
            self.session_ticket.take(),
            self.qlog_dir.clone(),
            self.qlog_level.clone(),
            self.runtime_mode,
            Arc::clone(tsfn),
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
    pub fn stream_send(&self, stream_id: i64, data: Buffer, fin: bool) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.stream_send(stream_id as u64, data.to_vec(), fin)
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
            .ok_or_else(|| napi::Error::from_reason("quic client not running"))?;
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
            .ok_or_else(|| napi::Error::from_reason("quic client not running"))?;
        let metrics = handle
            .get_session_metrics()
            .map_err(napi::Error::from)?
            .ok_or_else(|| napi::Error::from_reason("metrics unavailable"))?;
        Ok(metrics)
    }

    #[napi]
    pub fn ping(&self) -> napi::Result<bool> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("quic client not running"))?;
        handle.ping().map_err(napi::Error::from)
    }

    #[napi]
    pub fn get_qlog_path(&self) -> napi::Result<Option<String>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("quic client not running"))?;
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
    /// Also releases the TSFN reference held by this struct.
    #[napi]
    pub fn join_worker(&mut self) {
        if let Some(mut h) = self.handle.take() {
            h.join();
        }
        self.tsfn = None;
    }

    #[napi]
    pub fn shutdown(&mut self) -> napi::Result<()> {
        if let Some(mut h) = self.handle.take() {
            h.shutdown();
        }
        self.tsfn = None;
        Ok(())
    }
}
