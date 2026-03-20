//! Worker thread loops for raw QUIC (no HTTP/3 framing).
//! Shares the event delivery mechanism (TSFN) with the H3 worker
//! but uses direct `stream_send` / `stream_recv` instead of H3 framing.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock, Weak,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use ring::hmac;
use ring::rand::SecureRandom;
use slab::Slab;

use crate::buffer_pool::BufferPool;
use crate::cid::CidEncoding;
use crate::client_topology::{
    default_quic_client_socket_strategy, shared_client_bind_addr, shared_client_worker_key,
    ClientSocketStrategy, SharedClientWorkerKey as SharedQuicClientWorkerKey,
};
use crate::config::{ClientAuthMode, TransportRuntimeMode};
use crate::error::Http3NativeError;
use crate::event_loop::{self, EventBatcher, ProtocolHandler, MAX_BATCH_SIZE, SEND_BUF_SIZE};
#[cfg(feature = "node-api")]
use crate::event_loop::EventTsfn;
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::quic_connection::{QuicConnection, QuicConnectionInit};
use crate::reactor_metrics::{self, RawQuicClientCloseCause, SessionKind, WorkerSpawnKind};
use crate::shared_client_reactor;
use crate::timer_heap::TimerHeap;
use crate::transport::{self, ErasedWaker, TxDatagram};

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

enum SharedQuicClientCommand {
    OpenSession {
        quiche_config: quiche::Config,
        server_addr: SocketAddr,
        server_name: String,
        session_ticket: Option<Vec<u8>>,
        qlog_dir: Option<String>,
        qlog_level: Option<String>,
        batcher: EventBatcher,
        resp_tx: Sender<Result<u32, Http3NativeError>>,
    },
    StreamSend {
        session_handle: u32,
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
    },
    StreamClose {
        session_handle: u32,
        stream_id: u64,
        error_code: u32,
    },
    SendDatagram {
        session_handle: u32,
        data: Vec<u8>,
        resp_tx: Sender<bool>,
    },
    GetSessionMetrics {
        session_handle: u32,
        resp_tx: Sender<Option<JsSessionMetrics>>,
    },
    Ping {
        session_handle: u32,
        resp_tx: Sender<bool>,
    },
    GetQlogPath {
        session_handle: u32,
        resp_tx: Sender<Option<String>>,
    },
    Close {
        session_handle: u32,
        error_code: u32,
        reason: String,
    },
    ReleaseSession {
        session_handle: u32,
    },
}

struct SharedQuicClientWorkerControl {
    cmd_tx: Sender<SharedQuicClientCommand>,
    waker: Arc<dyn ErasedWaker>,
    local_addr: SocketAddr,
    join_handle: Mutex<Option<thread::JoinHandle<()>>>,
    running: AtomicBool,
    session_count: AtomicUsize,
    key: SharedQuicClientWorkerKey,
}

impl SharedQuicClientWorkerControl {
    fn wake(&self) {
        let _ = self.waker.wake();
    }
}

enum QuicClientHandleKind {
    Dedicated {
        cmd_tx: Sender<QuicClientCommand>,
        join_handle: Option<thread::JoinHandle<()>>,
        waker: Arc<dyn ErasedWaker>,
    },
    Shared {
        session_handle: u32,
        worker: Arc<SharedQuicClientWorkerControl>,
    },
}

pub struct QuicClientHandle {
    kind: Option<QuicClientHandleKind>,
    local_addr: SocketAddr,
}

impl QuicClientHandle {
    pub fn stream_send(&self, stream_id: u64, data: Vec<u8>, fin: bool) -> bool {
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(QuicClientCommand::StreamSend {
                        stream_id,
                        data,
                        fin,
                    })
                    .is_ok()
                {
                    let _ = waker.wake();
                    true
                } else {
                    false
                }
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::StreamSend {
                        session_handle: *session_handle,
                        stream_id,
                        data,
                        fin,
                    })
                    .is_ok()
                {
                    worker.wake();
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    pub fn stream_close(&self, stream_id: u64, error_code: u32) -> bool {
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(QuicClientCommand::StreamClose {
                        stream_id,
                        error_code,
                    })
                    .is_ok()
                {
                    let _ = waker.wake();
                    true
                } else {
                    false
                }
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::StreamClose {
                        session_handle: *session_handle,
                        stream_id,
                        error_code,
                    })
                    .is_ok()
                {
                    worker.wake();
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    pub fn send_datagram(&self, data: Vec<u8>) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(QuicClientCommand::SendDatagram { data, resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("quic client not running".into())
                    })?;
                let _ = waker.wake();
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::SendDatagram {
                        session_handle: *session_handle,
                        data,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState(
                            "shared quic client worker not running".into(),
                        )
                    })?;
                worker.wake();
            }
            None => return Err(Http3NativeError::InvalidState("quic client not running".into())),
        }
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for datagram".into()))
    }

    pub fn get_session_metrics(&self) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(QuicClientCommand::GetSessionMetrics { resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("quic client not running".into())
                    })?;
                let _ = waker.wake();
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::GetSessionMetrics {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState(
                            "shared quic client worker not running".into(),
                        )
                    })?;
                worker.wake();
            }
            None => return Err(Http3NativeError::InvalidState("quic client not running".into())),
        }
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for metrics".into()))
    }

    pub fn ping(&self) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(QuicClientCommand::Ping { resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("quic client not running".into())
                    })?;
                let _ = waker.wake();
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::Ping {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState(
                            "shared quic client worker not running".into(),
                        )
                    })?;
                worker.wake();
            }
            None => return Err(Http3NativeError::InvalidState("quic client not running".into())),
        }
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for ping".into()))
    }

    pub fn get_qlog_path(&self) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(QuicClientCommand::GetQlogPath { resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("quic client not running".into())
                    })?;
                let _ = waker.wake();
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::GetQlogPath {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState(
                            "shared quic client worker not running".into(),
                        )
                    })?;
                worker.wake();
            }
            None => return Err(Http3NativeError::InvalidState("quic client not running".into())),
        }
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for qlog path".into()))
    }

    pub fn close(&self, error_code: u32, reason: String) -> bool {
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(QuicClientCommand::Close { error_code, reason })
                    .is_ok()
                {
                    let _ = waker.wake();
                    true
                } else {
                    false
                }
            }
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::Close {
                        session_handle: *session_handle,
                        error_code,
                        reason,
                    })
                    .is_ok()
                {
                    worker.wake();
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(&mut self) {
        let Some(kind) = self.kind.take() else {
            return;
        };
        match kind {
            QuicClientHandleKind::Dedicated {
                cmd_tx,
                mut join_handle,
                waker,
            } => {
                let _ = cmd_tx.send(QuicClientCommand::Shutdown);
                let _ = waker.wake();
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
            }
            QuicClientHandleKind::Shared {
                session_handle,
                worker,
            } => {
                let _ = worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::ReleaseSession { session_handle });
                worker.wake();
                if worker.session_count.fetch_sub(1, Ordering::AcqRel) == 1 {
                    if let Ok(mut join_handle) = worker.join_handle.lock() {
                        if let Some(handle) = join_handle.take() {
                            let _ = handle.join();
                        }
                    }
                    if let Ok(mut registry) = shared_quic_client_worker_registry().lock() {
                        registry.remove(&worker.key);
                    }
                }
            }
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
    pub client_auth: ClientAuthMode,
    pub cid_encoding: CidEncoding,
    pub runtime_mode: TransportRuntimeMode,
}

pub(crate) fn spawn_dedicated_quic_server_on_driver<D>(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<QuicServerCommand>,
    cmd_rx: Receiver<QuicServerCommand>,
    batcher: EventBatcher,
) -> Result<QuicServerHandle, Http3NativeError>
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();

    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::RawQuicServer);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut handler = QuicServerHandler::new(quiche_config, server_config);
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, batcher, local_addr);
    });

    Ok(QuicServerHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_dedicated_quic_client_on_driver<D>(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<QuicClientCommand>,
    cmd_rx: Receiver<QuicClientCommand>,
    batcher: EventBatcher,
) -> Result<QuicClientHandle, Http3NativeError>
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();

    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::RawQuicClientDedicated);
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
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, batcher, local_addr);
    });

    Ok(QuicClientHandle {
        kind: Some(QuicClientHandleKind::Dedicated {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_clone,
        }),
        local_addr,
    })
}

#[cfg(feature = "node-api")]
pub fn spawn_quic_server(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    user_set_mtu: bool,
    tsfn: EventTsfn,
) -> Result<QuicServerHandle, Http3NativeError> {
    spawn_quic_server_with_batcher(
        quiche_config,
        server_config,
        bind_addr,
        user_set_mtu,
        EventBatcher::new_tsfn(tsfn),
    )
}

pub(crate) fn spawn_quic_server_with_batcher(
    mut quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    user_set_mtu: bool,
    batcher: EventBatcher,
) -> Result<QuicServerHandle, Http3NativeError> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let std_socket = UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io)?;
    std_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    let _ = transport::socket::set_socket_buffers(&std_socket, 2 * 1024 * 1024);
    let local_addr = std_socket.local_addr().map_err(Http3NativeError::Io)?;

    // Loopback MTU auto-detection
    if !user_set_mtu {
        let mtu = crate::config::effective_max_datagram_size(&local_addr);
        quiche_config.set_max_recv_udp_payload_size(mtu);
        quiche_config.set_max_send_udp_payload_size(mtu);
    }

    let (driver, waker) = transport::create_platform_driver(std_socket, server_config.runtime_mode)?;
    spawn_dedicated_quic_server_on_driver(
        quiche_config,
        server_config,
        driver,
        waker,
        local_addr,
        cmd_tx,
        cmd_rx,
        batcher,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "node-api")]
pub fn spawn_quic_client(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    user_set_mtu: bool,
    runtime_mode: TransportRuntimeMode,
    tsfn: EventTsfn,
) -> Result<QuicClientHandle, Http3NativeError> {
    spawn_quic_client_with_batcher(
        quiche_config,
        server_addr,
        server_name,
        session_ticket,
        qlog_dir,
        qlog_level,
        user_set_mtu,
        runtime_mode,
        EventBatcher::new_tsfn(tsfn),
    )
}

pub(crate) fn spawn_quic_client_with_batcher(
    mut quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    user_set_mtu: bool,
    runtime_mode: TransportRuntimeMode,
    batcher: EventBatcher,
) -> Result<QuicClientHandle, Http3NativeError> {
    if default_quic_client_socket_strategy(runtime_mode) == ClientSocketStrategy::SharedPerFamily {
        return spawn_shared_quic_client(
            quiche_config,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            user_set_mtu,
            runtime_mode,
            batcher,
        );
    }

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let bind_addr = shared_client_bind_addr(server_addr);
    let (driver, waker, local_addr) =
        transport::prepare_client_platform_driver(bind_addr, runtime_mode)?;

    // Loopback MTU auto-detection (check server address)
    if !user_set_mtu {
        let mtu = crate::config::effective_max_datagram_size(&server_addr);
        quiche_config.set_max_recv_udp_payload_size(mtu);
        quiche_config.set_max_send_udp_payload_size(mtu);
    }

    spawn_dedicated_quic_client_on_driver(
        quiche_config,
        server_addr,
        server_name,
        session_ticket,
        qlog_dir,
        qlog_level,
        driver,
        waker,
        local_addr,
        cmd_tx,
        cmd_rx,
        batcher,
    )
}

struct SharedQuicClientSession {
    handler: QuicClientHandler,
    batcher: EventBatcher,
    server_addr: SocketAddr,
}

fn shared_quic_client_worker_registry(
) -> &'static Mutex<HashMap<SharedQuicClientWorkerKey, Weak<SharedQuicClientWorkerControl>>> {
    static REGISTRY: OnceLock<
        Mutex<HashMap<SharedQuicClientWorkerKey, Weak<SharedQuicClientWorkerControl>>>,
    > = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire_shared_quic_client_worker(
    server_addr: SocketAddr,
    runtime_mode: TransportRuntimeMode,
) -> Result<Arc<SharedQuicClientWorkerControl>, Http3NativeError> {
    let key = shared_client_worker_key(server_addr, runtime_mode);
    let mut registry = shared_quic_client_worker_registry()
        .lock()
        .map_err(|_| Http3NativeError::InvalidState("shared quic client registry poisoned".into()))?;
    if let Some(worker) = registry.get(&key).and_then(Weak::upgrade) {
        if worker.running.load(Ordering::Acquire) {
            reactor_metrics::record_shared_worker_reuse(WorkerSpawnKind::RawQuicClientShared);
            return Ok(worker);
        }
    }

    let bind_addr = shared_client_bind_addr(server_addr);
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (driver, waker, local_addr) =
        transport::prepare_client_platform_driver(bind_addr, runtime_mode)?;
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let control = Arc::new(SharedQuicClientWorkerControl {
        cmd_tx,
        waker: waker_arc.clone(),
        local_addr,
        join_handle: Mutex::new(None),
        running: AtomicBool::new(true),
        session_count: AtomicUsize::new(0),
        key: key.clone(),
    });
    let control_for_thread = Arc::clone(&control);
    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::RawQuicClientShared);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        run_shared_quic_client_event_loop(&mut driver, cmd_rx, local_addr);
        control_for_thread.running.store(false, Ordering::Release);
    });
    if let Ok(mut slot) = control.join_handle.lock() {
        *slot = Some(join_handle);
    }
    registry.insert(key, Arc::downgrade(&control));
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn spawn_shared_quic_client(
    mut quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    user_set_mtu: bool,
    runtime_mode: TransportRuntimeMode,
    batcher: EventBatcher,
) -> Result<QuicClientHandle, Http3NativeError> {
    let worker = acquire_shared_quic_client_worker(server_addr, runtime_mode)?;

    if !user_set_mtu {
        let mtu = crate::config::effective_max_datagram_size(&server_addr);
        quiche_config.set_max_recv_udp_payload_size(mtu);
        quiche_config.set_max_send_udp_payload_size(mtu);
    }

    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    worker
        .cmd_tx
        .send(SharedQuicClientCommand::OpenSession {
            quiche_config,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            batcher,
            resp_tx,
        })
        .map_err(|_| Http3NativeError::InvalidState("shared quic client worker not running".into()))?;
    worker.wake();

    let session_handle = resp_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for shared quic session".into())
        })??;
    worker.session_count.fetch_add(1, Ordering::AcqRel);

    Ok(QuicClientHandle {
        kind: Some(QuicClientHandleKind::Shared {
            session_handle,
            worker: Arc::clone(&worker),
        }),
        local_addr: worker.local_addr,
    })
}

fn emit_shared_quic_client_runtime_error<D: transport::Driver>(
    sessions: &mut Slab<SharedQuicClientSession>,
    driver: &D,
    syscall: &str,
    reason_code: &str,
    err: &std::io::Error,
) {
    shared_client_reactor::emit_runtime_error(
        sessions,
        driver,
        syscall,
        reason_code,
        err,
        |session| &mut session.batcher,
    );
}

fn remove_shared_quic_client_session(
    sessions: &mut Slab<SharedQuicClientSession>,
    route_by_dcid: &mut HashMap<Vec<u8>, usize>,
    timer_heap: &mut TimerHeap,
    handle: usize,
) {
    if !sessions.contains(handle) {
        return;
    }
    if let Some(session) = sessions.get(handle) {
        reactor_metrics::record_raw_quic_client_reap(
            session.handler.pending_writes.len(),
            session.handler.conn.blocked_queue.len(),
            session.handler.conn.known_streams.len(),
        );
        if !session.handler.session_closed_emitted {
            reactor_metrics::record_raw_quic_client_close_cause(
                RawQuicClientCloseCause::Release,
            );
            reactor_metrics::record_session_close(SessionKind::RawQuicClient);
        }
    }
    timer_heap.remove_connection(handle);
    let _ = sessions.remove(handle);
    route_by_dcid.retain(|_, mapped_handle| *mapped_handle != handle);
}

fn refresh_shared_quic_client_dcid(
    route_by_dcid: &mut HashMap<Vec<u8>, usize>,
    handle: usize,
    session: &mut SharedQuicClientSession,
) {
    let (current_dcid, needs_update, retired_dcids) = session.handler.take_dcid_updates();
    if needs_update {
        route_by_dcid.insert(current_dcid, handle);
    }
    for retired in retired_dcids {
        route_by_dcid.remove(&retired);
    }
}

fn sync_shared_quic_client_timer(
    timer_heap: &mut TimerHeap,
    handle: usize,
    session: &SharedQuicClientSession,
) {
    shared_client_reactor::sync_timer(timer_heap, handle, session, |current| {
        current.handler.timer_deadline
    });
}

fn flush_shared_quic_client_sends(
    sessions: &mut Slab<SharedQuicClientSession>,
    handles_buf: &mut Vec<usize>,
    tx_pool: &mut BufferPool,
    outbound: &mut Vec<TxDatagram>,
) {
    shared_client_reactor::flush_round_robin_sends(
        sessions,
        handles_buf,
        outbound,
        |session| {
            QuicClientHandler::try_send_next_with_pool_parts(
                &mut session.handler.conn,
                session.handler.send_buf.as_mut_slice(),
                tx_pool,
            )
        },
    );
}

fn refresh_shared_quic_client_timers_after_sends(
    sessions: &mut Slab<SharedQuicClientSession>,
    timer_heap: &mut TimerHeap,
    handles_buf: &mut Vec<usize>,
) {
    handles_buf.clear();
    handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
    for handle in handles_buf.iter().copied() {
        if let Some(session) = sessions.get_mut(handle) {
            session.handler.refresh_timeout_deadline();
            sync_shared_quic_client_timer(timer_heap, handle, session);
        }
    }
}

fn run_shared_quic_client_event_loop<D: transport::Driver>(
    driver: &mut D,
    cmd_rx: crossbeam_channel::Receiver<SharedQuicClientCommand>,
    local_addr: SocketAddr,
) {
    let mut sessions: Slab<SharedQuicClientSession> = Slab::new();
    let mut route_by_dcid: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut timer_heap = TimerHeap::new();
    let mut tx_pool = BufferPool::new(512, 1350);
    let mut handles_buf = Vec::new();
    let mut outbound = Vec::new();
    let mut closed_sessions = Vec::new();

    loop {
        let deadline = timer_heap.next_deadline();

        let outcome = match driver.poll(deadline) {
            Ok(outcome) => outcome,
            Err(err) => {
                emit_shared_quic_client_runtime_error(
                    &mut sessions,
                    driver,
                    "poll",
                    "driver-poll-failed",
                    &err,
                );
                return;
            }
        };

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                SharedQuicClientCommand::OpenSession {
                    mut quiche_config,
                    server_addr,
                    server_name,
                    session_ticket,
                    qlog_dir,
                    qlog_level,
                    batcher,
                    resp_tx,
                } => {
                    let handler = QuicClientHandler::new(
                        local_addr,
                        server_addr,
                        &server_name,
                        session_ticket.as_deref(),
                        qlog_dir.as_deref(),
                        qlog_level.as_deref(),
                        &mut quiche_config,
                    );
                    let result = handler.map_or_else(
                        || {
                            Err(Http3NativeError::Config(
                                "failed to create quic client session".into(),
                            ))
                        },
                        |handler| {
                            let dcid = handler.current_dcid();
                            let handle = sessions.insert(SharedQuicClientSession {
                                handler,
                                batcher,
                                server_addr,
                            });
                            route_by_dcid.insert(dcid, handle);
                            if let Some(session) = sessions.get(handle) {
                                sync_shared_quic_client_timer(&mut timer_heap, handle, session);
                            }
                            Ok(handle as u32)
                        },
                    );
                    let _ = resp_tx.send(result);
                }
                SharedQuicClientCommand::StreamSend {
                    session_handle,
                    stream_id,
                    data,
                    fin,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.queue_stream_send(stream_id, data, fin);
                    }
                }
                SharedQuicClientCommand::StreamClose {
                    session_handle,
                    stream_id,
                    error_code,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.close_stream(stream_id, error_code);
                    }
                }
                SharedQuicClientCommand::SendDatagram {
                    session_handle,
                    data,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.send_datagram(&data));
                    let _ = resp_tx.send(ok);
                }
                SharedQuicClientCommand::GetSessionMetrics {
                    session_handle,
                    resp_tx,
                } => {
                    let metrics = sessions
                        .get(session_handle as usize)
                        .map(|session| session.handler.metrics_snapshot());
                    let _ = resp_tx.send(metrics);
                }
                SharedQuicClientCommand::Ping {
                    session_handle,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.ping());
                    let _ = resp_tx.send(ok);
                }
                SharedQuicClientCommand::GetQlogPath {
                    session_handle,
                    resp_tx,
                } => {
                    let path = sessions
                        .get(session_handle as usize)
                        .and_then(|session| session.handler.qlog_path());
                    let _ = resp_tx.send(path);
                }
                SharedQuicClientCommand::Close {
                    session_handle,
                    error_code,
                    reason,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.close_session(error_code, &reason);
                    }
                }
                SharedQuicClientCommand::ReleaseSession { session_handle } => {
                    remove_shared_quic_client_session(
                        &mut sessions,
                        &mut route_by_dcid,
                        &mut timer_heap,
                        session_handle as usize,
                    );
                }
            }
        }

        flush_shared_quic_client_sends(&mut sessions, &mut handles_buf, &mut tx_pool, &mut outbound);
        refresh_shared_quic_client_timers_after_sends(
            &mut sessions,
            &mut timer_heap,
            &mut handles_buf,
        );
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                emit_shared_quic_client_runtime_error(
                    &mut sessions,
                    driver,
                    "submit_sends",
                    "driver-submit-sends-failed",
                    &err,
                );
                return;
            }
        }

        let rx_count = outcome.rx.len();
        for (rx_idx, mut pkt) in outcome.rx.into_iter().enumerate() {
            let Ok(header) = quiche::Header::from_slice(pkt.data.as_mut_slice(), SCID_LEN) else {
                continue;
            };
            let Some(handle) = route_by_dcid.get(header.dcid.as_ref()).copied() else {
                continue;
            };
            let mut should_remove = false;
            if let Some(session) = sessions.get_mut(handle) {
                if pkt.peer == session.server_addr {
                    session.handler.process_packet_for_handle(
                        pkt.data.as_mut_slice(),
                        pkt.peer,
                        local_addr,
                        &mut session.batcher.batch,
                        handle as u32,
                    );
                    refresh_shared_quic_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_quic_client_timer(&mut timer_heap, handle, session);
                    if session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush() {
                        should_remove = true;
                    }
                }
            }
            if should_remove {
                remove_shared_quic_client_session(
                    &mut sessions,
                    &mut route_by_dcid,
                    &mut timer_heap,
                    handle,
                );
            }
            if (rx_idx + 1) % 64 == 0 && rx_idx + 1 < rx_count {
                flush_shared_quic_client_sends(
                    &mut sessions,
                    &mut handles_buf,
                    &mut tx_pool,
                    &mut outbound,
                );
                refresh_shared_quic_client_timers_after_sends(
                    &mut sessions,
                    &mut timer_heap,
                    &mut handles_buf,
                );
                if !outbound.is_empty() {
                    if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                        emit_shared_quic_client_runtime_error(
                            &mut sessions,
                            driver,
                            "submit_sends",
                            "driver-submit-sends-failed",
                            &err,
                        );
                        return;
                    }
                }
            }
        }

        let now = Instant::now();
        closed_sessions.clear();
        for handle in timer_heap.pop_expired(now) {
            if let Some(session) = sessions.get_mut(handle) {
                session.handler.process_timers_for_handle(
                    now,
                    &mut session.batcher.batch,
                    handle as u32,
                );
                refresh_shared_quic_client_dcid(&mut route_by_dcid, handle, session);
                sync_shared_quic_client_timer(&mut timer_heap, handle, session);
                if session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush() {
                    closed_sessions.push(handle);
                }
            }
        }
        handles_buf.clear();
        handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
        for handle in handles_buf.iter().copied() {
            if let Some(session) = sessions.get_mut(handle) {
                session
                    .handler
                    .poll_drain_events_for_handle(&mut session.batcher.batch, handle as u32);
                session.handler.flush_pending_writes_for_handle(
                    &mut session.batcher.batch,
                    handle as u32,
                );
                if session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush() {
                    closed_sessions.push(handle);
                }
            }
        }
        for handle in closed_sessions.drain(..) {
            remove_shared_quic_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                handle,
            );
        }

        flush_shared_quic_client_sends(&mut sessions, &mut handles_buf, &mut tx_pool, &mut outbound);
        refresh_shared_quic_client_timers_after_sends(
            &mut sessions,
            &mut timer_heap,
            &mut handles_buf,
        );
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                emit_shared_quic_client_runtime_error(
                    &mut sessions,
                    driver,
                    "submit_sends",
                    "driver-submit-sends-failed",
                    &err,
                );
                return;
            }
        }
        let recycled = driver.drain_recycled_tx();
        if !recycled.is_empty() {
            reactor_metrics::record_tx_buffers_recycled(recycled.len());
            for buf in recycled {
                tx_pool.checkin(buf);
            }
        }

        closed_sessions.clear();
        handles_buf.clear();
        handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
        for handle in handles_buf.iter().copied() {
            if let Some(session) = sessions.get_mut(handle) {
                if !session.batcher.flush() || session.handler.is_reapable() {
                    closed_sessions.push(handle);
                }
            }
        }
        for handle in closed_sessions.drain(..) {
            remove_shared_quic_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                handle,
            );
        }

        if sessions.is_empty() && driver.pending_tx_count() == 0 {
            return;
        }
    }
}

// ── QUIC Server Protocol Handler ────────────────────────────────────

struct QuicServerHandler {
    conn_map: QuicConnectionMap,
    timer_heap: TimerHeap,
    buffer_pool: BufferPool,
    tx_pool: BufferPool,
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
            tx_pool: BufferPool::new(512, 1350),
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
                        reactor_metrics::record_session_open(SessionKind::RawQuicServer);
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
                                reactor_metrics::record_session_open(SessionKind::RawQuicServer);
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
                if self.server_config.client_auth.require_client_cert()
                    && conn.quiche_conn.peer_cert().is_none()
                {
                    let _ = conn
                        .quiche_conn
                        .close(false, 0x0100, b"client certificate required");
                } else {
                    let peer_certificate_chain = conn
                        .quiche_conn
                        .peer_cert_chain()
                        .map(|chain| {
                            chain
                                .into_iter()
                                .map(|certificate| certificate.to_vec())
                                .collect()
                        })
                        .or_else(|| {
                            conn.quiche_conn
                                .peer_cert()
                                .map(|certificate| vec![certificate.to_vec()])
                        });
                    conn.handshake_complete_emitted = true;
                    batch.push(JsH3Event::handshake_complete_with_peer_certificate(
                        handle as u32,
                        conn.quiche_conn.peer_cert().is_some(),
                        peer_certificate_chain,
                    ));
                }
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
        self.last_expired.sort_unstable();
        self.last_expired.dedup();
        for &handle in &self.last_expired {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.on_timeout();
                if conn.is_closed() {
                    reactor_metrics::record_session_close(SessionKind::RawQuicServer);
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
                let sent = if let Some(conn) = self.conn_map.get_mut(handle) {
                    // Write directly into a pool buffer — no intermediate copy.
                    let mut tx_buf = self.tx_pool.checkout();
                    if let Ok((len, send_info)) = conn.send(tx_buf.as_mut_slice()) {
                        tx_buf.truncate(len);
                        outbound.push(TxDatagram {
                            data: tx_buf,
                            to: send_info.to,
                        });
                        true
                    } else {
                        self.tx_pool.checkin(tx_buf);
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
            reactor_metrics::record_raw_quic_drain_event();
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
                if !conn.blocked_set.is_empty() {
                    conn.poll_drain_events(handle as u32, batch);
                }
            }
        }
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        reactor_metrics::record_tx_buffers_recycled(buffers.len());
        for buf in buffers {
            self.tx_pool.checkin(buf);
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
                reactor_metrics::record_session_close(SessionKind::RawQuicServer);
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
    tx_pool: BufferPool,
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
        reactor_metrics::record_session_open(SessionKind::RawQuicClient);
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            send_buf: vec![0u8; SEND_BUF_SIZE],
            tx_pool: BufferPool::new(256, 1350),
            timer_deadline,
            session_closed_emitted: false,
        })
    }

    fn current_dcid(&self) -> Vec<u8> {
        self.conn.quiche_conn.source_id().into_owned().to_vec()
    }

    fn take_dcid_updates(&mut self) -> (Vec<u8>, bool, Vec<Vec<u8>>) {
        let current_dcid = self.current_dcid();
        let needs_update = current_dcid.as_slice() != self.conn.conn_id.as_slice();
        if needs_update {
            self.conn.conn_id = current_dcid.clone();
        }
        let mut retired_dcids = Vec::new();
        while let Some(retired) = self.conn.quiche_conn.retired_scid_next() {
            retired_dcids.push(retired.into_owned().to_vec());
        }
        (current_dcid, needs_update, retired_dcids)
    }

    fn close_session(&mut self, error_code: u32, reason: &str) {
        let _ = self
            .conn
            .quiche_conn
            .close(true, u64::from(error_code), reason.as_bytes());
    }

    fn refresh_timeout_deadline(&mut self) {
        self.timer_deadline = self.conn.timeout().map(|timeout| Instant::now() + timeout);
    }

    fn queue_stream_send(&mut self, stream_id: u64, data: Vec<u8>, fin: bool) {
        if let Some(pw) = self.pending_writes.get_mut(&stream_id) {
            pw.data.extend_from_slice(&data);
            pw.fin = pw.fin || fin;
            return;
        }

        let written = self.conn.stream_send(stream_id, &data, fin).unwrap_or(0);
        if written < data.len() {
            self.pending_writes.insert(
                stream_id,
                PendingWrite {
                    data: data[written..].to_vec(),
                    fin,
                },
            );
            reactor_metrics::record_raw_quic_client_pending_writes(self.pending_writes.len());
        } else if fin && written == 0 && data.is_empty() {
            self.pending_writes.insert(
                stream_id,
                PendingWrite {
                    data: Vec::new(),
                    fin: true,
                },
            );
            reactor_metrics::record_raw_quic_client_pending_writes(self.pending_writes.len());
        }
    }

    fn close_stream(&mut self, stream_id: u64, error_code: u32) {
        let _ = self.conn.stream_close(stream_id, u64::from(error_code));
    }

    fn send_datagram(&mut self, data: &[u8]) -> bool {
        self.conn.send_datagram(data).is_ok()
    }

    fn metrics_snapshot(&self) -> JsSessionMetrics {
        snapshot_quic_metrics(&self.conn)
    }

    fn ping(&mut self) -> bool {
        self.conn.quiche_conn.send_ack_eliciting().is_ok()
    }

    fn qlog_path(&self) -> Option<String> {
        self.conn.qlog_path.clone()
    }

    fn emit_session_close(
        &mut self,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
        cause: RawQuicClientCloseCause,
    ) {
        if self.session_closed_emitted {
            return;
        }
        reactor_metrics::record_raw_quic_client_close_cause(cause);
        reactor_metrics::record_session_close(SessionKind::RawQuicClient);
        batch.push(JsH3Event::session_close(conn_handle));
        self.session_closed_emitted = true;
    }

    fn process_packet_for_handle(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
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
            batch.push(JsH3Event::handshake_complete(conn_handle));
        }
        self.conn.poll_quic_events(conn_handle, batch);
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
        self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);

        if self.conn.is_closed() && !self.session_closed_emitted {
            self.emit_session_close(batch, conn_handle, RawQuicClientCloseCause::Packet);
        }
    }

    fn process_timers_for_handle(
        &mut self,
        now: Instant,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) {
        if self.timer_deadline.is_some_and(|deadline| deadline <= now) {
            self.conn.on_timeout();
            if self.conn.is_closed() && !self.session_closed_emitted {
                self.emit_session_close(batch, conn_handle, RawQuicClientCloseCause::Timeout);
            } else {
                self.conn.poll_quic_events(conn_handle, batch);
                if let Some(ticket) = self.conn.update_session_ticket() {
                    batch.push(JsH3Event::session_ticket(conn_handle, ticket));
                }
                self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);
            }
        }
    }

    fn try_send_next(&mut self) -> Option<TxDatagram> {
        Self::try_send_next_with_pool_parts(
            &mut self.conn,
            self.send_buf.as_mut_slice(),
            &mut self.tx_pool,
        )
    }

    fn try_send_next_with_pool_parts(
        conn: &mut QuicConnection,
        _send_buf: &mut [u8],
        tx_pool: &mut BufferPool,
    ) -> Option<TxDatagram> {
        // Write directly into pool buffer — no intermediate copy.
        let mut tx_buf = tx_pool.checkout();
        let Ok((len, send_info)) = conn.send(tx_buf.as_mut_slice()) else {
            tx_pool.checkin(tx_buf);
            return None;
        };
        tx_buf.truncate(len);
        Some(TxDatagram {
            data: tx_buf,
            to: send_info.to,
        })
    }

    fn flush_pending_writes_for_handle(
        &mut self,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) {
        let flushed = flush_quic_client_pending_writes(&mut self.conn, &mut self.pending_writes);
        for stream_id in flushed {
            reactor_metrics::record_raw_quic_drain_event();
            batch.push(JsH3Event::drain(conn_handle, stream_id));
        }
    }

    fn poll_drain_events_for_handle(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        if !self.conn.blocked_set.is_empty() {
            self.conn.poll_drain_events(conn_handle, batch);
        }
    }

    fn recycle_tx_buffers_into_pool(&mut self, buffers: Vec<Vec<u8>>) {
        reactor_metrics::record_tx_buffers_recycled(buffers.len());
        for buf in buffers {
            self.tx_pool.checkin(buf);
        }
    }

    fn is_reapable(&self) -> bool {
        self.session_closed_emitted && self.pending_writes.is_empty()
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
            QuicClientCommand::Shutdown => {
                if !self.session_closed_emitted {
                    reactor_metrics::record_raw_quic_client_close_cause(
                        RawQuicClientCloseCause::Shutdown,
                    );
                    reactor_metrics::record_session_close(SessionKind::RawQuicClient);
                    self.session_closed_emitted = true;
                }
                return true;
            }
            QuicClientCommand::Close { error_code, reason } => {
                self.close_session(error_code, &reason);
            }
            QuicClientCommand::StreamSend {
                stream_id,
                data,
                fin,
            } => {
                self.queue_stream_send(stream_id, data, fin);
            }
            QuicClientCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                self.close_stream(stream_id, error_code);
            }
            QuicClientCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.send_datagram(&data));
            }
            QuicClientCommand::GetSessionMetrics { resp_tx } => {
                let _ = resp_tx.send(Some(self.metrics_snapshot()));
            }
            QuicClientCommand::Ping { resp_tx } => {
                let _ = resp_tx.send(self.ping());
            }
            QuicClientCommand::GetQlogPath { resp_tx } => {
                let _ = resp_tx.send(self.qlog_path());
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
        self.process_packet_for_handle(buf, peer, local, batch, 0);
    }

    fn process_timers(&mut self, now: Instant, batch: &mut Vec<JsH3Event>) {
        self.process_timers_for_handle(now, batch, 0);
    }

    fn flush_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
        while let Some(packet) = self.try_send_next() {
            outbound.push(packet);
        }
        self.refresh_timeout_deadline();
    }

    fn flush_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        self.flush_pending_writes_for_handle(batch, 0);
    }

    fn poll_drain_events(&mut self, batch: &mut Vec<JsH3Event>) {
        self.poll_drain_events_for_handle(batch, 0);
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        self.recycle_tx_buffers_into_pool(buffers);
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
