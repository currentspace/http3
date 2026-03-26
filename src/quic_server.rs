//! N-API bindings for the raw QUIC server (`NativeQuicServer`), providing
//! bidirectional streams without HTTP/3 framing.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::config::{ClientAuthMode, JsQuicServerOptions, TransportRuntimeMode};
use crate::h3_event::{JsAddressInfo, JsSessionMetrics};

use std::net::SocketAddr;

#[napi]
pub struct NativeQuicServer {
    handle: Option<crate::quic_worker::QuicServerHandle>,
    quiche_config: Option<quiche::Config>,
    server_config: Option<crate::quic_worker::QuicServerConfig>,
    tsfn: Option<crate::worker::EventTsfn>,
}

#[napi]
impl NativeQuicServer {
    #[napi(constructor)]
    pub fn new(
        options: JsQuicServerOptions,
        #[napi(ts_arg_type = "(err: Error | null, events: Array<JsH3Event>) => void")]
        callback: crate::worker::EventTsfn,
    ) -> napi::Result<Self> {
        let quiche_config =
            crate::config::new_quic_server_config(&options).map_err(napi::Error::from)?;
        let server_config = crate::quic_worker::QuicServerConfig {
            qlog_dir: options.qlog_dir,
            qlog_level: options.qlog_level,
            max_connections: options.max_connections.unwrap_or(10_000) as usize,
            disable_retry: options.disable_retry.unwrap_or(true),
            client_auth: ClientAuthMode::parse(
                options.client_auth.as_deref(),
                options.ca.is_some(),
            )
            .map_err(napi::Error::from)?,
            cid_encoding: crate::cid::CidEncoding::random(),
            runtime_mode: TransportRuntimeMode::parse(options.runtime_mode.as_deref())
                .map_err(napi::Error::from)?,
        };

        Ok(Self {
            handle: None,
            quiche_config: Some(quiche_config),
            server_config: Some(server_config),
            tsfn: Some(callback),
        })
    }

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
        let server_config = self
            .server_config
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;
        let tsfn = self
            .tsfn
            .take()
            .ok_or_else(|| napi::Error::from_reason("already listening"))?;

        let worker_handle =
            crate::quic_worker::spawn_quic_server(quiche_config, server_config, addr, tsfn)
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

    #[napi]
    pub fn stream_send(&self, conn_handle: u32, stream_id: i64, data: Buffer, fin: bool) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::quic_worker::QuicServerCommand::StreamSend {
            conn_handle,
            stream_id: stream_id as u64,
            chunk: crate::chunk_pool::Chunk::unpooled(data.to_vec()),
            fin,
        })
    }

    #[napi]
    pub fn stream_close(&self, conn_handle: u32, stream_id: i64, error_code: u32) -> bool {
        let Some(handle) = &self.handle else {
            return false;
        };
        handle.send_command(crate::quic_worker::QuicServerCommand::StreamClose {
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
        handle.send_command(crate::quic_worker::QuicServerCommand::CloseSession {
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
            .ok_or_else(|| napi::Error::from_reason("metrics unavailable"))?;
        Ok(metrics)
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
