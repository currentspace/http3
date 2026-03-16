//! Worker thread loops for raw QUIC (no HTTP/3 framing).
//! Shares the event delivery mechanism (TSFN) with the H3 worker
//! but uses direct `stream_send` / `stream_recv` instead of H3 framing.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Sender;
use ring::hmac;
use ring::rand::SecureRandom;
use slab::Slab;

use crate::buffer_pool::BufferPool;
use crate::cid::CidEncoding;
use crate::error::Http3NativeError;
use crate::event_loop::{self, EventTsfn, ProtocolHandler, SEND_BUF_SIZE};
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::quic_connection::{QuicConnection, QuicConnectionInit};
use crate::timer_heap::TimerHeap;
use crate::transport::{self, Driver, ErasedWaker, TxDatagram};

const SCID_LEN: usize = crate::cid::SCID_LEN;
const TOKEN_LIFETIME_SECS: u64 = 60;

// ── Server command/handle ──────────────────────────────────────────

pub enum QuicServerCommand {
    StreamSend {
        conn_handle: u32,
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
    },
    StreamClose {
        conn_handle: u32,
        stream_id: u64,
        error_code: u32,
    },
    CloseSession {
        conn_handle: u32,
        error_code: u32,
        reason: String,
    },
    SendDatagram {
        conn_handle: u32,
        data: Vec<u8>,
        resp_tx: Sender<bool>,
    },
    GetSessionMetrics {
        conn_handle: u32,
        resp_tx: Sender<Option<JsSessionMetrics>>,
    },
    PingSession {
        conn_handle: u32,
        resp_tx: Sender<bool>,
    },
    GetQlogPath {
        conn_handle: u32,
        resp_tx: Sender<Option<String>>,
    },
    Shutdown,
}

pub struct QuicServerHandle {
    cmd_tx: Sender<QuicServerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
    local_addr: SocketAddr,
    waker: Arc<dyn ErasedWaker>,
}

impl QuicServerHandle {
    pub fn send_command(&self, cmd: QuicServerCommand) -> bool {
        if self.cmd_tx.send(cmd).is_ok() {
            let _ = self.waker.wake();
            true
        } else {
            false
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn get_session_metrics(
        &self,
        conn_handle: u32,
    ) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicServerCommand::GetSessionMetrics {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for metrics".into()))
    }

    pub fn send_datagram(
        &self,
        conn_handle: u32,
        data: Vec<u8>,
    ) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicServerCommand::SendDatagram {
                conn_handle,
                data,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for datagram".into()))
    }

    pub fn ping_session(&self, conn_handle: u32) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicServerCommand::PingSession {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for ping".into()))
    }

    pub fn get_qlog_path(&self, conn_handle: u32) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicServerCommand::GetQlogPath {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for qlog path".into()))
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(QuicServerCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for QuicServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Client command/handle ──────────────────────────────────────────

pub enum QuicClientCommand {
    StreamSend {
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
    },
    StreamClose {
        stream_id: u64,
        error_code: u32,
    },
    SendDatagram {
        data: Vec<u8>,
        resp_tx: Sender<bool>,
    },
    GetSessionMetrics {
        resp_tx: Sender<Option<JsSessionMetrics>>,
    },
    Ping {
        resp_tx: Sender<bool>,
    },
    GetQlogPath {
        resp_tx: Sender<Option<String>>,
    },
    Close {
        error_code: u32,
        reason: String,
    },
    Shutdown,
}

pub struct QuicClientHandle {
    cmd_tx: Sender<QuicClientCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
    local_addr: SocketAddr,
    waker: Arc<dyn ErasedWaker>,
}

impl QuicClientHandle {
    pub fn stream_send(&self, stream_id: u64, data: Vec<u8>, fin: bool) -> bool {
        if self
            .cmd_tx
            .send(QuicClientCommand::StreamSend {
                stream_id,
                data,
                fin,
            })
            .is_ok()
        {
            let _ = self.waker.wake();
            true
        } else {
            false
        }
    }

    pub fn stream_close(&self, stream_id: u64, error_code: u32) -> bool {
        if self
            .cmd_tx
            .send(QuicClientCommand::StreamClose {
                stream_id,
                error_code,
            })
            .is_ok()
        {
            let _ = self.waker.wake();
            true
        } else {
            false
        }
    }

    pub fn send_datagram(&self, data: Vec<u8>) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicClientCommand::SendDatagram { data, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("quic client not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for datagram".into()))
    }

    pub fn get_session_metrics(&self) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicClientCommand::GetSessionMetrics { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("quic client not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for metrics".into()))
    }

    pub fn ping(&self) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicClientCommand::Ping { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("quic client not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for ping".into()))
    }

    pub fn get_qlog_path(&self) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(QuicClientCommand::GetQlogPath { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("quic client not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for qlog path".into()))
    }

    pub fn close(&self, error_code: u32, reason: String) -> bool {
        if self
            .cmd_tx
            .send(QuicClientCommand::Close { error_code, reason })
            .is_ok()
        {
            let _ = self.waker.wake();
            true
        } else {
            false
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(QuicClientCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for QuicClientHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Minimal connection map for QUIC ────────────────────────────────

struct QuicConnectionMap {
    by_dcid: HashMap<Vec<u8>, usize>,
    connections: Slab<QuicConnection>,
    token_key: hmac::Key,
    max_connections: usize,
    cid_encoding: CidEncoding,
}

impl QuicConnectionMap {
    fn new(max_connections: usize, cid_encoding: CidEncoding) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let mut key_bytes = [0u8; 32];
        #[allow(clippy::expect_used)]
        rng.fill(&mut key_bytes)
            .expect("system RNG should not fail");
        Self {
            by_dcid: HashMap::new(),
            connections: Slab::new(),
            token_key: hmac::Key::new(hmac::HMAC_SHA256, &key_bytes),
            max_connections,
            cid_encoding,
        }
    }

    fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        self.cid_encoding.generate_scid()
    }

    fn route_packet(&self, dcid: &[u8]) -> Option<usize> {
        self.by_dcid.get(dcid).copied()
    }

    fn add_dcid(&mut self, handle: usize, dcid: Vec<u8>) {
        if self.connections.contains(handle) {
            self.by_dcid.insert(dcid, handle);
        }
    }

    fn remove_dcid(&mut self, dcid: &[u8]) {
        self.by_dcid.remove(dcid);
    }

    fn mint_token(&self, peer: &SocketAddr, odcid: &[u8]) -> Vec<u8> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut payload = Vec::new();
        match peer {
            SocketAddr::V4(v4) => {
                payload.push(4);
                payload.extend_from_slice(&v4.ip().octets());
                payload.extend_from_slice(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                payload.push(6);
                payload.extend_from_slice(&v6.ip().octets());
                payload.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        payload.extend_from_slice(&now.to_be_bytes());
        payload.push(odcid.len() as u8);
        payload.extend_from_slice(odcid);
        let tag = hmac::sign(&self.token_key, &payload);
        let mut token = tag.as_ref().to_vec();
        token.extend_from_slice(&payload);
        token
    }

    fn validate_token(&self, token: &[u8], peer: &SocketAddr) -> Option<Vec<u8>> {
        if token.len() < 32 {
            return None;
        }
        let (tag_bytes, payload) = token.split_at(32);
        if hmac::verify(&self.token_key, payload, tag_bytes).is_err() {
            return None;
        }
        let mut pos = 0;
        if pos >= payload.len() {
            return None;
        }
        let family = payload[pos];
        pos += 1;
        match (family, peer) {
            (4, SocketAddr::V4(v4)) => {
                if payload.len() < pos + 6 {
                    return None;
                }
                if payload[pos..pos + 4] != v4.ip().octets() {
                    return None;
                }
                pos += 4;
                if payload[pos..pos + 2] != v4.port().to_be_bytes() {
                    return None;
                }
                pos += 2;
            }
            (6, SocketAddr::V6(v6)) => {
                if payload.len() < pos + 18 {
                    return None;
                }
                if payload[pos..pos + 16] != v6.ip().octets() {
                    return None;
                }
                pos += 16;
                if payload[pos..pos + 2] != v6.port().to_be_bytes() {
                    return None;
                }
                pos += 2;
            }
            _ => return None,
        }
        if payload.len() < pos + 8 {
            return None;
        }
        let timestamp = u64::from_be_bytes(payload[pos..pos + 8].try_into().ok()?);
        pos += 8;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now.saturating_sub(timestamp) > TOKEN_LIFETIME_SECS {
            return None;
        }
        if pos >= payload.len() {
            return None;
        }
        let odcid_len = payload[pos] as usize;
        pos += 1;
        if payload.len() < pos + odcid_len {
            return None;
        }
        Some(payload[pos..pos + odcid_len].to_vec())
    }

    fn accept_new(
        &mut self,
        scid: &[u8],
        odcid: Option<&quiche::ConnectionId<'_>>,
        peer: SocketAddr,
        local: SocketAddr,
        config: &mut quiche::Config,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
    ) -> Result<usize, Http3NativeError> {
        if self.connections.len() >= self.max_connections {
            return Err(Http3NativeError::Config(format!(
                "max connections ({}) reached",
                self.max_connections,
            )));
        }
        let scid_owned = scid.to_vec();
        let scid_ref = quiche::ConnectionId::from_ref(scid);
        let quiche_conn = quiche::accept(&scid_ref, odcid, local, peer, config)
            .map_err(Http3NativeError::Quiche)?;
        let conn = QuicConnection::new(
            quiche_conn,
            scid_owned.clone(),
            QuicConnectionInit {
                role: "server",
                qlog_dir,
                qlog_level,
            },
        );
        let handle = self.connections.insert(conn);
        self.by_dcid.insert(scid_owned, handle);
        Ok(handle)
    }

    fn get(&self, handle: usize) -> Option<&QuicConnection> {
        self.connections.get(handle)
    }

    fn get_mut(&mut self, handle: usize) -> Option<&mut QuicConnection> {
        self.connections.get_mut(handle)
    }

    fn remove(&mut self, handle: usize) -> Option<QuicConnection> {
        if self.connections.contains(handle) {
            let conn = self.connections.remove(handle);
            self.by_dcid.retain(|_, &mut h| h != handle);
            Some(conn)
        } else {
            None
        }
    }

    fn fill_handles(&self, buf: &mut Vec<usize>) {
        buf.clear();
        buf.extend(self.connections.iter().map(|(handle, _)| handle));
    }

    fn drain_closed(&mut self) -> Vec<usize> {
        let closed: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, conn)| conn.is_closed())
            .map(|(handle, _)| handle)
            .collect();
        for &handle in &closed {
            self.remove(handle);
        }
        closed
    }
}

// ── Pending write ──────────────────────────────────────────────────

struct PendingWrite {
    data: Vec<u8>,
    fin: bool,
}

// ── Spawn functions ────────────────────────────────────────────────

pub struct QuicServerConfig {
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub max_connections: usize,
    pub disable_retry: bool,
    pub cid_encoding: CidEncoding,
}

pub fn spawn_quic_server(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    tsfn: EventTsfn,
) -> Result<QuicServerHandle, Http3NativeError> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let std_socket = UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io)?;
    std_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    let _ = transport::socket::set_socket_buffers(&std_socket, 2 * 1024 * 1024);
    let local_addr = std_socket.local_addr().map_err(Http3NativeError::Io)?;

    let (driver, waker) =
        transport::PlatformDriver::new(std_socket).map_err(Http3NativeError::Io)?;
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();

    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut handler = QuicServerHandler::new(quiche_config, server_config);
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, tsfn, local_addr);
    });

    Ok(QuicServerHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    })
}

pub fn spawn_quic_client(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    tsfn: EventTsfn,
) -> Result<QuicClientHandle, Http3NativeError> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let bind_addr = match server_addr {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        SocketAddr::V6(_) => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)),
    };
    let std_socket = UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io)?;
    std_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    let _ = transport::socket::set_socket_buffers(&std_socket, 2 * 1024 * 1024);
    let local_addr = std_socket.local_addr().map_err(Http3NativeError::Io)?;

    let (driver, waker) =
        transport::PlatformDriver::new(std_socket).map_err(Http3NativeError::Io)?;
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();

    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut quiche_config = quiche_config;
        let handler = QuicClientHandler::new(
            local_addr,
            server_addr,
            &server_name,
            session_ticket.as_deref(),
            qlog_dir.as_deref(),
            qlog_level.as_deref(),
            &mut quiche_config,
        );
        let Some(mut handler) = handler else { return };
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, tsfn, local_addr);
    });

    Ok(QuicClientHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    })
}

// ── QUIC Server Protocol Handler ────────────────────────────────────

struct QuicServerHandler {
    conn_map: QuicConnectionMap,
    timer_heap: TimerHeap,
    buffer_pool: BufferPool,
    pending_writes: HashMap<(u32, u64), PendingWrite>,
    conn_send_buffers: HashMap<usize, Vec<u8>>,
    handles_buf: Vec<usize>,
    server_config: QuicServerConfig,
    quiche_config: quiche::Config,
    disable_retry: bool,
    last_expired: Vec<usize>,
}

impl QuicServerHandler {
    fn new(quiche_config: quiche::Config, server_config: QuicServerConfig) -> Self {
        let disable_retry = server_config.disable_retry;
        Self {
            conn_map: QuicConnectionMap::new(
                server_config.max_connections,
                server_config.cid_encoding.clone(),
            ),
            timer_heap: TimerHeap::new(),
            buffer_pool: BufferPool::default(),
            pending_writes: HashMap::new(),
            conn_send_buffers: HashMap::new(),
            handles_buf: Vec::new(),
            server_config,
            quiche_config,
            disable_retry,
            last_expired: Vec::new(),
        }
    }
}

impl ProtocolHandler for QuicServerHandler {
    type Command = QuicServerCommand;

    fn dispatch_command(
        &mut self,
        cmd: QuicServerCommand,
        _batch: &mut Vec<JsH3Event>,
    ) -> bool {
        match cmd {
            QuicServerCommand::Shutdown => return true,
            QuicServerCommand::StreamSend {
                conn_handle,
                stream_id,
                data,
                fin,
            } => {
                let key = (conn_handle, stream_id);
                if let Some(pw) = self.pending_writes.get_mut(&key) {
                    pw.data.extend_from_slice(&data);
                    pw.fin = pw.fin || fin;
                } else if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let written = conn.stream_send(stream_id, &data, fin).unwrap_or(0);
                    if written < data.len() {
                        self.pending_writes.insert(
                            key,
                            PendingWrite {
                                data: data[written..].to_vec(),
                                fin,
                            },
                        );
                    } else if fin && written == 0 && data.is_empty() {
                        self.pending_writes.insert(
                            key,
                            PendingWrite {
                                data: Vec::new(),
                                fin: true,
                            },
                        );
                    }
                }
            }
            QuicServerCommand::StreamClose {
                conn_handle,
                stream_id,
                error_code,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let _ = conn.stream_close(stream_id, u64::from(error_code));
                }
            }
            QuicServerCommand::CloseSession {
                conn_handle,
                error_code,
                reason,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let _ = conn
                        .quiche_conn
                        .close(true, u64::from(error_code), reason.as_bytes());
                }
            }
            QuicServerCommand::SendDatagram {
                conn_handle,
                data,
                resp_tx,
            } => {
                let ok = self
                    .conn_map
                    .get_mut(conn_handle as usize)
                    .is_some_and(|conn| conn.send_datagram(&data).is_ok());
                let _ = resp_tx.send(ok);
            }
            QuicServerCommand::GetSessionMetrics {
                conn_handle,
                resp_tx,
            } => {
                let metrics = self
                    .conn_map
                    .get(conn_handle as usize)
                    .map(snapshot_quic_metrics);
                let _ = resp_tx.send(metrics);
            }
            QuicServerCommand::PingSession {
                conn_handle,
                resp_tx,
            } => {
                let ok = self
                    .conn_map
                    .get_mut(conn_handle as usize)
                    .is_some_and(|conn| conn.quiche_conn.send_ack_eliciting().is_ok());
                let _ = resp_tx.send(ok);
            }
            QuicServerCommand::GetQlogPath {
                conn_handle,
                resp_tx,
            } => {
                let path = self
                    .conn_map
                    .get(conn_handle as usize)
                    .and_then(|conn| conn.qlog_path.clone());
                let _ = resp_tx.send(path);
            }
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    fn process_packet(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        pending_outbound: &mut Vec<TxDatagram>,
        batch: &mut Vec<JsH3Event>,
    ) {
        let Ok(hdr) = quiche::Header::from_slice(buf, SCID_LEN) else {
            return;
        };

        let handle = if let Some(handle) = self.conn_map.route_packet(hdr.dcid.as_ref()) {
            handle
        } else {
            if hdr.ty != quiche::Type::Initial {
                return;
            }

            if self.disable_retry {
                let Ok(scid) = self.conn_map.generate_scid() else {
                    return;
                };
                let client_dcid = hdr.dcid.to_vec();
                match self.conn_map.accept_new(
                    &scid,
                    None,
                    peer,
                    local,
                    &mut self.quiche_config,
                    self.server_config.qlog_dir.as_deref(),
                    self.server_config.qlog_level.as_deref(),
                ) {
                    Ok(h) => {
                        self.conn_map.add_dcid(h, client_dcid);
                        batch.push(JsH3Event::new_session(
                            h as u32,
                            peer.ip().to_string(),
                            peer.port(),
                            String::new(),
                        ));
                        h
                    }
                    Err(_) => return,
                }
            } else if let Some(token) = hdr.token.as_ref().filter(|t| !t.is_empty()) {
                match self.conn_map.validate_token(token, &peer) {
                    Some(odcid) => {
                        let scid = hdr.dcid.to_vec();
                        let odcid_ref = quiche::ConnectionId::from_ref(&odcid);
                        match self.conn_map.accept_new(
                            &scid,
                            Some(&odcid_ref),
                            peer,
                            local,
                            &mut self.quiche_config,
                            self.server_config.qlog_dir.as_deref(),
                            self.server_config.qlog_level.as_deref(),
                        ) {
                            Ok(h) => {
                                self.conn_map.add_dcid(h, odcid);
                                batch.push(JsH3Event::new_session(
                                    h as u32,
                                    peer.ip().to_string(),
                                    peer.port(),
                                    String::new(),
                                ));
                                h
                            }
                            Err(_) => return,
                        }
                    }
                    None => return,
                }
            } else {
                let Ok(scid) = self.conn_map.generate_scid() else {
                    return;
                };
                let scid_ref = quiche::ConnectionId::from_ref(&scid);
                let token = self.conn_map.mint_token(&peer, hdr.dcid.as_ref());
                let mut out = self.buffer_pool.checkout();
                if let Ok(len) = quiche::retry(
                    &hdr.scid,
                    &hdr.dcid,
                    &scid_ref,
                    &token,
                    hdr.version,
                    &mut out,
                ) {
                    pending_outbound.push(TxDatagram {
                        data: out[..len].to_vec(),
                        to: peer,
                    });
                }
                self.buffer_pool.checkin(out);
                return;
            }
        };

        let recv_info = quiche::RecvInfo {
            from: peer,
            to: local,
        };

        let (timeout, current_scid, needs_dcid_update, retired_scids) = {
            let Some(conn) = self.conn_map.get_mut(handle) else {
                return;
            };
            if conn.recv(buf, recv_info).is_err() {
                return;
            }
            if conn.quiche_conn.is_established() && !conn.is_established {
                conn.mark_established();
            }
            if conn.quiche_conn.is_established() && !conn.handshake_complete_emitted {
                conn.handshake_complete_emitted = true;
                batch.push(JsH3Event::handshake_complete(handle as u32));
            }

            let current_scid: Vec<u8> = conn.quiche_conn.source_id().into_owned().to_vec();
            let needs_dcid_update = current_scid.as_slice() != conn.conn_id.as_slice();
            if needs_dcid_update {
                conn.conn_id = current_scid.clone();
            }

            conn.poll_quic_events(handle as u32, batch);

            let mut retired_scids = Vec::new();
            while let Some(retired) = conn.quiche_conn.retired_scid_next() {
                retired_scids.push(retired.into_owned().to_vec());
            }

            (
                conn.timeout(),
                current_scid,
                needs_dcid_update,
                retired_scids,
            )
        };

        if needs_dcid_update {
            self.conn_map.add_dcid(handle, current_scid);
        }
        for retired_scid in retired_scids {
            self.conn_map.remove_dcid(&retired_scid);
        }
        top_up_server_scids(&mut self.conn_map, handle);

        if let Some(timeout) = timeout {
            self.timer_heap.schedule(handle, Instant::now() + timeout);
        }
    }

    fn process_timers(&mut self, now: Instant, batch: &mut Vec<JsH3Event>) {
        self.last_expired = self.timer_heap.pop_expired(now);
        for &handle in &self.last_expired.clone() {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.on_timeout();
                if conn.is_closed() {
                    batch.push(JsH3Event::session_close(handle as u32));
                } else {
                    conn.poll_quic_events(handle as u32, batch);
                    if let Some(timeout) = conn.timeout() {
                        self.timer_heap.schedule(handle, Instant::now() + timeout);
                    }
                }
            }
        }
    }

    fn flush_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
        self.conn_map.fill_handles(&mut self.handles_buf);
        if self.handles_buf.is_empty() {
            return;
        }
        // Round-robin: pull one packet from each connection in turn until all
        // are drained.  This prevents a single busy connection from monopolizing
        // the socket send buffer under fan-out.
        let count = self.handles_buf.len();
        let mut done = vec![false; count];
        let mut active = count;
        while active > 0 {
            for i in 0..count {
                if done[i] {
                    continue;
                }
                let handle = self.handles_buf[i];
                let send_buf = self
                    .conn_send_buffers
                    .entry(handle)
                    .or_insert_with(|| vec![0u8; SEND_BUF_SIZE]);
                let sent = if let Some(conn) = self.conn_map.get_mut(handle) {
                    if let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
                        outbound.push(TxDatagram {
                            data: send_buf[..len].to_vec(),
                            to: send_info.to,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !sent {
                    done[i] = true;
                    active -= 1;
                }
            }
        }
    }

    fn flush_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        let flushed = flush_quic_pending_writes(&mut self.conn_map, &mut self.pending_writes);
        for (conn_handle, stream_id) in flushed {
            batch.push(JsH3Event::drain(conn_handle, stream_id));
        }
    }

    fn poll_drain_events(&mut self, batch: &mut Vec<JsH3Event>) {
        self.conn_map.fill_handles(&mut self.handles_buf);
        for i in 0..self.handles_buf.len() {
            let handle = self.handles_buf[i];
            if self.last_expired.contains(&handle) {
                continue;
            }
            if let Some(conn) = self.conn_map.get_mut(handle) {
                if !conn.blocked_streams.is_empty() {
                    conn.poll_drain_events(handle as u32, batch);
                }
            }
        }
    }

    fn cleanup_closed(&mut self, batch: &mut Vec<JsH3Event>) {
        let closed = self.conn_map.drain_closed();
        for handle in &closed {
            self.timer_heap.remove_connection(*handle);
            self.conn_send_buffers.remove(handle);
            self.pending_writes
                .retain(|&(ch, _), _| ch != *handle as u32);
            if !self.last_expired.contains(handle) {
                batch.push(JsH3Event::session_close(*handle as u32));
            }
        }
        self.last_expired.clear();
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        self.timer_heap.next_deadline()
    }
}

// ── QUIC Client Protocol Handler ────────────────────────────────────

struct QuicClientHandler {
    conn: QuicConnection,
    pending_writes: HashMap<u64, PendingWrite>,
    send_buf: Vec<u8>,
    timer_deadline: Option<Instant>,
    session_closed_emitted: bool,
}

impl QuicClientHandler {
    fn new(
        local_addr: SocketAddr,
        server_addr: SocketAddr,
        server_name: &str,
        session_ticket: Option<&[u8]>,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        quiche_config: &mut quiche::Config,
    ) -> Option<Self> {
        let Ok(scid) = CidEncoding::random().generate_scid() else {
            return None;
        };
        let scid_ref = quiche::ConnectionId::from_ref(&scid);
        let Ok(mut quiche_conn) = quiche::connect(
            Some(server_name),
            &scid_ref,
            local_addr,
            server_addr,
            quiche_config,
        ) else {
            return None;
        };
        if let Some(ticket) = session_ticket {
            let _ = quiche_conn.set_session(ticket);
        }
        let conn = QuicConnection::new(
            quiche_conn,
            scid,
            QuicConnectionInit {
                role: "client",
                qlog_dir,
                qlog_level,
            },
        );
        let timer_deadline = conn.timeout().map(|t| Instant::now() + t);
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            send_buf: vec![0u8; SEND_BUF_SIZE],
            timer_deadline,
            session_closed_emitted: false,
        })
    }
}

impl ProtocolHandler for QuicClientHandler {
    type Command = QuicClientCommand;

    fn dispatch_command(
        &mut self,
        cmd: QuicClientCommand,
        _batch: &mut Vec<JsH3Event>,
    ) -> bool {
        match cmd {
            QuicClientCommand::Shutdown => return true,
            QuicClientCommand::Close { error_code, reason } => {
                let _ = self
                    .conn
                    .quiche_conn
                    .close(true, u64::from(error_code), reason.as_bytes());
            }
            QuicClientCommand::StreamSend {
                stream_id,
                data,
                fin,
            } => {
                if let Some(pw) = self.pending_writes.get_mut(&stream_id) {
                    pw.data.extend_from_slice(&data);
                    pw.fin = pw.fin || fin;
                } else {
                    let written = self.conn.stream_send(stream_id, &data, fin).unwrap_or(0);
                    if written < data.len() {
                        self.pending_writes.insert(
                            stream_id,
                            PendingWrite {
                                data: data[written..].to_vec(),
                                fin,
                            },
                        );
                    } else if fin && written == 0 && data.is_empty() {
                        self.pending_writes.insert(
                            stream_id,
                            PendingWrite {
                                data: Vec::new(),
                                fin: true,
                            },
                        );
                    }
                }
            }
            QuicClientCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                let _ = self.conn.stream_close(stream_id, u64::from(error_code));
            }
            QuicClientCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.conn.send_datagram(&data).is_ok());
            }
            QuicClientCommand::GetSessionMetrics { resp_tx } => {
                let _ = resp_tx.send(Some(snapshot_quic_metrics(&self.conn)));
            }
            QuicClientCommand::Ping { resp_tx } => {
                let _ = resp_tx.send(self.conn.quiche_conn.send_ack_eliciting().is_ok());
            }
            QuicClientCommand::GetQlogPath { resp_tx } => {
                let _ = resp_tx.send(self.conn.qlog_path.clone());
            }
        }
        false
    }

    fn process_packet(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        _pending_outbound: &mut Vec<TxDatagram>,
        batch: &mut Vec<JsH3Event>,
    ) {
        let recv_info = quiche::RecvInfo {
            from: peer,
            to: local,
        };
        if self.conn.recv(buf, recv_info).is_err() {
            return;
        }
        if self.conn.quiche_conn.is_established() && !self.conn.is_established {
            self.conn.mark_established();
        }
        if self.conn.quiche_conn.is_established() && !self.conn.handshake_complete_emitted {
            self.conn.handshake_complete_emitted = true;
            batch.push(JsH3Event::handshake_complete(0));
        }
        self.conn.poll_quic_events(0, batch);
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(0, ticket));
        }
        self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);

        if self.conn.is_closed() && !self.session_closed_emitted {
            batch.push(JsH3Event::session_close(0));
            self.session_closed_emitted = true;
        }
    }

    fn process_timers(&mut self, now: Instant, batch: &mut Vec<JsH3Event>) {
        if self.timer_deadline.is_some_and(|d| d <= now) {
            self.conn.on_timeout();
            if self.conn.is_closed() && !self.session_closed_emitted {
                batch.push(JsH3Event::session_close(0));
                self.session_closed_emitted = true;
            } else {
                self.conn.poll_quic_events(0, batch);
                if let Some(ticket) = self.conn.update_session_ticket() {
                    batch.push(JsH3Event::session_ticket(0, ticket));
                }
                self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);
            }
        }
    }

    fn flush_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
        while let Ok((len, send_info)) = self.conn.send(self.send_buf.as_mut_slice()) {
            outbound.push(TxDatagram {
                data: self.send_buf[..len].to_vec(),
                to: send_info.to,
            });
        }
    }

    fn flush_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        let flushed =
            flush_quic_client_pending_writes(&mut self.conn, &mut self.pending_writes);
        for stream_id in flushed {
            batch.push(JsH3Event::drain(0, stream_id));
        }
    }

    fn poll_drain_events(&mut self, batch: &mut Vec<JsH3Event>) {
        if !self.conn.blocked_streams.is_empty() {
            self.conn.poll_drain_events(0, batch);
        }
    }

    fn cleanup_closed(&mut self, _batch: &mut Vec<JsH3Event>) {
        // Client session_close is emitted in process_packet / process_timers.
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        self.timer_deadline
    }

    fn is_done(&self) -> bool {
        self.session_closed_emitted
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn top_up_server_scids(conn_map: &mut QuicConnectionMap, handle: usize) {
    loop {
        let should_add = match conn_map.get_mut(handle) {
            Some(conn) => conn.quiche_conn.is_established() && conn.quiche_conn.scids_left() > 0,
            None => return,
        };
        if !should_add {
            break;
        }
        let Ok(scid) = conn_map.generate_scid() else {
            break;
        };
        let Ok(reset_token) = generate_stateless_reset_token() else {
            break;
        };
        let added = match conn_map.get_mut(handle) {
            Some(conn) => {
                let scid_ref = quiche::ConnectionId::from_ref(&scid);
                conn.quiche_conn
                    .new_scid(&scid_ref, reset_token, true)
                    .is_ok()
            }
            None => return,
        };
        if !added {
            break;
        }
        conn_map.add_dcid(handle, scid);
    }
}

fn generate_stateless_reset_token() -> Result<u128, Http3NativeError> {
    let rng = ring::rand::SystemRandom::new();
    let mut token = [0u8; 16];
    rng.fill(&mut token)
        .map_err(|_| Http3NativeError::Config("cryptographic RNG failed".into()))?;
    Ok(u128::from_be_bytes(token))
}

fn snapshot_quic_metrics(conn: &QuicConnection) -> JsSessionMetrics {
    JsSessionMetrics {
        packets_in: conn.metrics.packets_in,
        packets_out: conn.metrics.packets_out,
        bytes_in: conn.metrics.bytes_in as i64,
        bytes_out: conn.metrics.bytes_out as i64,
        handshake_time_ms: conn.handshake_time_ms(),
        rtt_ms: conn.rtt_ms(),
        cwnd: conn.cwnd() as i64,
    }
}

fn flush_quic_pending_writes(
    conn_map: &mut QuicConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
) -> Vec<(u32, u64)> {
    let mut flushed = Vec::new();
    pending.retain(|&(conn_handle, stream_id), pw| {
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            return false;
        };
        let written = conn.stream_send(stream_id, &pw.data, pw.fin).unwrap_or(0);
        if written == 0 && pw.fin && pw.data.is_empty() {
            true
        } else if written >= pw.data.len() {
            flushed.push((conn_handle, stream_id));
            false
        } else {
            if written > 0 {
                pw.data.drain(..written);
            }
            true
        }
    });
    flushed
}

fn flush_quic_client_pending_writes(
    conn: &mut QuicConnection,
    pending: &mut HashMap<u64, PendingWrite>,
) -> Vec<u64> {
    let mut flushed = Vec::new();
    pending.retain(|&stream_id, pw| {
        let written = conn.stream_send(stream_id, &pw.data, pw.fin).unwrap_or(0);
        if written == 0 && pw.fin && pw.data.is_empty() {
            true
        } else if written >= pw.data.len() {
            flushed.push(stream_id);
            false
        } else {
            if written > 0 {
                pw.data.drain(..written);
            }
            true
        }
    });
    flushed
}
