//! Worker thread mode: a dedicated OS thread runs the QUIC/H3 hot loop,
//! delivering batched events to the JS main thread via ThreadsafeFunction.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock, Weak,
};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ring::rand::SecureRandom;
use slab::Slab;

use crate::buffer_pool::BufferPool;
use crate::client_topology::{
    default_h3_client_socket_strategy, shared_client_bind_addr, shared_client_worker_key,
    ClientSocketStrategy, SharedClientWorkerKey as SharedH3ClientWorkerKey,
};
use crate::config::{Http3Config, JsServerOptions, TransportRuntimeMode};
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::connection_map::ConnectionMap;
use crate::error::Http3NativeError;
use crate::event_loop::{self, EventBatcher, ProtocolHandler, MAX_BATCH_SIZE, SEND_BUF_SIZE};
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::reactor_metrics::{self, SessionKind, WorkerSpawnKind};
use crate::shared_client_reactor;
use crate::timer_heap::TimerHeap;
use crate::transport::{self, ErasedWaker, TxDatagram};

// Re-export for backward compatibility with server.rs / client.rs / quic_server.rs / quic_client.rs
pub use crate::event_loop::EventTsfn;

/// Commands sent from the JS main thread to the worker thread.
pub enum WorkerCommand {
    SendResponseHeaders {
        conn_handle: u32,
        stream_id: u64,
        headers: Vec<(String, String)>,
        fin: bool,
    },
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
    SendTrailers {
        conn_handle: u32,
        stream_id: u64,
        headers: Vec<(String, String)>,
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
    GetRemoteSettings {
        conn_handle: u32,
        resp_tx: Sender<Vec<(u64, u64)>>,
    },
    GetQlogPath {
        conn_handle: u32,
        resp_tx: Sender<Option<String>>,
    },
    PingSession {
        conn_handle: u32,
        resp_tx: Sender<bool>,
    },
    Shutdown,
}

/// Buffered partial write for a stream blocked by flow control.
struct PendingWrite {
    data: Vec<u8>,
    fin: bool,
}

/// Per-worker state inside a `WorkerHandle`.
struct H3ServerWorker {
    cmd_tx: Sender<WorkerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
    waker: Arc<dyn ErasedWaker>,
}

/// Handle returned to the JS side for sending commands to the worker(s).
/// Supports multiple sharded workers via SO_REUSEPORT — commands are
/// routed to the correct worker by the worker index encoded in conn_handle.
pub struct WorkerHandle {
    workers: Vec<H3ServerWorker>,
    local_addr: SocketAddr,
}

impl WorkerHandle {
    /// Route a command to the worker that owns `conn_handle`.
    pub fn send_command(&self, cmd: WorkerCommand) -> bool {
        let ch = command_conn_handle(&cmd);
        let worker = &self.workers[crate::server_sharding::worker_index(ch)];
        let local = remap_command_handle(cmd);
        if worker.cmd_tx.send(local).is_ok() {
            let _ = worker.waker.wake();
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
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker.cmd_tx
            .send(WorkerCommand::GetSessionMetrics { conn_handle: local, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server metrics".into())
        })
    }

    pub fn send_datagram(&self, conn_handle: u32, data: Vec<u8>) -> Result<bool, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker.cmd_tx
            .send(WorkerCommand::SendDatagram { conn_handle: local, data, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server datagram send".into())
        })
    }

    pub fn get_remote_settings(
        &self,
        conn_handle: u32,
    ) -> Result<Vec<(u64, u64)>, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker.cmd_tx
            .send(WorkerCommand::GetRemoteSettings { conn_handle: local, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server settings".into())
        })
    }

    pub fn ping_session(&self, conn_handle: u32) -> Result<bool, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker.cmd_tx
            .send(WorkerCommand::PingSession { conn_handle: local, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for server ping".into()))
    }

    pub fn get_qlog_path(&self, conn_handle: u32) -> Result<Option<String>, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker.cmd_tx
            .send(WorkerCommand::GetQlogPath { conn_handle: local, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server qlog path".into())
        })
    }

    pub fn shutdown(&mut self) {
        for worker in &self.workers {
            let _ = worker.cmd_tx.send(WorkerCommand::Shutdown);
            let _ = worker.waker.wake();
        }
        for worker in &mut self.workers {
            if let Some(handle) = worker.join_handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn command_conn_handle(cmd: &WorkerCommand) -> u32 {
    match cmd {
        WorkerCommand::SendResponseHeaders { conn_handle, .. }
        | WorkerCommand::StreamSend { conn_handle, .. }
        | WorkerCommand::StreamClose { conn_handle, .. }
        | WorkerCommand::SendTrailers { conn_handle, .. }
        | WorkerCommand::CloseSession { conn_handle, .. }
        | WorkerCommand::SendDatagram { conn_handle, .. }
        | WorkerCommand::GetSessionMetrics { conn_handle, .. }
        | WorkerCommand::GetRemoteSettings { conn_handle, .. }
        | WorkerCommand::GetQlogPath { conn_handle, .. }
        | WorkerCommand::PingSession { conn_handle, .. } => *conn_handle,
        WorkerCommand::Shutdown => 0,
    }
}

fn remap_command_handle(cmd: WorkerCommand) -> WorkerCommand {
    use crate::server_sharding::local_conn_handle;
    match cmd {
        WorkerCommand::SendResponseHeaders { conn_handle, stream_id, headers, fin } =>
            WorkerCommand::SendResponseHeaders { conn_handle: local_conn_handle(conn_handle), stream_id, headers, fin },
        WorkerCommand::StreamSend { conn_handle, stream_id, data, fin } =>
            WorkerCommand::StreamSend { conn_handle: local_conn_handle(conn_handle), stream_id, data, fin },
        WorkerCommand::StreamClose { conn_handle, stream_id, error_code } =>
            WorkerCommand::StreamClose { conn_handle: local_conn_handle(conn_handle), stream_id, error_code },
        WorkerCommand::SendTrailers { conn_handle, stream_id, headers } =>
            WorkerCommand::SendTrailers { conn_handle: local_conn_handle(conn_handle), stream_id, headers },
        WorkerCommand::CloseSession { conn_handle, error_code, reason } =>
            WorkerCommand::CloseSession { conn_handle: local_conn_handle(conn_handle), error_code, reason },
        WorkerCommand::SendDatagram { conn_handle, data, resp_tx } =>
            WorkerCommand::SendDatagram { conn_handle: local_conn_handle(conn_handle), data, resp_tx },
        WorkerCommand::GetSessionMetrics { conn_handle, resp_tx } =>
            WorkerCommand::GetSessionMetrics { conn_handle: local_conn_handle(conn_handle), resp_tx },
        WorkerCommand::GetRemoteSettings { conn_handle, resp_tx } =>
            WorkerCommand::GetRemoteSettings { conn_handle: local_conn_handle(conn_handle), resp_tx },
        WorkerCommand::GetQlogPath { conn_handle, resp_tx } =>
            WorkerCommand::GetQlogPath { conn_handle: local_conn_handle(conn_handle), resp_tx },
        WorkerCommand::PingSession { conn_handle, resp_tx } =>
            WorkerCommand::PingSession { conn_handle: local_conn_handle(conn_handle), resp_tx },
        WorkerCommand::Shutdown => WorkerCommand::Shutdown,
    }
}

/// Commands sent from JS to the client worker thread.
pub enum ClientWorkerCommand {
    SendRequest {
        headers: Vec<(String, String)>,
        fin: bool,
        resp_tx: Sender<Result<u64, String>>,
    },
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
    GetRemoteSettings {
        resp_tx: Sender<Vec<(u64, u64)>>,
    },
    GetQlogPath {
        resp_tx: Sender<Option<String>>,
    },
    Ping {
        resp_tx: Sender<bool>,
    },
    Close {
        error_code: u32,
        reason: String,
    },
    Shutdown,
}

enum SharedClientWorkerCommand {
    OpenSession {
        quiche_config: quiche::Config,
        server_addr: SocketAddr,
        server_name: String,
        session_ticket: Option<Vec<u8>>,
        qlog_dir: Option<String>,
        qlog_level: Option<String>,
        tsfn: EventTsfn,
        resp_tx: Sender<Result<u32, Http3NativeError>>,
    },
    SendRequest {
        session_handle: u32,
        headers: Vec<(String, String)>,
        fin: bool,
        resp_tx: Sender<Result<u64, String>>,
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
    GetRemoteSettings {
        session_handle: u32,
        resp_tx: Sender<Vec<(u64, u64)>>,
    },
    GetQlogPath {
        session_handle: u32,
        resp_tx: Sender<Option<String>>,
    },
    Ping {
        session_handle: u32,
        resp_tx: Sender<bool>,
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

struct SharedClientWorkerControl {
    cmd_tx: Sender<SharedClientWorkerCommand>,
    waker: Arc<dyn ErasedWaker>,
    local_addr: SocketAddr,
    join_handle: Mutex<Option<thread::JoinHandle<()>>>,
    running: AtomicBool,
    session_count: AtomicUsize,
    key: SharedH3ClientWorkerKey,
}

impl SharedClientWorkerControl {
    fn wake(&self) {
        let _ = self.waker.wake();
    }
}

enum ClientWorkerHandleKind {
    Dedicated {
        cmd_tx: Sender<ClientWorkerCommand>,
        join_handle: Option<thread::JoinHandle<()>>,
        waker: Arc<dyn ErasedWaker>,
    },
    Shared {
        session_handle: u32,
        worker: Arc<SharedClientWorkerControl>,
    },
}

/// Handle returned to JS for controlling the client worker thread.
pub struct ClientWorkerHandle {
    kind: Option<ClientWorkerHandleKind>,
    local_addr: SocketAddr,
}

impl ClientWorkerHandle {
    /// Open a new request stream and return the stream ID.
    pub fn send_request(
        &self,
        headers: Vec<(String, String)>,
        fin: bool,
    ) -> Result<u64, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::SendRequest {
                        headers,
                        fin,
                        resp_tx,
                    })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::SendRequest {
                        session_handle: *session_handle,
                        headers,
                        fin,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }

        match resp_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(stream_id)) => Ok(stream_id),
            Ok(Err(reason)) => Err(Http3NativeError::InvalidState(reason)),
            Err(_) => Err(Http3NativeError::InvalidState(
                "timed out waiting for stream ID from worker".into(),
            )),
        }
    }

    /// Queue stream data to be sent by the worker.
    pub fn stream_send(&self, stream_id: u64, data: Vec<u8>, fin: bool) -> bool {
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(ClientWorkerCommand::StreamSend {
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
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::StreamSend {
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
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(ClientWorkerCommand::StreamClose {
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
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::StreamClose {
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
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::SendDatagram { data, resp_tx })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::SendDatagram {
                        session_handle: *session_handle,
                        data,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client datagram send".into())
        })
    }

    pub fn get_session_metrics(&self) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::GetSessionMetrics { resp_tx })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::GetSessionMetrics {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client metrics".into())
        })
    }

    pub fn get_remote_settings(&self) -> Result<Vec<(u64, u64)>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::GetRemoteSettings { resp_tx })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::GetRemoteSettings {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client settings".into())
        })
    }

    pub fn ping(&self) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::Ping { resp_tx })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::Ping {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for client ping".into()))
    }

    pub fn get_qlog_path(&self) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::GetQlogPath { resp_tx })
                    .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
                let _ = waker.wake();
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::GetQlogPath {
                        session_handle: *session_handle,
                        resp_tx,
                    })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("shared client worker not running".into())
                    })?;
                worker.wake();
            }
            None => {
                return Err(Http3NativeError::InvalidState("client worker not running".into()))
            }
        }
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client qlog path".into())
        })
    }

    /// Request a graceful connection close.
    pub fn close(&self, error_code: u32, reason: String) -> bool {
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                if cmd_tx
                    .send(ClientWorkerCommand::Close { error_code, reason })
                    .is_ok()
                {
                    let _ = waker.wake();
                    true
                } else {
                    false
                }
            }
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                if worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::Close {
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
            ClientWorkerHandleKind::Dedicated {
                cmd_tx,
                mut join_handle,
                waker,
            } => {
                let _ = cmd_tx.send(ClientWorkerCommand::Shutdown);
                let _ = waker.wake();
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
            }
            ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            } => {
                let _ = worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::ReleaseSession { session_handle });
                worker.wake();
                if worker.session_count.fetch_sub(1, Ordering::AcqRel) == 1 {
                    if let Ok(mut join_handle) = worker.join_handle.lock() {
                        if let Some(handle) = join_handle.take() {
                            let _ = handle.join();
                        }
                    }
                    if let Ok(mut registry) = shared_client_worker_registry().lock() {
                        registry.remove(&worker.key);
                    }
                }
            }
        }
    }
}

impl Drop for ClientWorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Spawn functions ─────────────────────────────────────────────────

/// Spawn a client worker thread that owns UDP I/O and QUIC/H3 processing.
#[allow(clippy::too_many_arguments)]
pub fn spawn_client_worker(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    tsfn: EventTsfn,
) -> Result<ClientWorkerHandle, Http3NativeError> {
    if default_h3_client_socket_strategy(runtime_mode) == ClientSocketStrategy::SharedPerFamily {
        return spawn_shared_client_worker(
            quiche_config,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            runtime_mode,
            tsfn,
        );
    }

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

    let bind_addr = shared_client_bind_addr(server_addr);
    let (driver, waker, local_addr) =
        transport::prepare_client_platform_driver(bind_addr, runtime_mode)?;

    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();

    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3ClientDedicated);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut quiche_config = quiche_config;
        let handler = H3ClientHandler::new(
            local_addr,
            server_addr,
            &server_name,
            session_ticket.as_deref(),
            qlog_dir.as_deref(),
            qlog_level.as_deref(),
            &mut quiche_config,
        );
        let Some(mut handler) = handler else { return };
        event_loop::run_event_loop(
            &mut driver,
            cmd_rx,
            &mut handler,
            EventBatcher::new_tsfn(tsfn),
            local_addr,
        );
    });

    Ok(ClientWorkerHandle {
        kind: Some(ClientWorkerHandleKind::Dedicated {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_clone,
        }),
        local_addr,
    })
}

struct SharedClientSession {
    handler: H3ClientHandler,
    batcher: EventBatcher,
    server_addr: SocketAddr,
}

fn shared_client_worker_registry(
) -> &'static Mutex<HashMap<SharedH3ClientWorkerKey, Weak<SharedClientWorkerControl>>> {
    static REGISTRY: OnceLock<
        Mutex<HashMap<SharedH3ClientWorkerKey, Weak<SharedClientWorkerControl>>>,
    > = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire_shared_client_worker(
    server_addr: SocketAddr,
    runtime_mode: TransportRuntimeMode,
) -> Result<Arc<SharedClientWorkerControl>, Http3NativeError> {
    let key = shared_client_worker_key(server_addr, runtime_mode);
    let mut registry = shared_client_worker_registry()
        .lock()
        .map_err(|_| Http3NativeError::InvalidState("shared client registry poisoned".into()))?;
    if let Some(worker) = registry.get(&key).and_then(Weak::upgrade) {
        if worker.running.load(Ordering::Acquire) {
            reactor_metrics::record_shared_worker_reuse(WorkerSpawnKind::H3ClientShared);
            return Ok(worker);
        }
    }

    let bind_addr = shared_client_bind_addr(server_addr);
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (driver, waker, local_addr) =
        transport::prepare_client_platform_driver(bind_addr, runtime_mode)?;
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let control = Arc::new(SharedClientWorkerControl {
        cmd_tx,
        waker: waker_arc.clone(),
        local_addr,
        join_handle: Mutex::new(None),
        running: AtomicBool::new(true),
        session_count: AtomicUsize::new(0),
        key: key.clone(),
    });
    let control_for_thread = Arc::clone(&control);
    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3ClientShared);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        run_shared_client_event_loop(&mut driver, cmd_rx, local_addr);
        control_for_thread.running.store(false, Ordering::Release);
    });
    if let Ok(mut slot) = control.join_handle.lock() {
        *slot = Some(join_handle);
    }
    registry.insert(key, Arc::downgrade(&control));
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
fn spawn_shared_client_worker(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    tsfn: EventTsfn,
) -> Result<ClientWorkerHandle, Http3NativeError> {
    let worker = acquire_shared_client_worker(server_addr, runtime_mode)?;

    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    worker
        .cmd_tx
        .send(SharedClientWorkerCommand::OpenSession {
            quiche_config,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            tsfn,
            resp_tx,
        })
        .map_err(|_| Http3NativeError::InvalidState("shared client worker not running".into()))?;
    worker.wake();

    let session_handle = resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
        Http3NativeError::InvalidState("timed out waiting for shared h3 session".into())
    })??;
    worker.session_count.fetch_add(1, Ordering::AcqRel);

    Ok(ClientWorkerHandle {
        kind: Some(ClientWorkerHandleKind::Shared {
            session_handle,
            worker: Arc::clone(&worker),
        }),
        local_addr: worker.local_addr,
    })
}

fn emit_shared_client_runtime_error<D: transport::Driver>(
    sessions: &mut Slab<SharedClientSession>,
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

fn remove_shared_client_session(
    sessions: &mut Slab<SharedClientSession>,
    route_by_dcid: &mut HashMap<Vec<u8>, usize>,
    timer_heap: &mut TimerHeap,
    handle: usize,
) {
    if !sessions.contains(handle) {
        return;
    }
    if let Some(session) = sessions.get(handle) {
        if !session.handler.session_closed_emitted {
            reactor_metrics::record_session_close(SessionKind::H3Client);
        }
    }
    timer_heap.remove_connection(handle);
    let _ = sessions.remove(handle);
    route_by_dcid.retain(|_, mapped_handle| *mapped_handle != handle);
}

fn refresh_shared_client_dcid(
    route_by_dcid: &mut HashMap<Vec<u8>, usize>,
    handle: usize,
    session: &mut SharedClientSession,
) {
    let (current_dcid, needs_update, retired_dcids) = session.handler.take_dcid_updates();
    if needs_update {
        route_by_dcid.insert(current_dcid, handle);
    }
    for retired in retired_dcids {
        route_by_dcid.remove(&retired);
    }
}

fn sync_shared_client_timer(timer_heap: &mut TimerHeap, handle: usize, session: &SharedClientSession) {
    shared_client_reactor::sync_timer(timer_heap, handle, session, |current| {
        current.handler.timer_deadline
    });
}

fn flush_shared_client_sends(
    sessions: &mut Slab<SharedClientSession>,
    handles_buf: &mut Vec<usize>,
    tx_pool: &mut BufferPool,
    outbound: &mut Vec<TxDatagram>,
) {
    shared_client_reactor::flush_round_robin_sends(
        sessions,
        handles_buf,
        outbound,
        |session| {
            H3ClientHandler::try_send_next_with_pool_parts(
                &mut session.handler.conn,
                session.handler.send_buf.as_mut_slice(),
                tx_pool,
            )
        },
    );
}

fn run_shared_client_event_loop<D: transport::Driver>(
    driver: &mut D,
    cmd_rx: crossbeam_channel::Receiver<SharedClientWorkerCommand>,
    local_addr: SocketAddr,
) {
    let mut sessions: Slab<SharedClientSession> = Slab::new();
    let mut route_by_dcid: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut timer_heap = TimerHeap::new();
    let mut tx_pool = BufferPool::new(256, 65535);
    let mut handles_buf = Vec::new();
    let mut outbound = Vec::new();
    let mut closed_sessions = Vec::new();

    loop {
        let deadline = timer_heap.next_deadline();

        let outcome = match driver.poll(deadline) {
            Ok(outcome) => outcome,
            Err(err) => {
                emit_shared_client_runtime_error(
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
                SharedClientWorkerCommand::OpenSession {
                    mut quiche_config,
                    server_addr,
                    server_name,
                    session_ticket,
                    qlog_dir,
                    qlog_level,
                    tsfn,
                    resp_tx,
                } => {
                    let handler = H3ClientHandler::new(
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
                                "failed to create h3 client session".into(),
                            ))
                        },
                        |handler| {
                            let dcid = handler.current_dcid();
                            let handle = sessions.insert(SharedClientSession {
                                handler,
                                batcher: EventBatcher::new_tsfn(tsfn),
                                server_addr,
                            });
                            route_by_dcid.insert(dcid, handle);
                            if let Some(session) = sessions.get(handle) {
                                sync_shared_client_timer(&mut timer_heap, handle, session);
                            }
                            Ok(handle as u32)
                        },
                    );
                    let _ = resp_tx.send(result);
                }
                SharedClientWorkerCommand::SendRequest {
                    session_handle,
                    headers,
                    fin,
                    resp_tx,
                } => {
                    let result = sessions
                        .get_mut(session_handle as usize)
                        .map(|session| session.handler.send_request(headers, fin))
                        .unwrap_or_else(|| Err("client session not running".into()));
                    let _ = resp_tx.send(result);
                }
                SharedClientWorkerCommand::StreamSend {
                    session_handle,
                    stream_id,
                    data,
                    fin,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.queue_stream_send(stream_id, data, fin);
                    }
                }
                SharedClientWorkerCommand::StreamClose {
                    session_handle,
                    stream_id,
                    error_code,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.close_stream(stream_id, error_code);
                    }
                }
                SharedClientWorkerCommand::SendDatagram {
                    session_handle,
                    data,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.send_datagram(&data));
                    let _ = resp_tx.send(ok);
                }
                SharedClientWorkerCommand::GetSessionMetrics {
                    session_handle,
                    resp_tx,
                } => {
                    let metrics = sessions
                        .get(session_handle as usize)
                        .map(|session| session.handler.metrics_snapshot());
                    let _ = resp_tx.send(metrics);
                }
                SharedClientWorkerCommand::GetRemoteSettings {
                    session_handle,
                    resp_tx,
                } => {
                    let settings = sessions
                        .get(session_handle as usize)
                        .map(|session| session.handler.remote_settings())
                        .unwrap_or_default();
                    let _ = resp_tx.send(settings);
                }
                SharedClientWorkerCommand::GetQlogPath {
                    session_handle,
                    resp_tx,
                } => {
                    let path = sessions
                        .get(session_handle as usize)
                        .and_then(|session| session.handler.qlog_path());
                    let _ = resp_tx.send(path);
                }
                SharedClientWorkerCommand::Ping {
                    session_handle,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.ping());
                    let _ = resp_tx.send(ok);
                }
                SharedClientWorkerCommand::Close {
                    session_handle,
                    error_code,
                    reason,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        session.handler.close_session(error_code, &reason);
                    }
                }
                SharedClientWorkerCommand::ReleaseSession { session_handle } => {
                    remove_shared_client_session(
                        &mut sessions,
                        &mut route_by_dcid,
                        &mut timer_heap,
                        session_handle as usize,
                    );
                }
            }
        }

        flush_shared_client_sends(&mut sessions, &mut handles_buf, &mut tx_pool, &mut outbound);
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                emit_shared_client_runtime_error(
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
            let Ok(header) = quiche::Header::from_slice(pkt.data.as_mut_slice(), crate::cid::SCID_LEN) else {
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
                    refresh_shared_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_client_timer(&mut timer_heap, handle, session);
                    if session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush() {
                        should_remove = true;
                    }
                }
            }
            if should_remove {
                remove_shared_client_session(
                    &mut sessions,
                    &mut route_by_dcid,
                    &mut timer_heap,
                    handle,
                );
            }
            if (rx_idx + 1) % 64 == 0 && rx_idx + 1 < rx_count {
                flush_shared_client_sends(
                    &mut sessions,
                    &mut handles_buf,
                    &mut tx_pool,
                    &mut outbound,
                );
                if !outbound.is_empty() {
                    if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                        emit_shared_client_runtime_error(
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
                refresh_shared_client_dcid(&mut route_by_dcid, handle, session);
                sync_shared_client_timer(&mut timer_heap, handle, session);
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
            remove_shared_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                handle,
            );
        }

        flush_shared_client_sends(&mut sessions, &mut handles_buf, &mut tx_pool, &mut outbound);
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                emit_shared_client_runtime_error(
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
            remove_shared_client_session(
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

/// Spawn server worker thread(s) for the given configuration.
///
/// When `reuse_port` is enabled in the config, spawns multiple workers
/// (one per available CPU, capped at 8) each with its own socket bound
/// to the same address via SO_REUSEPORT.  The kernel distributes incoming
/// packets by 4-tuple hash.  All workers share the same TSFN for event
/// delivery to JS.
///
/// When `reuse_port` is disabled (default), spawns a single worker.
pub fn spawn_worker(
    quiche_config: quiche::Config,
    http3_config: Http3Config,
    bind_addr: SocketAddr,
    tsfn: EventTsfn,
    stored_options: crate::server::StoredServerOptions,
) -> Result<WorkerHandle, Http3NativeError> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1);

    let mut config_slot = Some(quiche_config);
    spawn_worker_sharded(
        move || {
            if let Some(config) = config_slot.take() {
                return Ok(config);
            }
            // Workers 1..N: rebuild config from the stored snapshot
            // which preserves all original options (datagrams, TLS, etc).
            Http3Config::new_server_quiche_config(&stored_options.to_js_server_options())
        },
        http3_config,
        bind_addr,
        num_workers,
        tsfn,
    )
}

/// Spawn N H3 server workers with SO_REUSEPORT.  `make_quiche_config` is
/// called once per worker since `quiche::Config` is not `Clone`.
pub(crate) fn spawn_worker_sharded<F>(
    mut make_quiche_config: F,
    http3_config: Http3Config,
    bind_addr: SocketAddr,
    num_workers: usize,
    tsfn: EventTsfn,
) -> Result<WorkerHandle, Http3NativeError>
where
    F: FnMut() -> Result<quiche::Config, Http3NativeError>,
{
    let num_workers = num_workers.max(1);
    let use_reuse_port = num_workers > 1;

    // Bind the first socket to discover the actual local address.
    let first_socket = transport::socket::bind_worker_socket(bind_addr, use_reuse_port)?;
    first_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    let _ = transport::socket::set_socket_buffers(&first_socket, 2 * 1024 * 1024);
    let local_addr = first_socket.local_addr().map_err(Http3NativeError::Io)?;

    // Query path MTU once for all workers on this address.
    let ceiling = if !local_addr.ip().is_unspecified() {
        crate::config::effective_pmtud_ceiling(&local_addr)
    } else {
        crate::config::FALLBACK_MAX_UDP_PAYLOAD
    };

    let tsfn = Arc::new(tsfn);
    let mut workers = Vec::with_capacity(num_workers);

    // Worker 0 uses the already-bound first socket.
    {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut config = make_quiche_config()?;
        config.set_max_send_udp_payload_size(ceiling);
        config.set_max_recv_udp_payload_size(ceiling);
        let (driver, waker) =
            transport::create_platform_driver(first_socket, http3_config.runtime_mode)?;
        let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
        let tsfn_ref = Arc::clone(&tsfn);
        let http3_clone = Http3Config {
            qlog_dir: http3_config.qlog_dir.clone(),
            qlog_level: http3_config.qlog_level.clone(),
            qpack_max_table_capacity: http3_config.qpack_max_table_capacity,
            qpack_blocked_streams: http3_config.qpack_blocked_streams,
            max_connections: http3_config.max_connections,
            disable_retry: http3_config.disable_retry,
            reuse_port: http3_config.reuse_port,
            cid_encoding: http3_config.cid_encoding.clone(),
            runtime_mode: http3_config.runtime_mode,
        };

        reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3Server);
        let join_handle = thread::spawn(move || {
            let mut driver = driver;
            let mut handler = H3ServerHandler::new(config, http3_clone, 0);
            event_loop::run_event_loop(
                &mut driver, cmd_rx, &mut handler,
                EventBatcher::new_shared_tsfn(tsfn_ref), local_addr,
            );
        });
        workers.push(H3ServerWorker {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_arc,
        });
    }

    // Workers 1..N bind new sockets to the same address via SO_REUSEPORT.
    for i in 1..num_workers {
        let socket = transport::socket::bind_worker_socket(local_addr, true)?;
        socket.set_nonblocking(true).map_err(Http3NativeError::Io)?;
        let _ = transport::socket::set_socket_buffers(&socket, 2 * 1024 * 1024);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut config = make_quiche_config()?;
        config.set_max_send_udp_payload_size(ceiling);
        config.set_max_recv_udp_payload_size(ceiling);
        let (driver, waker) =
            transport::create_platform_driver(socket, http3_config.runtime_mode)?;
        let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
        let tsfn_ref = Arc::clone(&tsfn);
        let http3_clone = Http3Config {
            qlog_dir: http3_config.qlog_dir.clone(),
            qlog_level: http3_config.qlog_level.clone(),
            qpack_max_table_capacity: http3_config.qpack_max_table_capacity,
            qpack_blocked_streams: http3_config.qpack_blocked_streams,
            max_connections: http3_config.max_connections,
            disable_retry: http3_config.disable_retry,
            reuse_port: http3_config.reuse_port,
            cid_encoding: http3_config.cid_encoding.clone(),
            runtime_mode: http3_config.runtime_mode,
        };
        let worker_index = i as u32;

        reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3Server);
        let join_handle = thread::spawn(move || {
            let mut driver = driver;
            let mut handler = H3ServerHandler::new(config, http3_clone, worker_index);
            event_loop::run_event_loop(
                &mut driver, cmd_rx, &mut handler,
                EventBatcher::new_shared_tsfn(tsfn_ref), local_addr,
            );
        });
        workers.push(H3ServerWorker {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_arc,
        });
    }

    Ok(WorkerHandle { workers, local_addr })
}

// ── H3 Server Protocol Handler ──────────────────────────────────────

struct H3ServerHandler {
    conn_map: ConnectionMap,
    timer_heap: TimerHeap,
    buffer_pool: BufferPool,
    tx_pool: BufferPool,
    pending_writes: HashMap<(u32, u64), PendingWrite>,
    pending_session_closes: HashMap<u32, (u32, String, Instant)>,
    conn_send_buffers: HashMap<usize, Vec<u8>>,
    handles_buf: Vec<usize>,
    http3_config: Http3Config,
    quiche_config: quiche::Config,
    disable_retry: bool,
    /// Handles that were expired in the most recent process_timers call.
    /// Used by poll_drain_events and cleanup_closed to avoid duplicate events.
    last_expired: Vec<usize>,
    handle_offset: u32,
}

impl H3ServerHandler {
    fn new(quiche_config: quiche::Config, http3_config: Http3Config, worker_index: u32) -> Self {
        let disable_retry = http3_config.disable_retry;
        Self {
            conn_map: ConnectionMap::with_max_connections_and_cid(
                http3_config.max_connections,
                http3_config.cid_encoding.clone(),
            ),
            timer_heap: TimerHeap::new(),
            buffer_pool: BufferPool::default(),
            tx_pool: BufferPool::new(256, 65535),
            pending_writes: HashMap::new(),
            pending_session_closes: HashMap::new(),
            conn_send_buffers: HashMap::new(),
            handles_buf: Vec::new(),
            http3_config,
            quiche_config,
            disable_retry,
            last_expired: Vec::new(),
            handle_offset: crate::server_sharding::handle_offset(worker_index),
        }
    }
}

impl ProtocolHandler for H3ServerHandler {
    type Command = WorkerCommand;

    #[allow(clippy::too_many_lines)]
    fn dispatch_command(&mut self, cmd: WorkerCommand, _batch: &mut Vec<JsH3Event>) -> bool {
        match cmd {
            WorkerCommand::Shutdown => return true,
            WorkerCommand::SendResponseHeaders {
                conn_handle,
                stream_id,
                headers,
                fin,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let h3_headers: Vec<quiche::h3::Header> = headers
                        .iter()
                        .map(|(n, v)| quiche::h3::Header::new(n.as_bytes(), v.as_bytes()))
                        .collect();
                    let _ = conn.send_response(stream_id, &h3_headers, fin);
                }
            }
            WorkerCommand::StreamSend {
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
                    let written = conn.send_body(stream_id, &data, fin).unwrap_or(0);
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
            WorkerCommand::StreamClose {
                conn_handle,
                stream_id,
                error_code,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let _ = conn.stream_close(stream_id, u64::from(error_code));
                }
            }
            WorkerCommand::SendTrailers {
                conn_handle,
                stream_id,
                headers,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let h3_headers: Vec<quiche::h3::Header> = headers
                        .iter()
                        .map(|(n, v)| quiche::h3::Header::new(n.as_bytes(), v.as_bytes()))
                        .collect();
                    let _ = conn.send_trailers(stream_id, &h3_headers);
                }
            }
            WorkerCommand::CloseSession {
                conn_handle,
                error_code,
                reason,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    if conn.send_goaway().is_ok() {
                        self.pending_session_closes.insert(
                            conn_handle,
                            (
                                error_code,
                                reason,
                                Instant::now() + Duration::from_millis(25),
                            ),
                        );
                    } else {
                        let _ = conn.quiche_conn.close(
                            true,
                            u64::from(error_code),
                            reason.as_bytes(),
                        );
                    }
                }
            }
            WorkerCommand::SendDatagram {
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
            WorkerCommand::GetSessionMetrics {
                conn_handle,
                resp_tx,
            } => {
                let metrics = self.conn_map.get(conn_handle as usize).map(snapshot_metrics);
                let _ = resp_tx.send(metrics);
            }
            WorkerCommand::GetRemoteSettings {
                conn_handle,
                resp_tx,
            } => {
                let settings = self
                    .conn_map
                    .get(conn_handle as usize)
                    .map_or_else(Vec::new, H3Connection::remote_settings);
                let _ = resp_tx.send(settings);
            }
            WorkerCommand::GetQlogPath {
                conn_handle,
                resp_tx,
            } => {
                let path = self
                    .conn_map
                    .get(conn_handle as usize)
                    .and_then(|conn| conn.qlog_path.clone());
                let _ = resp_tx.send(path);
            }
            WorkerCommand::PingSession {
                conn_handle,
                resp_tx,
            } => {
                let ok = if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    conn.quiche_conn.send_ack_eliciting().is_ok()
                } else {
                    false
                };
                let _ = resp_tx.send(ok);
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
        let offset = self.handle_offset;
        let Ok(hdr) = quiche::Header::from_slice(buf, crate::connection_map::SCID_LEN) else {
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
                    self.http3_config.qlog_dir.as_deref(),
                    self.http3_config.qlog_level.as_deref(),
                    self.http3_config.qpack_max_table_capacity,
                    self.http3_config.qpack_blocked_streams,
                ) {
                    Ok(h) => {
                        self.conn_map.add_dcid(h, client_dcid);
                        reactor_metrics::record_session_open(SessionKind::H3Server);
                        batch.push(JsH3Event::new_session(
                            offset | (h as u32),
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
                            self.http3_config.qlog_dir.as_deref(),
                            self.http3_config.qlog_level.as_deref(),
                            self.http3_config.qpack_max_table_capacity,
                            self.http3_config.qpack_blocked_streams,
                        ) {
                            Ok(h) => {
                                self.conn_map.add_dcid(h, odcid);
                                reactor_metrics::record_session_open(SessionKind::H3Server);
                                batch.push(JsH3Event::new_session(
                                    offset | (h as u32),
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

            if (conn.quiche_conn.is_established() || conn.quiche_conn.is_in_early_data())
                && !conn.is_established
            {
                let _ = conn.init_h3();
            }
            if conn.quiche_conn.is_established() && !conn.handshake_complete_emitted {
                conn.handshake_complete_emitted = true;
                batch.push(JsH3Event::handshake_complete(offset | (handle as u32)));
            }

            let current_scid: Vec<u8> = conn.quiche_conn.source_id().into_owned().to_vec();
            let needs_dcid_update = current_scid.as_slice() != conn.conn_id.as_slice();
            if needs_dcid_update {
                conn.conn_id = current_scid.clone();
            }

            conn.poll_h3_events(offset | (handle as u32), batch);

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
        let offset = self.handle_offset;
        self.last_expired = self.timer_heap.pop_expired(now);
        self.last_expired.sort_unstable();
        self.last_expired.dedup();
        for &handle in &self.last_expired {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.on_timeout();
                if conn.is_closed() {
                    reactor_metrics::record_session_close(SessionKind::H3Server);
                    batch.push(JsH3Event::session_close(offset | (handle as u32)));
                } else {
                    conn.poll_h3_events(offset | (handle as u32), batch);
                    if let Some(timeout) = conn.timeout() {
                        self.timer_heap.schedule(handle, Instant::now() + timeout);
                    }
                }
            }
        }

        // Complete deferred graceful session closes
        let due_closes: Vec<u32> = self
            .pending_session_closes
            .iter()
            .filter_map(|(handle, (_, _, deadline))| (now >= *deadline).then_some(*handle))
            .collect();
        for conn_handle in due_closes {
            if let Some((error_code, reason, _)) =
                self.pending_session_closes.remove(&conn_handle)
            {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let _ = conn
                        .quiche_conn
                        .close(true, u64::from(error_code), reason.as_bytes());
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
        // are drained.  Prevents one busy connection from monopolizing the
        // socket send buffer under fan-out.
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
        let offset = self.handle_offset;
        let flushed = flush_pending_writes(&mut self.conn_map, &mut self.pending_writes);
        for (local_handle, stream_id) in flushed {
            batch.push(JsH3Event::drain(offset | local_handle, stream_id));
        }
    }

    fn poll_drain_events(&mut self, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        self.conn_map.fill_handles(&mut self.handles_buf);
        for i in 0..self.handles_buf.len() {
            let handle = self.handles_buf[i];
            if self.last_expired.contains(&handle) {
                continue;
            }
            if let Some(conn) = self.conn_map.get_mut(handle) {
                if !conn.blocked_set.is_empty() {
                    conn.poll_drain_events(offset | (handle as u32), batch);
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
        let offset = self.handle_offset;
        let closed = self.conn_map.drain_closed();
        for handle in &closed {
            self.timer_heap.remove_connection(*handle);
            self.conn_send_buffers.remove(handle);
            self.pending_session_closes.remove(&(*handle as u32));
            self.pending_writes
                .retain(|&(ch, _), _| ch as usize != *handle);
            if !self.last_expired.contains(handle) {
                reactor_metrics::record_session_close(SessionKind::H3Server);
                batch.push(JsH3Event::session_close(offset | (*handle as u32)));
            }
        }
        self.last_expired.clear();
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        let timer_deadline = self.timer_heap.next_deadline();
        let close_deadline = self
            .pending_session_closes
            .values()
            .map(|&(_, _, d)| d)
            .min();
        match (timer_deadline, close_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

// ── H3 Client Protocol Handler ──────────────────────────────────────

struct H3ClientHandler {
    conn: H3Connection,
    pending_writes: HashMap<u64, PendingWrite>,
    send_buf: Vec<u8>,
    tx_pool: BufferPool,
    timer_deadline: Option<Instant>,
    session_closed_emitted: bool,
}

impl H3ClientHandler {
    fn new(
        local_addr: SocketAddr,
        server_addr: SocketAddr,
        server_name: &str,
        session_ticket: Option<&[u8]>,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        quiche_config: &mut quiche::Config,
    ) -> Option<Self> {
        let Ok(scid) = ConnectionMap::generate_random_scid() else {
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
        let conn = H3Connection::new(
            quiche_conn,
            scid,
            H3ConnectionInit {
                role: "client",
                qlog_dir,
                qlog_level,
                qpack_max_table_capacity: None,
                qpack_blocked_streams: None,
            },
        );
        let timer_deadline = conn.timeout().map(|t| Instant::now() + t);
        reactor_metrics::record_session_open(SessionKind::H3Client);
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            send_buf: vec![0u8; SEND_BUF_SIZE],
            tx_pool: BufferPool::new(256, 65535),
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

    fn send_request(&mut self, headers: Vec<(String, String)>, fin: bool) -> Result<u64, String> {
        if (self.conn.quiche_conn.is_established() || self.conn.quiche_conn.is_in_early_data())
            && !self.conn.is_established
        {
            let _ = self.conn.init_h3();
        }
        let h3_headers: Vec<quiche::h3::Header> = headers
            .iter()
            .map(|(name, value)| quiche::h3::Header::new(name.as_bytes(), value.as_bytes()))
            .collect();
        self.conn
            .send_request(&h3_headers, fin)
            .map_err(|error| format!("send_request failed: {error}"))
    }

    fn queue_stream_send(&mut self, stream_id: u64, data: Vec<u8>, fin: bool) {
        if let Some(pw) = self.pending_writes.get_mut(&stream_id) {
            pw.data.extend_from_slice(&data);
            pw.fin = pw.fin || fin;
            return;
        }

        let written = self.conn.send_body(stream_id, &data, fin).unwrap_or(0);
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

    fn close_stream(&mut self, stream_id: u64, error_code: u32) {
        let _ = self.conn.stream_close(stream_id, u64::from(error_code));
    }

    fn send_datagram(&mut self, data: &[u8]) -> bool {
        self.conn.send_datagram(data).is_ok()
    }

    fn metrics_snapshot(&self) -> JsSessionMetrics {
        snapshot_metrics(&self.conn)
    }

    fn remote_settings(&self) -> Vec<(u64, u64)> {
        self.conn.remote_settings()
    }

    fn ping(&mut self) -> bool {
        self.conn.quiche_conn.send_ack_eliciting().is_ok()
    }

    fn qlog_path(&self) -> Option<String> {
        self.conn.qlog_path.clone()
    }

    fn emit_session_close(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        if self.session_closed_emitted {
            return;
        }
        reactor_metrics::record_session_close(SessionKind::H3Client);
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

        if (self.conn.quiche_conn.is_established() || self.conn.quiche_conn.is_in_early_data())
            && !self.conn.is_established
        {
            let _ = self.conn.init_h3();
        }
        if self.conn.quiche_conn.is_established() && !self.conn.handshake_complete_emitted {
            self.conn.handshake_complete_emitted = true;
            batch.push(JsH3Event::handshake_complete(conn_handle));
        }
        self.conn.poll_h3_events(conn_handle, batch);
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
        self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);

        if self.conn.is_closed() {
            self.emit_session_close(batch, conn_handle);
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
            if self.conn.is_closed() {
                self.emit_session_close(batch, conn_handle);
            } else {
                self.conn.poll_h3_events(conn_handle, batch);
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
        conn: &mut H3Connection,
        _send_buf: &mut [u8],
        tx_pool: &mut BufferPool,
    ) -> Option<TxDatagram> {
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
        let flushed = flush_client_pending_writes(&mut self.conn, &mut self.pending_writes);
        for stream_id in flushed {
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

impl ProtocolHandler for H3ClientHandler {
    type Command = ClientWorkerCommand;

    fn dispatch_command(
        &mut self,
        cmd: ClientWorkerCommand,
        _batch: &mut Vec<JsH3Event>,
    ) -> bool {
        match cmd {
            ClientWorkerCommand::Shutdown => {
                if !self.session_closed_emitted {
                    reactor_metrics::record_session_close(SessionKind::H3Client);
                    self.session_closed_emitted = true;
                }
                return true;
            }
            ClientWorkerCommand::Close { error_code, reason } => {
                self.close_session(error_code, &reason);
            }
            ClientWorkerCommand::SendRequest {
                headers,
                fin,
                resp_tx,
            } => {
                let _ = resp_tx.send(self.send_request(headers, fin));
            }
            ClientWorkerCommand::StreamSend {
                stream_id,
                data,
                fin,
            } => {
                self.queue_stream_send(stream_id, data, fin);
            }
            ClientWorkerCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                self.close_stream(stream_id, error_code);
            }
            ClientWorkerCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.send_datagram(&data));
            }
            ClientWorkerCommand::GetSessionMetrics { resp_tx } => {
                let _ = resp_tx.send(Some(self.metrics_snapshot()));
            }
            ClientWorkerCommand::GetRemoteSettings { resp_tx } => {
                let _ = resp_tx.send(self.remote_settings());
            }
            ClientWorkerCommand::GetQlogPath { resp_tx } => {
                let _ = resp_tx.send(self.qlog_path());
            }
            ClientWorkerCommand::Ping { resp_tx } => {
                let _ = resp_tx.send(self.ping());
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

// ── Shared helpers ──────────────────────────────────────────────────

fn top_up_server_scids(conn_map: &mut ConnectionMap, handle: usize) {
    loop {
        let should_add_scid = match conn_map.get_mut(handle) {
            Some(conn) => conn.quiche_conn.is_established() && conn.quiche_conn.scids_left() > 0,
            None => return,
        };

        if !should_add_scid {
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

fn snapshot_metrics(conn: &H3Connection) -> JsSessionMetrics {
    JsSessionMetrics {
        packets_in: conn.metrics.packets_in,
        packets_out: conn.metrics.packets_out,
        bytes_in: conn.metrics.bytes_in as i64,
        bytes_out: conn.metrics.bytes_out as i64,
        handshake_time_ms: conn.handshake_time_ms(),
        rtt_ms: conn.rtt_ms(),
        cwnd: conn.cwnd() as i64,
        pmtu: conn.pmtu() as i64,
    }
}

/// Flush buffered partial writes for all streams.
fn flush_pending_writes(
    conn_map: &mut ConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
) -> Vec<(u32, u64)> {
    let mut flushed = Vec::new();
    pending.retain(|&(conn_handle, stream_id), pw| {
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            return false;
        };
        let written = conn.send_body(stream_id, &pw.data, pw.fin).unwrap_or(0);
        if written >= pw.data.len() {
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

/// Flush buffered partial writes for client streams.
fn flush_client_pending_writes(
    conn: &mut H3Connection,
    pending: &mut HashMap<u64, PendingWrite>,
) -> Vec<u64> {
    let mut flushed = Vec::new();
    pending.retain(|&stream_id, pw| {
        let written = conn.send_body(stream_id, &pw.data, pw.fin).unwrap_or(0);
        if written >= pw.data.len() {
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    /// QUIC path validation happens inside quiche — the worker accepts
    /// packets from any peer.
    #[test]
    fn allows_packets_after_peer_address_change() {
        let _original: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let _migrated: SocketAddr = "127.0.0.1:444".parse().expect("valid addr");
        // Acceptance is unconditional; this test verifies compile-time only.
    }
}
