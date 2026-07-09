//! Worker thread mode: a dedicated OS thread runs the QUIC/H3 hot loop,
//! delivering batched events to the JS main thread via ThreadsafeFunction.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
#[cfg(feature = "os-runtime")]
use ring::rand::SecureRandom;
use slab::Slab;

use crate::arc_buf::{ArcBuf, ArcBufFactory};
use crate::buffer_pool::{AdaptiveBufferPool, BufferPool};
use crate::chunk_pool::{Chunk, ChunkPool};
#[cfg(feature = "os-runtime")]
use crate::client_topology::{
    ClientSocketStrategy, SharedClientWorkerKey as SharedH3ClientWorkerKey,
    default_h3_client_socket_strategy, shared_client_bind_addr, shared_client_worker_key,
};
use crate::config::{Http3Config, TransportRuntimeMode};
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::connection_map::ConnectionMap;
use crate::datagram::TxDatagram;
use crate::error::Http3NativeError;
use crate::event_loop::{self, EventBatcher, MAX_BATCH_SIZE, ProtocolHandler, SEND_BUF_SIZE};
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::keylog_sink::KeylogBuffer;
use crate::outbound_admission::{
    OutboundAdmission, accepted_outbound_payload_units, outbound_payload_units,
};
use crate::pending_write::{
    PendingWrite, PendingWriteFlushOutcome, PendingWriteSendOutcome,
    flush_pending_write_with_progress,
};
use crate::reactor_metrics::{self, SessionKind, WorkerLoopExitCause, WorkerSpawnKind};
#[cfg(feature = "os-runtime")]
use crate::shared_client_reactor;
use crate::timer_heap::TimerHeap;
#[cfg(feature = "os-runtime")]
use crate::transport::{self, ErasedWaker};

const H3_INTERNAL_ERROR: u64 = 0x0102;

// Re-export for backward compatibility with server.rs / client.rs / quic_server.rs / quic_client.rs
#[cfg(feature = "node-api")]
pub use crate::event_loop::EventTsfn;

/// Commands sent from the JS main thread to the worker thread.
#[cfg(feature = "os-runtime")]
pub enum WorkerCommand {
    SendResponseHeaders {
        conn_handle: u32,
        stream_id: u64,
        headers: Vec<(String, String)>,
        fin: bool,
    },
    /// Combined headers + body + FIN in a single command — avoids 2 extra
    /// NAPI boundary crossings for the common respond-then-end pattern.
    SendResponse {
        conn_handle: u32,
        stream_id: u64,
        headers: Vec<(String, String)>,
        body: Chunk,
        fin: bool,
    },
    StreamSend {
        conn_handle: u32,
        stream_id: u64,
        chunk: Chunk,
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
        data: Chunk,
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

fn insert_pending_write<K>(pending: &mut HashMap<K, PendingWrite>, key: K, write: PendingWrite)
where
    K: Eq + std::hash::Hash,
{
    let queued = write.queued_bytes();
    if let Some(previous) = pending.insert(key, write) {
        reactor_metrics::record_outbound_pending_write_removed(previous.queued_bytes());
    }
    reactor_metrics::record_outbound_pending_write_added(queued);
}

fn remove_pending_write<K>(pending: &mut HashMap<K, PendingWrite>, key: &K) -> usize
where
    K: Eq + std::hash::Hash,
{
    if let Some(write) = pending.remove(key) {
        let queued_bytes = write.queued_bytes();
        let queued_units = write.queued_units();
        reactor_metrics::record_outbound_pending_write_removed(queued_bytes);
        queued_units
    } else {
        0
    }
}

struct PendingResponse {
    headers: Vec<(String, String)>,
    headers_sent: bool,
    headers_fin: bool,
    body: Option<PendingWrite>,
}

impl PendingResponse {
    fn headers_only(headers: Vec<(String, String)>, fin: bool) -> Self {
        Self {
            headers,
            headers_sent: false,
            headers_fin: fin,
            body: None,
        }
    }

    fn with_body(headers: Vec<(String, String)>, body: Chunk, fin: bool) -> Self {
        Self {
            headers,
            headers_sent: false,
            headers_fin: false,
            body: Some(PendingWrite::new(ArcBuf::from_chunk(body), fin)),
        }
    }

    fn push_body_chunk(&mut self, chunk: Chunk, fin: bool) {
        if let Some(write) = self.body.as_mut() {
            reactor_metrics::record_outbound_pending_write_added(write.push_chunk(chunk));
            if fin {
                write.set_fin();
            }
            return;
        }

        let write = PendingWrite::new(ArcBuf::from_chunk(chunk), fin);
        reactor_metrics::record_outbound_pending_write_added(write.queued_bytes());
        self.body = Some(write);
        self.headers_fin = false;
    }

    fn queued_bytes(&self) -> usize {
        self.body
            .as_ref()
            .map(PendingWrite::queued_bytes)
            .unwrap_or(0)
    }

    fn queued_units(&self) -> usize {
        self.body
            .as_ref()
            .map(PendingWrite::queued_units)
            .unwrap_or(0)
    }
}

fn insert_pending_response(
    pending: &mut HashMap<(u32, u64), PendingResponse>,
    key: (u32, u64),
    response: PendingResponse,
) {
    let queued = response.queued_bytes();
    if let Some(previous) = pending.insert(key, response) {
        reactor_metrics::record_outbound_pending_write_removed(previous.queued_bytes());
    }
    reactor_metrics::record_outbound_pending_write_added(queued);
}

fn remove_pending_response(
    pending: &mut HashMap<(u32, u64), PendingResponse>,
    key: &(u32, u64),
) -> usize {
    if let Some(response) = pending.remove(key) {
        let queued_bytes = response.queued_bytes();
        let queued_units = response.queued_units();
        reactor_metrics::record_outbound_pending_write_removed(queued_bytes);
        queued_units
    } else {
        0
    }
}

fn h3_headers(headers: &[(String, String)]) -> Vec<quiche::h3::Header> {
    headers
        .iter()
        .map(|(name, value)| quiche::h3::Header::new(name.as_bytes(), value.as_bytes()))
        .collect()
}

fn is_h3_stream_blocked(error: &Http3NativeError) -> bool {
    matches!(
        error,
        Http3NativeError::H3(quiche::h3::Error::StreamBlocked | quiche::h3::Error::Done)
    )
}

fn emit_h3_send_error_and_reset(
    conn: &mut H3Connection,
    batch: &mut Vec<JsH3Event>,
    conn_handle: u32,
    stream_id: u64,
    context: &str,
    error: &Http3NativeError,
) {
    batch.push(JsH3Event::error(
        conn_handle,
        stream_id as i64,
        0,
        format!("{context} failed: {error}"),
    ));
    let _ = conn.stream_close(stream_id, H3_INTERNAL_ERROR);
}

#[cfg(feature = "os-runtime")]
fn worker_command_outbound_bytes(cmd: &WorkerCommand) -> usize {
    match cmd {
        WorkerCommand::SendResponse { body, fin, .. } => {
            outbound_payload_units(body.remaining_len(), *fin)
        }
        WorkerCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        WorkerCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

fn client_worker_command_outbound_bytes(cmd: &ClientWorkerCommand) -> usize {
    match cmd {
        ClientWorkerCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        ClientWorkerCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn shared_client_worker_command_outbound_bytes(cmd: &SharedClientWorkerCommand) -> usize {
    match cmd {
        SharedClientWorkerCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        SharedClientWorkerCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

const PENDING_WRITE_POOL_SIZE: usize = 256;
const PENDING_WRITE_MIN_CAPACITY: usize = 4 * 1024;

/// Per-worker state inside a `WorkerHandle`.
#[cfg(feature = "os-runtime")]
pub struct H3ServerWorker {
    pub cmd_tx: Sender<WorkerCommand>,
    pub join_handle: Option<thread::JoinHandle<()>>,
    pub waker: Arc<dyn ErasedWaker>,
    outbound_admission: Arc<OutboundAdmission>,
}

/// Handle returned to the JS side for sending commands to the worker(s).
/// Supports multiple sharded workers via SO_REUSEPORT — commands are
/// routed to the correct worker by the worker index encoded in conn_handle.
#[cfg(feature = "os-runtime")]
pub struct WorkerHandle {
    workers: Vec<H3ServerWorker>,
    local_addr: SocketAddr,
}

#[cfg(feature = "os-runtime")]
impl WorkerHandle {
    /// Construct a `WorkerHandle` from a pre-spawned list of workers.
    pub fn from_workers(workers: Vec<H3ServerWorker>, local_addr: SocketAddr) -> Self {
        Self {
            workers,
            local_addr,
        }
    }

    pub(crate) fn try_admit_outbound(
        &self,
        conn_handle: u32,
        payload_len: usize,
        fin: bool,
    ) -> bool {
        self.workers[crate::server_sharding::worker_index(conn_handle)]
            .outbound_admission
            .try_admit(outbound_payload_units(payload_len, fin))
    }

    pub(crate) fn release_outbound_admission(
        &self,
        conn_handle: u32,
        payload_len: usize,
        fin: bool,
    ) {
        let _ = self.workers[crate::server_sharding::worker_index(conn_handle)]
            .outbound_admission
            .release(outbound_payload_units(payload_len, fin));
    }

    /// Route a command to the worker that owns `conn_handle`.
    pub fn send_command(&self, cmd: WorkerCommand) -> bool {
        let ch = command_conn_handle(&cmd);
        let worker = &self.workers[crate::server_sharding::worker_index(ch)];
        let local = remap_command_handle(cmd);
        let queued_bytes = worker_command_outbound_bytes(&local);
        if worker.cmd_tx.send(local).is_ok() {
            reactor_metrics::record_outbound_command_queued(queued_bytes);
            let _ = worker.waker.wake();
            true
        } else {
            false
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn wake_event_loop(&self) {
        for worker in &self.workers {
            let _ = worker.waker.wake();
        }
    }

    pub fn get_session_metrics(
        &self,
        conn_handle: u32,
    ) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(WorkerCommand::GetSessionMetrics {
                conn_handle: local,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server metrics".into())
        })
    }

    pub fn send_datagram<D>(&self, conn_handle: u32, data: D) -> Result<bool, Http3NativeError>
    where
        D: Into<Chunk>,
    {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(WorkerCommand::SendDatagram {
                conn_handle: local,
                data: data.into(),
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        Ok(resp_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    pub fn get_remote_settings(
        &self,
        conn_handle: u32,
    ) -> Result<Vec<(u64, u64)>, Http3NativeError> {
        let worker = &self.workers[crate::server_sharding::worker_index(conn_handle)];
        let local = crate::server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(WorkerCommand::GetRemoteSettings {
                conn_handle: local,
                resp_tx,
            })
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
        worker
            .cmd_tx
            .send(WorkerCommand::PingSession {
                conn_handle: local,
                resp_tx,
            })
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
        worker
            .cmd_tx
            .send(WorkerCommand::GetQlogPath {
                conn_handle: local,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server qlog path".into())
        })
    }

    /// Send the Shutdown command to all workers without joining threads.
    pub fn request_shutdown(&self) {
        for worker in &self.workers {
            let _ = worker.cmd_tx.send(WorkerCommand::Shutdown);
            let _ = worker.waker.wake();
        }
    }

    /// Join all worker threads. Call after `request_shutdown()`.
    pub fn join(&mut self) {
        for worker in &mut self.workers {
            if let Some(handle) = worker.join_handle.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

#[cfg(feature = "os-runtime")]
fn shutdown_spawned_h3_server_workers(workers: &mut Vec<H3ServerWorker>) {
    for worker in workers.iter() {
        let _ = worker.cmd_tx.send(WorkerCommand::Shutdown);
        let _ = worker.waker.wake();
    }
    for worker in workers.iter_mut() {
        if let Some(handle) = worker.join_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "os-runtime")]
impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(feature = "os-runtime")]
fn command_conn_handle(cmd: &WorkerCommand) -> u32 {
    match cmd {
        WorkerCommand::SendResponseHeaders { conn_handle, .. }
        | WorkerCommand::SendResponse { conn_handle, .. }
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

#[cfg(feature = "os-runtime")]
fn remap_command_handle(cmd: WorkerCommand) -> WorkerCommand {
    use crate::server_sharding::local_conn_handle;
    match cmd {
        WorkerCommand::SendResponseHeaders {
            conn_handle,
            stream_id,
            headers,
            fin,
        } => WorkerCommand::SendResponseHeaders {
            conn_handle: local_conn_handle(conn_handle),
            stream_id,
            headers,
            fin,
        },
        WorkerCommand::SendResponse {
            conn_handle,
            stream_id,
            headers,
            body,
            fin,
        } => WorkerCommand::SendResponse {
            conn_handle: local_conn_handle(conn_handle),
            stream_id,
            headers,
            body,
            fin,
        },
        WorkerCommand::StreamSend {
            conn_handle,
            stream_id,
            chunk,
            fin,
        } => WorkerCommand::StreamSend {
            conn_handle: local_conn_handle(conn_handle),
            stream_id,
            chunk,
            fin,
        },
        WorkerCommand::StreamClose {
            conn_handle,
            stream_id,
            error_code,
        } => WorkerCommand::StreamClose {
            conn_handle: local_conn_handle(conn_handle),
            stream_id,
            error_code,
        },
        WorkerCommand::SendTrailers {
            conn_handle,
            stream_id,
            headers,
        } => WorkerCommand::SendTrailers {
            conn_handle: local_conn_handle(conn_handle),
            stream_id,
            headers,
        },
        WorkerCommand::CloseSession {
            conn_handle,
            error_code,
            reason,
        } => WorkerCommand::CloseSession {
            conn_handle: local_conn_handle(conn_handle),
            error_code,
            reason,
        },
        WorkerCommand::SendDatagram {
            conn_handle,
            data,
            resp_tx,
        } => WorkerCommand::SendDatagram {
            conn_handle: local_conn_handle(conn_handle),
            data,
            resp_tx,
        },
        WorkerCommand::GetSessionMetrics {
            conn_handle,
            resp_tx,
        } => WorkerCommand::GetSessionMetrics {
            conn_handle: local_conn_handle(conn_handle),
            resp_tx,
        },
        WorkerCommand::GetRemoteSettings {
            conn_handle,
            resp_tx,
        } => WorkerCommand::GetRemoteSettings {
            conn_handle: local_conn_handle(conn_handle),
            resp_tx,
        },
        WorkerCommand::GetQlogPath {
            conn_handle,
            resp_tx,
        } => WorkerCommand::GetQlogPath {
            conn_handle: local_conn_handle(conn_handle),
            resp_tx,
        },
        WorkerCommand::PingSession {
            conn_handle,
            resp_tx,
        } => WorkerCommand::PingSession {
            conn_handle: local_conn_handle(conn_handle),
            resp_tx,
        },
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
        chunk: Chunk,
        fin: bool,
    },
    StreamClose {
        stream_id: u64,
        error_code: u32,
    },
    SendDatagram {
        data: Chunk,
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

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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
        chunk: Chunk,
        fin: bool,
    },
    StreamClose {
        session_handle: u32,
        stream_id: u64,
        error_code: u32,
    },
    SendDatagram {
        session_handle: u32,
        data: Chunk,
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

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
struct SharedClientWorkerControl {
    cmd_tx: Sender<SharedClientWorkerCommand>,
    waker: Arc<dyn ErasedWaker>,
    local_addr: SocketAddr,
    outbound_admission: Arc<OutboundAdmission>,
    join_handle: Mutex<Option<thread::JoinHandle<()>>>,
    running: AtomicBool,
    session_count: AtomicUsize,
    key: SharedH3ClientWorkerKey,
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
impl SharedClientWorkerControl {
    fn wake(&self) {
        let _ = self.waker.wake();
    }
}

#[cfg(feature = "os-runtime")]
enum ClientWorkerHandleKind {
    Dedicated {
        cmd_tx: Sender<ClientWorkerCommand>,
        join_handle: Option<thread::JoinHandle<()>>,
        waker: Arc<dyn ErasedWaker>,
    },
    #[cfg(feature = "node-api")]
    Shared {
        session_handle: u32,
        worker: Arc<SharedClientWorkerControl>,
    },
}

/// Handle returned to JS for controlling the client worker thread.
#[cfg(feature = "os-runtime")]
pub struct ClientWorkerHandle {
    kind: Option<ClientWorkerHandleKind>,
    local_addr: SocketAddr,
    outbound_admission: Arc<OutboundAdmission>,
}

#[cfg(feature = "os-runtime")]
impl ClientWorkerHandle {
    pub(crate) fn try_admit_outbound(&self, payload_len: usize, fin: bool) -> bool {
        self.outbound_admission
            .try_admit(outbound_payload_units(payload_len, fin))
    }

    pub(crate) fn release_outbound_admission(&self, payload_len: usize, fin: bool) {
        let _ = self
            .outbound_admission
            .release(outbound_payload_units(payload_len, fin));
    }

    pub fn wake_event_loop(&self) {
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { waker, .. }) => {
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
            Some(ClientWorkerHandleKind::Shared { worker, .. }) => {
                worker.wake();
            }
            None => {}
        }
    }

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
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
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
    pub fn stream_send(&self, stream_id: u64, chunk: Chunk, fin: bool) -> bool {
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                let queued_bytes = chunk.remaining_len();
                if cmd_tx
                    .send(ClientWorkerCommand::StreamSend {
                        stream_id,
                        chunk,
                        fin,
                    })
                    .is_ok()
                {
                    reactor_metrics::record_outbound_command_queued(queued_bytes);
                    let _ = waker.wake();
                    true
                } else {
                    false
                }
            }
            #[cfg(feature = "node-api")]
            Some(ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                let queued_bytes = chunk.remaining_len();
                if worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::StreamSend {
                        session_handle: *session_handle,
                        stream_id,
                        chunk,
                        fin,
                    })
                    .is_ok()
                {
                    reactor_metrics::record_outbound_command_queued(queued_bytes);
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
            #[cfg(feature = "node-api")]
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

    pub fn send_datagram<D>(&self, data: D) -> Result<bool, Http3NativeError>
    where
        D: Into<Chunk>,
    {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        let data = data.into();
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::SendDatagram { data, resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
            }
        }
        Ok(resp_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    pub fn get_session_metrics(&self) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(ClientWorkerCommand::GetSessionMetrics { resp_tx })
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
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
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
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
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
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
                    .map_err(|_| {
                        Http3NativeError::InvalidState("client worker not running".into())
                    })?;
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
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
                return Err(Http3NativeError::InvalidState(
                    "client worker not running".into(),
                ));
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
            #[cfg(feature = "node-api")]
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

    /// Send the Shutdown (or ReleaseSession) command without joining threads.
    pub fn request_shutdown(&self) {
        let Some(kind) = &self.kind else { return };
        match kind {
            ClientWorkerHandleKind::Dedicated { cmd_tx, waker, .. } => {
                let _ = cmd_tx.send(ClientWorkerCommand::Shutdown);
                let _ = waker.wake();
            }
            #[cfg(feature = "node-api")]
            ClientWorkerHandleKind::Shared {
                session_handle,
                worker,
            } => {
                let _ = worker
                    .cmd_tx
                    .send(SharedClientWorkerCommand::ReleaseSession {
                        session_handle: *session_handle,
                    });
                worker.wake();
            }
        }
    }

    /// Join the worker thread. Call after `request_shutdown()`.
    pub fn join(&mut self) {
        let Some(kind) = self.kind.take() else { return };
        match kind {
            ClientWorkerHandleKind::Dedicated {
                mut join_handle, ..
            } => {
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
            }
            #[cfg(feature = "node-api")]
            ClientWorkerHandleKind::Shared { worker, .. } => {
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

    pub fn shutdown(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

#[cfg(feature = "os-runtime")]
impl Drop for ClientWorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Spawn functions ─────────────────────────────────────────────────

/// Cap a client's advertised/accepted UDP payload size to the path MTU.
/// Always compiled (not `os-runtime`-gated): `effective_pmtud_ceiling`
/// itself falls back to a constant when the kernel route query is
/// unavailable, so this stays reachable for a future direct-call/wasm
/// constructor that has no socket to query.
fn apply_h3_client_path_mtu_ceiling(config: &mut quiche::Config, server_addr: &SocketAddr) {
    let ceiling = crate::config::effective_pmtud_ceiling(server_addr);
    config.set_max_send_udp_payload_size(ceiling);
    config.set_max_recv_udp_payload_size(ceiling);
}

// --- Node-API-dependent spawn functions (use EventTsfn / shared workers) ---
#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
/// Spawn a client worker thread that owns UDP I/O and QUIC/H3 processing.
#[allow(clippy::too_many_arguments)]
pub fn spawn_client_worker(
    mut quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    tsfn: EventTsfn,
) -> Result<ClientWorkerHandle, Http3NativeError> {
    apply_h3_client_path_mtu_ceiling(&mut quiche_config, &server_addr);

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
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let admission_ref = Arc::clone(&outbound_admission);

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
            admission_ref,
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
        outbound_admission,
    })
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
struct SharedClientSession {
    handler: H3ClientHandler,
    batcher: EventBatcher,
    server_addr: SocketAddr,
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn shared_client_worker_registry()
-> &'static Mutex<HashMap<SharedH3ClientWorkerKey, Weak<SharedClientWorkerControl>>> {
    static REGISTRY: OnceLock<
        Mutex<HashMap<SharedH3ClientWorkerKey, Weak<SharedClientWorkerControl>>>,
    > = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let control = Arc::new(SharedClientWorkerControl {
        cmd_tx,
        waker: waker_arc.clone(),
        local_addr,
        outbound_admission: Arc::clone(&outbound_admission),
        join_handle: Mutex::new(None),
        running: AtomicBool::new(true),
        session_count: AtomicUsize::new(0),
        key: key.clone(),
    });
    let control_for_thread = Arc::clone(&control);
    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3ClientShared);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        run_shared_client_event_loop(&mut driver, cmd_rx, local_addr, outbound_admission);
        control_for_thread.running.store(false, Ordering::Release);
    });
    if let Ok(mut slot) = control.join_handle.lock() {
        *slot = Some(join_handle);
    }
    registry.insert(key, Arc::downgrade(&control));
    Ok(control)
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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
        outbound_admission: Arc::clone(&worker.outbound_admission),
    })
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn emit_shared_client_write_ready(
    sessions: &mut Slab<SharedClientSession>,
    pending_release: &mut Vec<u32>,
) {
    for (handle, session) in sessions.iter_mut() {
        if !session
            .batcher
            .collect_atomic(|batch| batch.push(JsH3Event::write_ready(0)))
        {
            pending_release.push(handle as u32);
        }
    }
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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
        reactor_metrics::record_lifecycle_trace(
            "h3-client",
            "shared-session-release",
            None,
            None,
            None,
            Some(format!(
                "conn_handle={handle} pending_writes={} blocked_streams={}",
                session.handler.pending_writes.len(),
                session.handler.conn.blocked_set.len()
            )),
        );
        if !session.handler.session_closed_emitted {
            reactor_metrics::record_session_close(SessionKind::H3Client);
        }
    }
    timer_heap.remove_connection(handle);
    let session = sessions.remove(handle);
    let released_units = session
        .handler
        .pending_writes
        .values()
        .map(PendingWrite::queued_units)
        .sum();
    let _ = session.handler.outbound_admission.release(released_units);
    route_by_dcid.retain(|_, mapped_handle| *mapped_handle != handle);
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn sync_shared_client_timer(
    timer_heap: &mut TimerHeap,
    handle: usize,
    session: &SharedClientSession,
) {
    shared_client_reactor::sync_timer(timer_heap, handle, session, |current| {
        current.handler.timer_deadline
    });
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn flush_shared_client_sends(
    sessions: &mut Slab<SharedClientSession>,
    handles_buf: &mut Vec<usize>,
    tx_pool: &mut BufferPool,
    outbound: &mut Vec<TxDatagram>,
) {
    shared_client_reactor::flush_round_robin_sends(sessions, handles_buf, outbound, |session| {
        H3ClientHandler::try_send_next_with_pool_parts(
            &mut session.handler.conn,
            session.handler.send_buf.as_mut_slice(),
            tx_pool,
        )
    });
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn refresh_shared_client_timers_after_sends(
    sessions: &mut Slab<SharedClientSession>,
    timer_heap: &mut TimerHeap,
    handles_buf: &mut Vec<usize>,
) {
    handles_buf.clear();
    handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
    for handle in handles_buf.iter().copied() {
        if let Some(session) = sessions.get_mut(handle) {
            session.handler.refresh_timeout_deadline();
            sync_shared_client_timer(timer_heap, handle, session);
        }
    }
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
fn run_shared_client_event_loop<D: transport::Driver>(
    driver: &mut D,
    cmd_rx: crossbeam_channel::Receiver<SharedClientWorkerCommand>,
    local_addr: SocketAddr,
    outbound_admission: Arc<OutboundAdmission>,
) {
    let _stop_guard = event_loop::WorkerLoopStopGuard;
    reactor_metrics::record_lifecycle_trace(
        "event-loop",
        "worker-loop-start",
        Some(driver.driver_kind()),
        None,
        Some(driver.pending_tx_count()),
        None,
    );
    let mut sessions: Slab<SharedClientSession> = Slab::new();
    let mut route_by_dcid: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut timer_heap = TimerHeap::new();
    let mut tx_pool = BufferPool::new(256, 65535);
    let mut handles_buf = Vec::new();
    let mut outbound = Vec::new();
    let mut closed_sessions = Vec::new();
    let mut release_requested = false;
    // Sessions queued by ReleaseSession; their CONNECTION_CLOSE frame
    // (set up by a preceding Close command) needs `flush_shared_client_sends`
    // to run *before* the session is actually removed, otherwise the
    // peer never sees the close. We park them here, let the per-iteration
    // flush deliver any pending packets, then remove + emit
    // SHUTDOWN_COMPLETE below.
    let mut pending_release: Vec<u32> = Vec::new();
    let mut poll_now = false;

    loop {
        let deadline = if poll_now {
            poll_now = false;
            Some(Instant::now())
        } else {
            timer_heap.next_deadline()
        };

        let outcome = match event_loop::poll_with_event_backpressure(driver, deadline) {
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

        let mut commands_drained = 0;
        while commands_drained < event_loop::MAX_COMMANDS_PER_TICK {
            let Ok(cmd) = cmd_rx.try_recv() else {
                break;
            };
            commands_drained += 1;
            let outbound_units = shared_client_worker_command_outbound_bytes(&cmd);
            reactor_metrics::record_outbound_command_dequeued(outbound_units);
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
                        Arc::clone(&outbound_admission),
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
                    chunk,
                    fin,
                } => {
                    let mut released_units = outbound_units;
                    let should_release =
                        if let Some(session) = sessions.get_mut(session_handle as usize) {
                            !session.batcher.collect_atomic(|batch| {
                                released_units = session.handler.queue_stream_send(
                                    stream_id,
                                    chunk,
                                    fin,
                                    batch,
                                    session_handle,
                                );
                            })
                        } else {
                            false
                        };
                    if outbound_admission.release(released_units) {
                        emit_shared_client_write_ready(&mut sessions, &mut pending_release);
                    }
                    if should_release {
                        pending_release.push(session_handle);
                    }
                }
                SharedClientWorkerCommand::StreamClose {
                    session_handle,
                    stream_id,
                    error_code,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        let released = session.handler.close_stream(stream_id, error_code);
                        if outbound_admission.release(released) {
                            emit_shared_client_write_ready(&mut sessions, &mut pending_release);
                        }
                    }
                }
                SharedClientWorkerCommand::SendDatagram {
                    session_handle,
                    data,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.send_datagram(data));
                    if outbound_admission.release(outbound_units) {
                        emit_shared_client_write_ready(&mut sessions, &mut pending_release);
                    }
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
                    // Defer the actual removal until after this iteration's
                    // flush_shared_client_sends, so any
                    // CONNECTION_CLOSE frame queued by a preceding `Close`
                    // command reaches the wire before the session is gone.
                    release_requested = true;
                    pending_release.push(session_handle);
                }
            }
        }
        if commands_drained == event_loop::MAX_COMMANDS_PER_TICK && !cmd_rx.is_empty() {
            poll_now = true;
        }

        flush_shared_client_sends(&mut sessions, &mut handles_buf, &mut tx_pool, &mut outbound);
        refresh_shared_client_timers_after_sends(&mut sessions, &mut timer_heap, &mut handles_buf);
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

        // Now that pending sends (CLOSE frames included) have hit the
        // wire, finalize any sessions queued for release: emit the
        // SHUTDOWN_COMPLETE sentinel so the JS `close()` await resolves,
        // then remove the session from routing maps.
        for session_handle in pending_release.drain(..) {
            if let Some(session) = sessions.get_mut(session_handle as usize) {
                event_loop::push_shutdown_complete(&mut session.batcher);
                let _ = session.batcher.flush();
            }
            remove_shared_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                session_handle as usize,
            );
        }

        let rx_count = outcome.rx.len();
        let mut rx_recycled: Vec<Vec<u8>> = Vec::new();
        for (rx_idx, pkt) in outcome.rx.into_iter().enumerate() {
            let peer = pkt.peer;
            let mut data = pkt.data;
            if let Ok(header) =
                quiche::Header::from_slice(data.as_mut_slice(), crate::cid::SCID_LEN)
            {
                if let Some(handle) = route_by_dcid.get(header.dcid.as_ref()).copied() {
                    let mut should_remove = false;
                    if let Some(session) = sessions.get_mut(handle) {
                        if peer == session.server_addr {
                            if !session.batcher.collect_atomic(|batch| {
                                session.handler.process_packet_for_handle(
                                    data.as_mut_slice(),
                                    peer,
                                    local_addr,
                                    0,
                                    batch,
                                    handle as u32,
                                );
                            }) {
                                should_remove = true;
                            } else {
                                refresh_shared_client_dcid(&mut route_by_dcid, handle, session);
                                sync_shared_client_timer(&mut timer_heap, handle, session);
                                if session.batcher.len() >= MAX_BATCH_SIZE
                                    && !session.batcher.flush()
                                {
                                    should_remove = true;
                                }
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
                }
            }
            rx_recycled.push(data);
            if (rx_idx + 1) % 64 == 0 && rx_idx + 1 < rx_count {
                flush_shared_client_sends(
                    &mut sessions,
                    &mut handles_buf,
                    &mut tx_pool,
                    &mut outbound,
                );
                refresh_shared_client_timers_after_sends(
                    &mut sessions,
                    &mut timer_heap,
                    &mut handles_buf,
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
        if !rx_recycled.is_empty() {
            driver.recycle_rx_buffers(rx_recycled);
        }

        let now = Instant::now();
        closed_sessions.clear();
        for handle in timer_heap.pop_expired(now) {
            if let Some(session) = sessions.get_mut(handle) {
                let app_budget = event_loop::app_event_budget(session.batcher.len());
                if !session.batcher.collect_atomic(|batch| {
                    session.handler.process_timers_for_handle(
                        now,
                        app_budget,
                        batch,
                        handle as u32,
                    );
                }) {
                    closed_sessions.push(handle);
                } else {
                    refresh_shared_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_client_timer(&mut timer_heap, handle, session);
                    if session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush() {
                        closed_sessions.push(handle);
                    }
                }
            }
        }
        handles_buf.clear();
        handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
        for handle in handles_buf.iter().copied() {
            if let Some(session) = sessions.get_mut(handle) {
                let app_budget = event_loop::app_event_budget(session.batcher.len());
                let app_ok = session.batcher.collect_atomic(|batch| {
                    session
                        .handler
                        .poll_app_events_for_handle(app_budget, batch, handle as u32);
                });
                if app_ok {
                    refresh_shared_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_client_timer(&mut timer_heap, handle, session);
                }
                let drain_ok = app_ok
                    && session.batcher.collect_atomic(|batch| {
                        session
                            .handler
                            .poll_drain_events_for_handle(batch, handle as u32);
                    });
                let flush_ok = drain_ok
                    && session.batcher.collect_atomic(|batch| {
                        session
                            .handler
                            .flush_pending_writes_for_handle(batch, handle as u32);
                    });
                if !flush_ok
                    || (session.batcher.len() >= MAX_BATCH_SIZE && !session.batcher.flush())
                {
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
        refresh_shared_client_timers_after_sends(&mut sessions, &mut timer_heap, &mut handles_buf);
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
            // Emit SHUTDOWN_COMPLETE before removing so the JS-side
            // ClientEventLoop.close() await on the sentinel resolves
            // even when the session was auto-reaped (e.g., via peer
            // CONNECTION_CLOSE) rather than via an explicit
            // ReleaseSession command.
            if let Some(session) = sessions.get_mut(handle) {
                event_loop::push_shutdown_complete(&mut session.batcher);
                let _ = session.batcher.flush();
            }
            remove_shared_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                handle,
            );
        }

        if sessions.is_empty() && driver.pending_tx_count() == 0 {
            let cause = if release_requested {
                WorkerLoopExitCause::Command
            } else {
                WorkerLoopExitCause::HandlerDone
            };
            let action = if release_requested {
                "exit-command"
            } else {
                "exit-handler-done"
            };
            reactor_metrics::record_worker_loop_exit(cause);
            reactor_metrics::record_lifecycle_trace(
                "event-loop",
                action,
                Some(driver.driver_kind()),
                None,
                Some(driver.pending_tx_count()),
                None,
            );
            return;
        }
    }
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
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
    // With SO_REUSEPORT, per-worker connection limits are approximate since
    // the kernel distributes packets by 4-tuple hash.  For exact enforcement
    // of small limits (e.g. maxConnections=2), use a single worker.
    let num_workers = if http3_config.max_connections <= num_workers {
        1
    } else {
        num_workers.max(1)
    };
    let use_reuse_port = num_workers > 1;

    // Bind the first socket to discover the actual local address.
    let first_socket = transport::socket::bind_worker_socket(bind_addr, use_reuse_port)?;
    first_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    let _ = transport::socket::set_socket_buffers(&first_socket, 2 * 1024 * 1024);
    let local_addr = first_socket.local_addr().map_err(Http3NativeError::Io)?;

    // Divide max_connections across workers so the total stays correct.
    let per_worker_max_connections = (http3_config.max_connections + num_workers - 1) / num_workers;

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
        let outbound_admission = Arc::new(OutboundAdmission::default());
        let admission_ref = Arc::clone(&outbound_admission);
        let http3_clone = Http3Config {
            qlog_dir: http3_config.qlog_dir.clone(),
            qlog_level: http3_config.qlog_level.clone(),
            qpack_max_table_capacity: http3_config.qpack_max_table_capacity,
            qpack_blocked_streams: http3_config.qpack_blocked_streams,
            max_connections: per_worker_max_connections,
            disable_retry: http3_config.disable_retry,
            reuse_port: http3_config.reuse_port,
            cid_encoding: http3_config.cid_encoding.clone(),
            runtime_mode: http3_config.runtime_mode,
            client_auth: http3_config.client_auth,
        };

        reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3Server);
        let join_handle = thread::spawn(move || {
            let mut driver = driver;
            let mut handler = H3ServerHandler::new(config, http3_clone, 0, admission_ref);
            event_loop::run_event_loop(
                &mut driver,
                cmd_rx,
                &mut handler,
                EventBatcher::new_shared_tsfn(tsfn_ref),
                local_addr,
            );
        });
        workers.push(H3ServerWorker {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_arc,
            outbound_admission,
        });
    }

    // Workers 1..N bind new sockets to the same address via SO_REUSEPORT.
    for i in 1..num_workers {
        let socket = match transport::socket::bind_worker_socket(local_addr, true) {
            Ok(socket) => socket,
            Err(err) => {
                shutdown_spawned_h3_server_workers(&mut workers);
                return Err(err);
            }
        };
        if let Err(err) = socket.set_nonblocking(true).map_err(Http3NativeError::Io) {
            shutdown_spawned_h3_server_workers(&mut workers);
            return Err(err);
        }
        let _ = transport::socket::set_socket_buffers(&socket, 2 * 1024 * 1024);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut config = match make_quiche_config() {
            Ok(config) => config,
            Err(err) => {
                shutdown_spawned_h3_server_workers(&mut workers);
                return Err(err);
            }
        };
        config.set_max_send_udp_payload_size(ceiling);
        config.set_max_recv_udp_payload_size(ceiling);
        let (driver, waker) =
            match transport::create_platform_driver(socket, http3_config.runtime_mode) {
                Ok(driver_and_waker) => driver_and_waker,
                Err(err) => {
                    shutdown_spawned_h3_server_workers(&mut workers);
                    return Err(err);
                }
            };
        let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
        let tsfn_ref = Arc::clone(&tsfn);
        let outbound_admission = Arc::new(OutboundAdmission::default());
        let admission_ref = Arc::clone(&outbound_admission);
        let http3_clone = Http3Config {
            qlog_dir: http3_config.qlog_dir.clone(),
            qlog_level: http3_config.qlog_level.clone(),
            qpack_max_table_capacity: http3_config.qpack_max_table_capacity,
            qpack_blocked_streams: http3_config.qpack_blocked_streams,
            max_connections: per_worker_max_connections,
            disable_retry: http3_config.disable_retry,
            reuse_port: http3_config.reuse_port,
            cid_encoding: http3_config.cid_encoding.clone(),
            runtime_mode: http3_config.runtime_mode,
            client_auth: http3_config.client_auth,
        };
        let worker_index = i as u32;

        reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3Server);
        let join_handle = thread::spawn(move || {
            let mut driver = driver;
            let mut handler =
                H3ServerHandler::new(config, http3_clone, worker_index, admission_ref);
            event_loop::run_event_loop(
                &mut driver,
                cmd_rx,
                &mut handler,
                EventBatcher::new_shared_tsfn(tsfn_ref),
                local_addr,
            );
        });
        workers.push(H3ServerWorker {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_arc,
            outbound_admission,
        });
    }

    Ok(WorkerHandle {
        workers,
        local_addr,
    })
}

// ── Generic driver-parameterized spawn functions for testing ─────────

/// Spawn a single H3 server worker on a caller-provided driver (e.g. `MockDriver`).
///
/// This is the H3 analogue of `quic_worker::spawn_server_worker_on_driver`.
#[cfg(feature = "os-runtime")]
pub fn spawn_h3_server_worker_on_driver<D>(
    quiche_config: quiche::Config,
    http3_config: Http3Config,
    worker_index: u32,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<WorkerCommand>,
    cmd_rx: crossbeam_channel::Receiver<WorkerCommand>,
    batcher: EventBatcher,
) -> H3ServerWorker
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let admission_ref = Arc::clone(&outbound_admission);

    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::H3Server);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut handler =
            H3ServerHandler::new(quiche_config, http3_config, worker_index, admission_ref);
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, batcher, local_addr);
    });

    H3ServerWorker {
        cmd_tx,
        join_handle: Some(join_handle),
        waker: waker_clone,
        outbound_admission,
    }
}

/// Spawn a dedicated H3 client worker on a caller-provided driver (e.g. `MockDriver`).
///
/// This is the H3 analogue of `quic_worker::spawn_dedicated_quic_client_on_driver`.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "os-runtime")]
pub fn spawn_h3_client_on_driver<D>(
    mut quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<ClientWorkerCommand>,
    cmd_rx: crossbeam_channel::Receiver<ClientWorkerCommand>,
    batcher: EventBatcher,
) -> ClientWorkerHandle
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    apply_h3_client_path_mtu_ceiling(&mut quiche_config, &server_addr);

    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let admission_ref = Arc::clone(&outbound_admission);

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
            admission_ref,
        );
        let Some(mut handler) = handler else { return };
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, batcher, local_addr);
    });

    ClientWorkerHandle {
        kind: Some(ClientWorkerHandleKind::Dedicated {
            cmd_tx,
            join_handle: Some(join_handle),
            waker: waker_clone,
        }),
        local_addr,
        outbound_admission,
    }
}

// ── H3 Server Protocol Handler ──────────────────────────────────────

/// HTTP/3 server protocol state machine: a `ConnectionMap` (many
/// connections), a `TimerHeap` (one soonest-deadline query across all of
/// them), buffer pools, and per-stream pending-write bookkeeping.
///
/// Kept always-compiled (no `os-runtime` gate) and `pub` (re-exported by
/// `wasm_exports` under `wasm-abi`), mirroring `H3ClientHandler` exactly —
/// see that type's doc comment. Only [`H3ServerHandler::new`] (the
/// ring-backed constructor) is `os-runtime`-gated;
/// [`H3ServerHandler::new_direct`] is the always-compiled alternative that
/// takes a caller-supplied 32-byte retry-token/SCID key instead. Unlike
/// the client, packet *routing* itself (accepting brand-new connections,
/// generating fresh per-connection SCIDs, retry-token mint/validate) has
/// two separate implementations rather than one shared one — see
/// [`H3ServerHandler::process_inbound_packet`]'s doc comment for why.
pub struct H3ServerHandler {
    conn_map: ConnectionMap,
    timer_heap: TimerHeap,
    buffer_pool: BufferPool,
    tx_pool: BufferPool,
    pending_write_pool: AdaptiveBufferPool,
    pending_writes: HashMap<(u32, u64), PendingWrite>,
    pending_responses: HashMap<(u32, u64), PendingResponse>,
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
    chunk_pool: ChunkPool,
    chunk_pool_rx: crossbeam_channel::Receiver<Vec<u8>>,
    outbound_admission: Arc<OutboundAdmission>,
}

impl H3ServerHandler {
    /// Native constructor: sources the `ConnectionMap`'s 32-byte
    /// HMAC/SCID key from `ring`'s system RNG via
    /// `ConnectionMap::with_max_connections_and_cid`. Requires
    /// `os-runtime` for the same reason as `H3ClientHandler::new`.
    #[cfg(feature = "os-runtime")]
    fn new(
        quiche_config: quiche::Config,
        http3_config: Http3Config,
        worker_index: u32,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let conn_map = ConnectionMap::with_max_connections_and_cid(
            http3_config.max_connections,
            http3_config.cid_encoding.clone(),
        );
        Self::from_parts(
            quiche_config,
            http3_config,
            conn_map,
            crate::server_sharding::handle_offset(worker_index),
            outbound_admission,
        )
    }

    /// Direct-call constructor for a sans-IO caller (a wasm ABI, or the
    /// unit tests below): the caller supplies the 32-byte retry-token/SCID
    /// key directly (e.g. from a JS host RNG via `crypto.getRandomValues`,
    /// the server-side analogue of the client's `scidHex` convention)
    /// instead of requiring `ring`. `handle_offset` is always `0` — a
    /// wasm/direct-call server is always a single logical worker, no
    /// sharding needed.
    pub fn new_direct(
        quiche_config: quiche::Config,
        http3_config: Http3Config,
        retry_token_key: [u8; 32],
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let conn_map = ConnectionMap::with_key_bytes(
            http3_config.max_connections,
            http3_config.cid_encoding.clone(),
            retry_token_key,
        );
        Self::from_parts(quiche_config, http3_config, conn_map, 0, outbound_admission)
    }

    fn from_parts(
        quiche_config: quiche::Config,
        http3_config: Http3Config,
        conn_map: ConnectionMap,
        handle_offset: u32,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let disable_retry = http3_config.disable_retry;
        let (chunk_pool, _chunk_pool_return, chunk_pool_rx) = ChunkPool::with_return_channel(64);
        Self {
            conn_map,
            timer_heap: TimerHeap::new(),
            buffer_pool: BufferPool::default(),
            tx_pool: BufferPool::new(256, 65535),
            pending_write_pool: AdaptiveBufferPool::new(
                PENDING_WRITE_POOL_SIZE,
                PENDING_WRITE_MIN_CAPACITY,
            ),
            pending_writes: HashMap::new(),
            pending_responses: HashMap::new(),
            pending_session_closes: HashMap::new(),
            conn_send_buffers: HashMap::new(),
            handles_buf: Vec::new(),
            http3_config,
            quiche_config,
            disable_retry,
            last_expired: Vec::new(),
            handle_offset,
            chunk_pool,
            chunk_pool_rx,
            outbound_admission,
        }
    }

    fn release_outbound_admission(&self, units: usize, batch: &mut Vec<JsH3Event>) {
        if self.outbound_admission.release(units) {
            batch.push(JsH3Event::write_ready(0));
        }
    }

    /// Soonest quiche timeout deadline across every connection this
    /// server holds, or `None` if none are pending (the "next timeout"
    /// direct-call operation — hoisted out of the `ProtocolHandler` impl
    /// below so a caller without the trait in scope can still read it,
    /// mirroring `H3ClientHandler::next_timer_deadline`).
    pub fn soonest_deadline(&mut self) -> Option<Instant> {
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

    /// `true` once there are no live connections and no pending graceful
    /// closes — the "is the whole server done" direct-call query.
    pub fn is_idle(&self) -> bool {
        self.conn_map.is_empty() && self.pending_session_closes.is_empty()
    }

    /// `true` if `conn_handle` refers to a connection that is closed (or
    /// no longer tracked at all) — the "is this connection done"
    /// direct-call query. A handle that was never valid, or has already
    /// been reaped by `reap_closed_connections`, also reads as closed
    /// (there is nothing left to wait for either way).
    pub fn connection_is_closed(&self, conn_handle: u32) -> bool {
        self.conn_map
            .get(conn_handle as usize)
            .is_none_or(H3Connection::is_closed)
    }

    /// Number of live connections currently tracked.
    pub fn connection_count(&self) -> usize {
        self.conn_map.len()
    }

    /// Graceful whole-server shutdown (the `hs_shutdown` direct-call
    /// operation): close every live connection with an application-level
    /// CONNECTION_CLOSE so peers see a graceful shutdown instead of
    /// waiting on idle timeout — mirrors `WorkerCommand::Shutdown`'s match
    /// arm exactly (native's own handling of the equivalent request).
    pub fn shutdown_all_connections(&mut self) {
        let mut handles = Vec::new();
        self.conn_map.fill_handles(&mut handles);
        for handle in handles {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                if !conn.quiche_conn.is_closed() && !conn.quiche_conn.is_draining() {
                    let _ = conn.quiche_conn.close(true, 0, b"server shutdown");
                }
            }
        }
    }

    /// Direct-call packet routing (the `hs_recv` primitive): parse the
    /// QUIC header, route to an existing connection by DCID, or accept
    /// (subject to the retry-token dance) / retry a fresh Initial packet.
    /// Any resulting Retry/Version-Negotiation packet is appended to
    /// `pending_outbound` (mirrors `ProtocolHandler::process_packet`'s own
    /// `pending_outbound` parameter — the retry path is the one place a
    /// server response bypasses a per-connection `quiche::Connection::send()`
    /// call, so it cannot flow through `flush_all_sends` alone).
    ///
    /// This is a **parallel implementation** of the native
    /// `ProtocolHandler::process_packet` below, not a hoisted-and-shared
    /// one, for a deliberate reason: the two differ in exactly the two
    /// spots that touch entropy sourced from `ring`, and this crate treats
    /// native's SCID-generation entropy source as a security-sensitive
    /// property not to change casually just to save code:
    ///
    /// 1. Fresh server SCIDs come from
    ///    [`ConnectionMap::generate_scid_direct`] (the always-compiled,
    ///    HMAC-PRF-based generator — see `retry_token.rs`) instead of
    ///    [`ConnectionMap::generate_scid`] (`ring`-backed). Both produce
    ///    valid, unique-enough, QUIC-LB-encoded-if-configured SCIDs; only
    ///    the underlying entropy-generation algorithm differs.
    /// 2. `top_up_server_scids` (issuing *additional* SCIDs after the
    ///    handshake, for connection migration) is intentionally **not**
    ///    called here — it is itself `ring`-backed
    ///    (`generate_stateless_reset_token`), and connection migration is
    ///    out of scope for this first server-wasm cut (mirrors the
    ///    client-side N4 non-goal in `docs/WASM_CLIENT_PLAN.md`: no
    ///    connection migration). A direct-call server's connections simply
    ///    never grow beyond their initial SCID.
    ///
    /// Also adds H3 server client-certificate-required enforcement (a
    /// genuinely new capability alongside server-side wasm support, not a
    /// port of existing native behavior — native's file-based H3 server
    /// config has none today; see `JsServerOptions::client_auth`'s doc
    /// comment). Mirrors `QuicServerHandler::process_inbound_packet`'s
    /// identical check exactly.
    pub fn process_inbound_packet(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        pending_outbound: &mut Vec<TxDatagram>,
        app_event_budget: usize,
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
                let Ok(scid) = self.conn_map.generate_scid_direct() else {
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
                let Ok(scid) = self.conn_map.generate_scid_direct() else {
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
                    pending_outbound.push(TxDatagram::new(
                        out[..len].to_vec(),
                        len,
                        peer,
                        // Retry/version-negotiation: pre-handshake, no per-
                        // connection PMTU yet — fall back to default cap.
                        None,
                    ));
                }
                self.buffer_pool.checkin(out);
                return;
            }
        };

        let recv_info = quiche::RecvInfo {
            from: peer,
            to: local,
        };

        let client_auth = self.http3_config.client_auth;
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
                // New capability (not a native port — see this method's
                // doc comment): enforce client-certificate-required when
                // configured, mirroring
                // `QuicServerHandler::process_inbound_packet` exactly.
                if client_auth.require_client_cert() && conn.quiche_conn.peer_cert().is_none() {
                    let _ = conn
                        .quiche_conn
                        .close(false, 0x0100, b"client certificate required");
                } else {
                    conn.handshake_complete_emitted = true;
                    batch.push(JsH3Event::handshake_complete(offset | (handle as u32)));
                }
            }

            let current_scid: Vec<u8> = conn.quiche_conn.source_id().into_owned().to_vec();
            let needs_dcid_update = current_scid.as_slice() != conn.conn_id.as_slice();
            if needs_dcid_update {
                conn.conn_id = current_scid.clone();
            }

            conn.poll_h3_events(offset | (handle as u32), app_event_budget, batch);
            for duration_ms in conn.poll_ping_acks() {
                batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
            }

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

        // Deliberately no `top_up_server_scids` call here — see this
        // method's doc comment (connection migration deferred).

        self.timer_heap
            .set_deadline(handle, timeout.map(|timeout| Instant::now() + timeout));
    }

    /// Expire due timers across every connection (the `hs_on_timeout`
    /// primitive) and complete any deferred graceful session closes whose
    /// deadline has passed. Hoisted from `ProtocolHandler::process_timers`
    /// below (which now just delegates here) — unlike packet routing,
    /// this has no `ring` dependency, so native and the direct-call/wasm
    /// surface share the exact same implementation.
    pub fn expire_timers(&mut self, now: Instant, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        self.last_expired = self.timer_heap.pop_expired(now);
        self.last_expired.sort_unstable();
        self.last_expired.dedup();
        for &handle in &self.last_expired {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.on_timeout();
                if conn.is_closed() {
                    reactor_metrics::record_lifecycle_trace(
                        "h3-server",
                        "session-close-timeout",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle={} blocked_streams={}",
                            offset | (handle as u32),
                            conn.blocked_set.len()
                        )),
                    );
                    reactor_metrics::record_session_close(SessionKind::H3Server);
                    batch.push(conn.session_close_event(offset | (handle as u32)));
                } else {
                    conn.poll_h3_events(offset | (handle as u32), app_event_budget, batch);
                    for duration_ms in conn.poll_ping_acks() {
                        batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
                    }
                    self.timer_heap
                        .set_deadline(handle, conn.timeout().map(|timeout| now + timeout));
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
            if let Some((error_code, reason, _)) = self.pending_session_closes.remove(&conn_handle)
            {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    reactor_metrics::record_lifecycle_trace(
                        "h3-server",
                        "close-session-deadline",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle={conn_handle} error_code={error_code} blocked_streams={} pending_graceful_closes={} reason={}",
                            conn.blocked_set.len(),
                            self.pending_session_closes.len(),
                            reason.as_str()
                        )),
                    );
                    let _ = conn
                        .quiche_conn
                        .close(true, u64::from(error_code), reason.as_bytes());
                }
            }
        }
    }

    /// Poll protocol/application events already buffered inside quiche for
    /// every connection (the `hs_drain_events` primitive's first sweep).
    /// Hoisted from `ProtocolHandler::poll_app_events` below.
    pub fn collect_app_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        if app_event_budget == 0 {
            return;
        }

        let offset = self.handle_offset;
        let mut remaining = app_event_budget;
        self.conn_map.fill_handles(&mut self.handles_buf);
        for i in 0..self.handles_buf.len() {
            if remaining == 0 {
                break;
            }

            let handle = self.handles_buf[i];
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.poll_h3_events(offset | (handle as u32), remaining, batch);
                for duration_ms in conn.poll_ping_acks() {
                    batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
                }
                remaining = app_event_budget.saturating_sub(batch.len());
            }
        }
    }

    /// Write every connection's next outbound datagram, round-robin (the
    /// `hs_next_send` primitive's refill step — a direct-call caller
    /// drains `outbound` one packet at a time via repeated calls, mirroring
    /// the native reactor's own per-tick `flush_sends` call). Hoisted from
    /// `ProtocolHandler::flush_sends` below.
    pub fn flush_all_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
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
                        let mtu = u16::try_from(conn.quiche_conn.max_send_udp_payload_size()).ok();
                        outbound.push(TxDatagram::new(tx_buf, len, send_info.to, mtu));
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
        let now = Instant::now();
        for &handle in &self.handles_buf {
            let timeout = self
                .conn_map
                .get_mut(handle)
                .and_then(|conn| conn.timeout());
            self.timer_heap
                .set_deadline(handle, timeout.map(|timeout| now + timeout));
        }
    }

    /// Retry buffered partial response/body writes where flow control has
    /// opened, pushing `drain` events (the `hs_drain_events` primitive's
    /// last sweep). Hoisted from `ProtocolHandler::flush_pending_writes`
    /// below.
    pub fn flush_all_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        let pending_response_keys = self.pending_responses.keys().copied().collect::<Vec<_>>();
        for key @ (conn_handle, stream_id) in pending_response_keys {
            let Some(mut response) = self.pending_responses.remove(&key) else {
                continue;
            };
            let before = response.queued_bytes();
            let before_units = response.queued_units();
            let event_conn_handle = offset | conn_handle;

            let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
                reactor_metrics::record_outbound_pending_write_removed(before);
                self.release_outbound_admission(before_units, batch);
                continue;
            };

            if conn.quiche_conn.stream_closed(stream_id) {
                reactor_metrics::record_outbound_pending_write_removed(before);
                self.release_outbound_admission(before_units, batch);
                continue;
            }

            match flush_one_h3_pending_response(conn, stream_id, &mut response) {
                Ok(outcome) if outcome.done => {
                    self.release_outbound_admission(outcome.released_units, batch);
                    reactor_metrics::record_outbound_pending_write_removed(before);
                    batch.push(JsH3Event::drain(event_conn_handle, stream_id));
                }
                Ok(outcome) => {
                    self.release_outbound_admission(outcome.released_units, batch);
                    reactor_metrics::record_outbound_pending_write_change(
                        before,
                        response.queued_bytes(),
                    );
                    self.pending_responses.insert(key, response);
                }
                Err(e) => {
                    reactor_metrics::record_outbound_pending_write_removed(before);
                    if self.outbound_admission.release(before_units) {
                        batch.push(JsH3Event::write_ready(0));
                    }
                    emit_h3_send_error_and_reset(
                        conn,
                        batch,
                        event_conn_handle,
                        stream_id,
                        "pending response flush",
                        &e,
                    );
                }
            }
        }

        let flushed = flush_pending_writes(
            &mut self.conn_map,
            &mut self.pending_writes,
            &mut self.pending_write_pool,
        );
        self.release_outbound_admission(flushed.released_units, batch);
        for (local_handle, stream_id) in flushed {
            batch.push(JsH3Event::drain(offset | local_handle, stream_id));
        }
    }

    /// Check blocked streams for writability, pushing `drain` events (the
    /// `hs_drain_events` primitive's middle sweep). Hoisted from
    /// `ProtocolHandler::poll_drain_events` below.
    pub fn collect_drain_events(&mut self, _app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
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

    /// Recycle TX buffers back into this handler's pool. Hoisted from
    /// `ProtocolHandler::recycle_tx_buffers` below (same name convention
    /// as `H3ClientHandler::recycle_tx_buffers_into_pool`).
    pub fn recycle_tx_buffers_into_pool(&mut self, buffers: Vec<Vec<u8>>) {
        reactor_metrics::record_tx_buffers_recycled(buffers.len());
        for buf in buffers {
            self.tx_pool.checkin(buf);
        }
    }

    /// Remove closed connections, pushing final `session_close` events
    /// (the "cleanup closed connections" direct-call operation). Hoisted
    /// from `ProtocolHandler::cleanup_closed` below.
    pub fn reap_closed_connections(&mut self, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        let closed = self.conn_map.drain_closed();
        for (handle, conn) in closed {
            self.timer_heap.remove_connection(handle);
            self.conn_send_buffers.remove(&handle);
            self.pending_session_closes.remove(&(handle as u32));
            // Audit finding #12: emit a reset for every abandoned stream
            // before dropping its PendingWrite, so the JS-side write
            // callback fires (via stream.destroy in _onReset) instead of
            // hanging waiting for a drain that will never come.
            let abandoned: Vec<u64> = self
                .pending_writes
                .keys()
                .filter(|&&(ch, _)| ch as usize == handle)
                .map(|&(_, sid)| sid)
                .chain(
                    self.pending_responses
                        .keys()
                        .filter(|&&(ch, _)| ch as usize == handle)
                        .map(|&(_, sid)| sid),
                )
                .collect();
            let mut abandoned = abandoned;
            abandoned.sort_unstable();
            abandoned.dedup();
            for stream_id in abandoned {
                batch.push(JsH3Event::reset(
                    offset | (handle as u32),
                    stream_id,
                    0, // H3_NO_ERROR — connection-level close, not stream-level reset
                ));
            }
            let mut removed_bytes = 0;
            let mut removed_units = 0;
            self.pending_writes.retain(|&(ch, _), write| {
                let keep = ch as usize != handle;
                if !keep {
                    removed_bytes += write.queued_bytes();
                    removed_units += write.queued_units();
                }
                keep
            });
            self.pending_responses.retain(|&(ch, _), response| {
                let keep = ch as usize != handle;
                if !keep {
                    removed_bytes += response.queued_bytes();
                    removed_units += response.queued_units();
                }
                keep
            });
            reactor_metrics::record_outbound_pending_write_removed(removed_bytes);
            self.release_outbound_admission(removed_units, batch);
            if !self.last_expired.contains(&handle) {
                reactor_metrics::record_lifecycle_trace(
                    "h3-server",
                    "session-close-cleanup",
                    None,
                    None,
                    None,
                    Some(format!(
                        "conn_handle={} pending_graceful_closes={}",
                        offset | (handle as u32),
                        self.pending_session_closes.len()
                    )),
                );
                reactor_metrics::record_session_close(SessionKind::H3Server);
                batch.push(conn.session_close_event(offset | (handle as u32)));
            }
        }
        self.last_expired.clear();
    }

    // ── Per-connection direct-call operations ───────────────────────
    //
    // Every method below is a hoisted, `batch`/return-value-based
    // equivalent of the corresponding `WorkerCommand` match arm in
    // `dispatch_command` above (the native command-dispatch path is left
    // completely unchanged; these are new, additional entry points for a
    // direct-call/wasm caller who has no command channel to send
    // through). Unlike `dispatch_command`'s match arms, these do **not**
    // call `release_outbound_admission` / emit `EVENT_WRITE_READY` —
    // that gauge exists solely to bound in-flight *cross-thread command
    // queue* depth between JS and the native worker thread (A2 task 2:
    // "the ack-gauge RX-pause machinery is bypassed — JS is synchronous
    // with the core"); a direct-call caller has no such queue, and reads
    // backpressure directly off each method's returned unit count instead
    // (mirroring `H3ClientHandler::queue_stream_send`'s identical
    // convention).

    /// Send response headers for `stream_id` on `conn_handle` (the
    /// `hs_send_response_headers` primitive). On `StreamBlocked`/`Done`,
    /// buffers the headers to retry via `flush_all_pending_writes` and
    /// returns an `Err` whose message contains `StreamBlocked` (same
    /// substring convention as `H3ClientHandler::send_request`).
    pub fn send_response_headers(
        &mut self,
        conn_handle: u32,
        stream_id: u64,
        headers: Vec<(String, String)>,
        fin: bool,
        batch: &mut Vec<JsH3Event>,
    ) -> Result<(), String> {
        let event_conn_handle = self.handle_offset | conn_handle;
        let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
            return Err(format!("no such connection: {conn_handle}"));
        };
        let h3_headers = h3_headers(&headers);
        match conn.send_response(stream_id, &h3_headers, fin) {
            Ok(()) => Ok(()),
            Err(e) if is_h3_stream_blocked(&e) => {
                insert_pending_response(
                    &mut self.pending_responses,
                    (conn_handle, stream_id),
                    PendingResponse::headers_only(headers, fin),
                );
                batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                Err(format!("send_response_headers failed: StreamBlocked ({e})"))
            }
            Err(e) => {
                emit_h3_send_error_and_reset(
                    conn,
                    batch,
                    event_conn_handle,
                    stream_id,
                    "send_response_headers",
                    &e,
                );
                Err(format!("send_response_headers failed: {e}"))
            }
        }
    }

    /// Queue body bytes (and optionally FIN) for `stream_id` on
    /// `conn_handle` (the `hs_stream_send` primitive) — returns admitted
    /// bytes (FIN-only accept = 1 per the outbound-units convention), or
    /// `0` if fully backpressured / the connection no longer exists.
    /// Mirrors `H3ClientHandler::queue_stream_send` exactly, keyed by
    /// `(conn_handle, stream_id)` instead of just `stream_id`, and checks
    /// `pending_responses` first (a server-only concept: a response whose
    /// headers themselves are still blocked).
    pub fn queue_stream_send(
        &mut self,
        conn_handle: u32,
        stream_id: u64,
        chunk: Chunk,
        fin: bool,
        batch: &mut Vec<JsH3Event>,
    ) -> usize {
        let event_conn_handle = self.handle_offset | conn_handle;
        let key = (conn_handle, stream_id);
        if let Some(response) = self.pending_responses.get_mut(&key) {
            response.push_body_chunk(chunk, fin);
            return 0;
        }
        if let Some(pw) = self.pending_writes.get_mut(&key) {
            reactor_metrics::record_outbound_pending_write_added(pw.push_chunk(chunk));
            if fin {
                pw.set_fin();
            }
            return 0;
        }
        let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
            return 0;
        };
        let outbound_units = outbound_payload_units(chunk.remaining_len(), fin);
        let payload_len = chunk.remaining_len();
        match conn.send_body_chunk(stream_id, chunk, fin) {
            Ok(outcome) => {
                let released_units = accepted_outbound_payload_units(
                    payload_len,
                    fin,
                    outcome.written,
                    outcome.fin_accepted,
                );
                if let Some(remainder) = outcome.remainder {
                    insert_pending_write(
                        &mut self.pending_writes,
                        key,
                        PendingWrite::new(remainder, fin),
                    );
                    batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                }
                released_units
            }
            Err(e) => {
                emit_h3_send_error_and_reset(
                    conn,
                    batch,
                    event_conn_handle,
                    stream_id,
                    "stream send",
                    &e,
                );
                outbound_units
            }
        }
    }

    /// Send trailers for `stream_id` on `conn_handle`.
    pub fn send_trailers(
        &mut self,
        conn_handle: u32,
        stream_id: u64,
        headers: &[(String, String)],
    ) -> Result<(), String> {
        let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
            return Err(format!("no such connection: {conn_handle}"));
        };
        let h3_headers: Vec<quiche::h3::Header> = headers
            .iter()
            .map(|(n, v)| quiche::h3::Header::new(n.as_bytes(), v.as_bytes()))
            .collect();
        conn.send_trailers(stream_id, &h3_headers)
            .map_err(|e| format!("send_trailers failed: {e}"))
    }

    /// Close `stream_id` on `conn_handle` with `error_code` (the
    /// `hs_stream_close` primitive) — returns the number of released
    /// outbound-admission units from any abandoned pending write/response
    /// (for a caller that wants to track its own backpressure gauge; the
    /// direct-call surface itself doesn't require this).
    pub fn close_stream(
        &mut self,
        conn_handle: u32,
        stream_id: u64,
        error_code: u32,
        batch: &mut Vec<JsH3Event>,
    ) -> usize {
        let event_conn_handle = self.handle_offset | conn_handle;
        let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
            return 0;
        };
        match conn.stream_close(stream_id, u64::from(error_code)) {
            Ok(()) => {
                let released = remove_pending_response(
                    &mut self.pending_responses,
                    &(conn_handle, stream_id),
                ) + remove_pending_write(&mut self.pending_writes, &(conn_handle, stream_id));
                if released > 0 {
                    batch.push(JsH3Event::reset(
                        event_conn_handle,
                        stream_id,
                        u64::from(error_code),
                    ));
                }
                released
            }
            Err(e) => {
                log::debug!(
                    "stream_close failed conn_handle={conn_handle} stream_id={stream_id} error_code={error_code}: {e}"
                );
                0
            }
        }
    }

    /// Send a QUIC DATAGRAM (RFC 9221) on `conn_handle`.
    pub fn send_datagram(&mut self, conn_handle: u32, data: Chunk) -> bool {
        self.conn_map
            .get_mut(conn_handle as usize)
            .is_some_and(|conn| conn.send_datagram_chunk(data).is_ok())
    }

    /// Snapshot session metrics for `conn_handle`, or `None` if it no
    /// longer exists (the `hs_session_metrics` primitive).
    pub fn session_metrics(&self, conn_handle: u32) -> Option<JsSessionMetrics> {
        self.conn_map.get(conn_handle as usize).map(snapshot_metrics)
    }

    /// Remote QPACK/H3 SETTINGS for `conn_handle` (the
    /// `hs_remote_settings` primitive).
    pub fn remote_settings(&self, conn_handle: u32) -> Vec<(u64, u64)> {
        self.conn_map
            .get(conn_handle as usize)
            .map_or_else(Vec::new, H3Connection::remote_settings)
    }

    /// Queue a PING on `conn_handle`.
    pub fn ping(&mut self, conn_handle: u32) -> bool {
        self.conn_map
            .get_mut(conn_handle as usize)
            .is_some_and(|conn| conn.queue_ping().is_ok())
    }

    /// Gracefully close a single connection (the `hs_close_connection`
    /// primitive): send GOAWAY and defer the actual CONNECTION_CLOSE by
    /// 25ms so in-flight responses have a chance to land — mirrors
    /// `WorkerCommand::CloseSession`'s match arm exactly.
    /// `expire_timers`/`process_inbound_packet`'s callers must keep
    /// pumping timers for the deferred close to actually complete (same
    /// as native).
    pub fn close_connection(&mut self, conn_handle: u32, error_code: u32, reason: String) {
        let Some(conn) = self.conn_map.get_mut(conn_handle as usize) else {
            return;
        };
        if conn.send_goaway().is_ok() {
            self.pending_session_closes.insert(
                conn_handle,
                (error_code, reason, Instant::now() + Duration::from_millis(25)),
            );
        } else {
            let _ = conn
                .quiche_conn
                .close(true, u64::from(error_code), reason.as_bytes());
        }
    }
}

#[cfg(feature = "os-runtime")]
impl ProtocolHandler for H3ServerHandler {
    type Command = WorkerCommand;

    #[allow(clippy::too_many_lines)]
    fn dispatch_command(&mut self, cmd: WorkerCommand, batch: &mut Vec<JsH3Event>) -> bool {
        let outbound_units = worker_command_outbound_bytes(&cmd);
        reactor_metrics::record_outbound_command_dequeued(outbound_units);
        match cmd {
            WorkerCommand::Shutdown => {
                // Audit finding #11: close every live connection with an
                // application-level CONNECTION_CLOSE so peers see a graceful
                // shutdown instead of waiting on idle timeout. The event
                // loop's exit-flush will push the CLOSE frames.
                let mut handles = Vec::new();
                self.conn_map.fill_handles(&mut handles);
                for handle in handles {
                    if let Some(conn) = self.conn_map.get_mut(handle) {
                        if !conn.quiche_conn.is_closed() && !conn.quiche_conn.is_draining() {
                            let _ = conn.quiche_conn.close(true, 0, b"server shutdown");
                        }
                    }
                }
                return true;
            }
            WorkerCommand::SendResponseHeaders {
                conn_handle,
                stream_id,
                headers,
                fin,
            } => {
                let event_conn_handle = self.handle_offset | conn_handle;
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let h3_headers = h3_headers(&headers);
                    // Audit finding #3: surface failures to JS instead of
                    // silently swallowing. Without this, `Done`/`StreamBlocked`
                    // leaves the stream with no headers ever on the wire,
                    // and subsequent body writes fail with FrameUnexpected
                    // because quiche's H3 framer requires headers first.
                    if let Err(e) = conn.send_response(stream_id, &h3_headers, fin) {
                        if is_h3_stream_blocked(&e) {
                            insert_pending_response(
                                &mut self.pending_responses,
                                (conn_handle, stream_id),
                                PendingResponse::headers_only(headers, fin),
                            );
                            batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                        } else {
                            emit_h3_send_error_and_reset(
                                conn,
                                batch,
                                event_conn_handle,
                                stream_id,
                                "send_response_headers",
                                &e,
                            );
                        }
                    }
                }
            }
            WorkerCommand::SendResponse {
                conn_handle,
                stream_id,
                headers,
                body,
                fin,
            } => {
                let event_conn_handle = self.handle_offset | conn_handle;
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    // Step 1: send response headers (never with FIN — body follows)
                    let h3_headers = h3_headers(&headers);
                    // Audit finding #3: don't call send_body if send_response
                    // failed — that would emit DATA frames on a stream with
                    // no HEADERS, an H3 protocol violation. Surface the
                    // failure to JS so the caller can retry the request.
                    if let Err(e) = conn.send_response(stream_id, &h3_headers, false) {
                        if is_h3_stream_blocked(&e) {
                            insert_pending_response(
                                &mut self.pending_responses,
                                (conn_handle, stream_id),
                                PendingResponse::with_body(headers, body, fin),
                            );
                            batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                        } else {
                            if self.outbound_admission.release(outbound_units) {
                                batch.push(JsH3Event::write_ready(0));
                            }
                            emit_h3_send_error_and_reset(
                                conn,
                                batch,
                                event_conn_handle,
                                stream_id,
                                "send_response",
                                &e,
                            );
                        }
                        return false;
                    }
                    // Step 2: send body + FIN
                    let key = (conn_handle, stream_id);
                    let payload_len = body.remaining_len();
                    match conn.send_body_chunk(stream_id, body, fin) {
                        Ok(outcome) => {
                            self.release_outbound_admission(
                                accepted_outbound_payload_units(
                                    payload_len,
                                    fin,
                                    outcome.written,
                                    outcome.fin_accepted,
                                ),
                                batch,
                            );
                            if let Some(remainder) = outcome.remainder {
                                insert_pending_write(
                                    &mut self.pending_writes,
                                    key,
                                    PendingWrite::new(remainder, fin),
                                );
                                batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                            }
                        }
                        Err(e) => {
                            if self.outbound_admission.release(outbound_units) {
                                batch.push(JsH3Event::write_ready(0));
                            }
                            emit_h3_send_error_and_reset(
                                conn,
                                batch,
                                event_conn_handle,
                                stream_id,
                                "stream send",
                                &e,
                            );
                        }
                    }
                } else {
                    self.release_outbound_admission(outbound_units, batch);
                }
            }
            WorkerCommand::StreamSend {
                conn_handle,
                stream_id,
                chunk,
                fin,
            } => {
                let event_conn_handle = self.handle_offset | conn_handle;
                let key = (conn_handle, stream_id);
                if let Some(response) = self.pending_responses.get_mut(&key) {
                    response.push_body_chunk(chunk, fin);
                } else if let Some(pw) = self.pending_writes.get_mut(&key) {
                    reactor_metrics::record_outbound_pending_write_added(pw.push_chunk(chunk));
                    if fin {
                        pw.set_fin();
                    }
                    // chunk backing allocation stays queued via ArcBuf.
                } else if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let payload_len = chunk.remaining_len();
                    match conn.send_body_chunk(stream_id, chunk, fin) {
                        Ok(outcome) => {
                            self.release_outbound_admission(
                                accepted_outbound_payload_units(
                                    payload_len,
                                    fin,
                                    outcome.written,
                                    outcome.fin_accepted,
                                ),
                                batch,
                            );
                            if let Some(remainder) = outcome.remainder {
                                insert_pending_write(
                                    &mut self.pending_writes,
                                    key,
                                    PendingWrite::new(remainder, fin),
                                );
                                batch.push(JsH3Event::stream_blocked(event_conn_handle, stream_id));
                            }
                        }
                        Err(e) => {
                            if self.outbound_admission.release(outbound_units) {
                                batch.push(JsH3Event::write_ready(0));
                            }
                            emit_h3_send_error_and_reset(
                                conn,
                                batch,
                                event_conn_handle,
                                stream_id,
                                "stream send",
                                &e,
                            );
                        }
                    }
                } else {
                    self.release_outbound_admission(outbound_units, batch);
                }
            }
            WorkerCommand::StreamClose {
                conn_handle,
                stream_id,
                error_code,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    match conn.stream_close(stream_id, u64::from(error_code)) {
                        Ok(()) => {
                            let released = remove_pending_response(
                                &mut self.pending_responses,
                                &(conn_handle, stream_id),
                            ) + remove_pending_write(
                                &mut self.pending_writes,
                                &(conn_handle, stream_id),
                            );
                            self.release_outbound_admission(released, batch);
                            if released > 0 {
                                batch.push(JsH3Event::reset(
                                    conn_handle,
                                    stream_id,
                                    u64::from(error_code),
                                ));
                            }
                        }
                        Err(e) => {
                            log::debug!(
                                "stream_close failed conn_handle={conn_handle} stream_id={stream_id} error_code={error_code}: {e}"
                            );
                        }
                    }
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
                    if let Err(e) = conn.send_trailers(stream_id, &h3_headers) {
                        log::debug!(
                            "send_trailers failed conn_handle={conn_handle} stream_id={stream_id}: {e}"
                        );
                    }
                }
            }
            WorkerCommand::CloseSession {
                conn_handle,
                error_code,
                reason,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    reactor_metrics::record_lifecycle_trace(
                        "h3-server",
                        "close-session-requested",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle={conn_handle} error_code={error_code} blocked_streams={} pending_graceful_closes={} reason={}",
                            conn.blocked_set.len(),
                            self.pending_session_closes.len(),
                            reason.as_str()
                        )),
                    );
                    if conn.send_goaway().is_ok() {
                        self.pending_session_closes.insert(
                            conn_handle,
                            (
                                error_code,
                                reason,
                                Instant::now() + Duration::from_millis(25),
                            ),
                        );
                        reactor_metrics::record_lifecycle_trace(
                            "h3-server",
                            "close-session-deferred",
                            None,
                            None,
                            None,
                            Some(format!(
                                "conn_handle={conn_handle} error_code={error_code} blocked_streams={} pending_graceful_closes={}",
                                conn.blocked_set.len(),
                                self.pending_session_closes.len()
                            )),
                        );
                    } else {
                        reactor_metrics::record_lifecycle_trace(
                            "h3-server",
                            "close-session-direct",
                            None,
                            None,
                            None,
                            Some(format!(
                                "conn_handle={conn_handle} error_code={error_code} blocked_streams={} reason={}",
                                conn.blocked_set.len(),
                                reason.as_str()
                            )),
                        );
                        let _ =
                            conn.quiche_conn
                                .close(true, u64::from(error_code), reason.as_bytes());
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
                    .is_some_and(|conn| conn.send_datagram_chunk(data).is_ok());
                self.release_outbound_admission(outbound_units, batch);
                let _ = resp_tx.send(ok);
            }
            WorkerCommand::GetSessionMetrics {
                conn_handle,
                resp_tx,
            } => {
                let metrics = self
                    .conn_map
                    .get(conn_handle as usize)
                    .map(snapshot_metrics);
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
                    conn.queue_ping().is_ok()
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
        app_event_budget: usize,
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
                    pending_outbound.push(TxDatagram::new(
                        out[..len].to_vec(),
                        len,
                        peer,
                        // Retry/version-negotiation: pre-handshake, no per-
                        // connection PMTU yet — fall back to default cap.
                        None,
                    ));
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

            conn.poll_h3_events(offset | (handle as u32), app_event_budget, batch);
            for duration_ms in conn.poll_ping_acks() {
                batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
            }

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

        self.timer_heap
            .set_deadline(handle, timeout.map(|timeout| Instant::now() + timeout));
    }

    fn process_timers(
        &mut self,
        now: Instant,
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
    ) {
        self.expire_timers(now, app_event_budget, batch);
    }

    fn poll_app_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        self.collect_app_events(app_event_budget, batch);
    }

    fn flush_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
        self.flush_all_sends(outbound);
    }

    fn flush_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        self.flush_all_pending_writes(batch);
    }

    fn poll_drain_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        self.collect_drain_events(app_event_budget, batch);
    }

    fn drain_recycled_buffers(&mut self) {
        self.conn_map.fill_handles(&mut self.handles_buf);
        for &handle in &self.handles_buf {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.drain_recycled();
            }
        }
        self.chunk_pool.drain_returned(&self.chunk_pool_rx);
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        self.recycle_tx_buffers_into_pool(buffers);
    }

    fn cleanup_closed(&mut self, batch: &mut Vec<JsH3Event>) {
        self.reap_closed_connections(batch);
    }

    fn emit_session_close_for_all_active(&mut self, batch: &mut Vec<JsH3Event>) {
        // Audit finding #34: on driver runtime error we exit without going
        // through cleanup_closed, so emit a session_close per active
        // connection here. JS-side sessions otherwise sit "open" forever.
        let mut handles = Vec::new();
        self.conn_map.fill_handles(&mut handles);
        let offset = self.handle_offset;
        for handle in handles {
            batch.push(JsH3Event::session_close(offset | (handle as u32)));
        }
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        self.soonest_deadline()
    }
}

// ── H3 Client Protocol Handler ──────────────────────────────────────

/// HTTP/3 client protocol state machine: a single connection's worth of
/// quiche + QPACK state plus pending-write/chunk-pool bookkeeping.
///
/// Kept always-compiled (no `os-runtime` gate) and `pub` (re-exported by
/// `wasm_exports` under `wasm-abi`): every method is a plain `&mut self`
/// call with zero socket/thread dependency — the sans-IO seam this crate is
/// built around. Only [`H3ClientHandler::new`] (the ring-backed SCID native
/// constructor) is `os-runtime`-gated; [`H3ClientHandler::new_direct`] is
/// the always-compiled alternative that takes caller-supplied SCID bytes.
pub struct H3ClientHandler {
    conn: H3Connection,
    pending_writes: HashMap<u64, PendingWrite>,
    send_buf: Vec<u8>,
    tx_pool: BufferPool,
    pending_write_pool: AdaptiveBufferPool,
    timer_deadline: Option<Instant>,
    session_closed_emitted: bool,
    chunk_pool: ChunkPool,
    chunk_pool_rx: crossbeam_channel::Receiver<Vec<u8>>,
    outbound_admission: Arc<OutboundAdmission>,
    keylog: Option<KeylogBuffer>,
}

impl H3ClientHandler {
    /// Native constructor: sources the SCID from `ring`'s system RNG.
    /// Requires `os-runtime` — `ring` is an optional dependency gated
    /// behind it (A1 task 1) — but every native call site already runs
    /// under `os-runtime` (it is in the default feature set), so this is
    /// not a behavior change.
    #[cfg(feature = "os-runtime")]
    #[allow(clippy::too_many_arguments)]
    fn new(
        local_addr: SocketAddr,
        server_addr: SocketAddr,
        server_name: &str,
        session_ticket: Option<&[u8]>,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        quiche_config: &mut quiche::Config,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Option<Self> {
        let Ok(scid) = crate::cid::CidEncoding::random().generate_scid() else {
            return None;
        };
        Self::from_scid(
            scid,
            local_addr,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            quiche_config,
            outbound_admission,
        )
    }

    /// Direct-call constructor for a sans-IO caller (a future wasm ABI, or
    /// the unit tests below): the caller supplies the 20-byte SCID
    /// directly (e.g. sourced from a JS host RNG) instead of requiring
    /// `ring`. No `Driver`/socket involved — same as `new`, this only
    /// touches `local_addr`/`server_addr` as plain `SocketAddr` values.
    #[allow(clippy::too_many_arguments)]
    pub fn new_direct(
        scid: Vec<u8>,
        local_addr: SocketAddr,
        server_addr: SocketAddr,
        server_name: &str,
        session_ticket: Option<&[u8]>,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        quiche_config: &mut quiche::Config,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Option<Self> {
        Self::from_scid(
            scid,
            local_addr,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            quiche_config,
            outbound_admission,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_scid(
        scid: Vec<u8>,
        local_addr: SocketAddr,
        server_addr: SocketAddr,
        server_name: &str,
        session_ticket: Option<&[u8]>,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        quiche_config: &mut quiche::Config,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Option<Self> {
        let scid_ref = quiche::ConnectionId::from_ref(&scid);
        let Ok(mut quiche_conn) = quiche::connect_with_buffer_factory::<ArcBufFactory>(
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
        let (chunk_pool, _chunk_pool_return, chunk_pool_rx) = ChunkPool::with_return_channel(64);
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            send_buf: vec![0u8; SEND_BUF_SIZE],
            tx_pool: BufferPool::new(256, 65535),
            pending_write_pool: AdaptiveBufferPool::new(
                PENDING_WRITE_POOL_SIZE,
                PENDING_WRITE_MIN_CAPACITY,
            ),
            timer_deadline,
            session_closed_emitted: false,
            chunk_pool,
            chunk_pool_rx,
            outbound_admission,
            keylog: None,
        })
    }

    /// Enable in-memory NSS-format keylog capture (A2 task 5). Additive:
    /// native file-based keylog (`Http3Config`'s `keylog` option +
    /// `quiche::Config::log_keys`) is unaffected — this installs a second,
    /// independent sink via `quiche::Connection::set_keylog` so a caller
    /// with no filesystem (e.g. a future wasm build) can still capture
    /// keys. Call once, before the handshake starts producing secrets.
    pub fn enable_keylog(&mut self) {
        let buffer = KeylogBuffer::new();
        self.conn.quiche_conn.set_keylog(buffer.writer());
        self.keylog = Some(buffer);
    }

    /// Drain accumulated NSS-format keylog lines captured since the last
    /// call (or since `enable_keylog`). Empty when keylog isn't enabled.
    pub fn take_keylog_lines(&mut self) -> Vec<u8> {
        self.keylog.as_ref().map(KeylogBuffer::take).unwrap_or_default()
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

    /// Request a graceful close (the "close" direct-call operation).
    pub fn close_session(&mut self, error_code: u32, reason: &str) {
        reactor_metrics::record_lifecycle_trace(
            "h3-client",
            "close-session-requested",
            None,
            None,
            None,
            Some(format!(
                "conn_handle=0 error_code={error_code} pending_writes={} blocked_streams={} reason={reason}",
                self.pending_writes.len(),
                self.conn.blocked_set.len()
            )),
        );
        let _ = self
            .conn
            .quiche_conn
            .close(true, u64::from(error_code), reason.as_bytes());
    }

    fn refresh_timeout_deadline(&mut self) {
        self.timer_deadline = self.conn.timeout().map(|timeout| Instant::now() + timeout);
    }

    pub fn send_request(
        &mut self,
        headers: Vec<(String, String)>,
        fin: bool,
    ) -> Result<u64, String> {
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

    pub fn queue_stream_send(
        &mut self,
        stream_id: u64,
        chunk: Chunk,
        fin: bool,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) -> usize {
        if let Some(pw) = self.pending_writes.get_mut(&stream_id) {
            reactor_metrics::record_outbound_pending_write_added(pw.push_chunk(chunk));
            if fin {
                pw.set_fin();
            }
            return 0;
        }

        let outbound_units = outbound_payload_units(chunk.remaining_len(), fin);
        let payload_len = chunk.remaining_len();
        match self.conn.send_body_chunk(stream_id, chunk, fin) {
            Ok(outcome) => {
                let released_units = accepted_outbound_payload_units(
                    payload_len,
                    fin,
                    outcome.written,
                    outcome.fin_accepted,
                );
                if let Some(remainder) = outcome.remainder {
                    insert_pending_write(
                        &mut self.pending_writes,
                        stream_id,
                        PendingWrite::new(remainder, fin),
                    );
                    batch.push(JsH3Event::stream_blocked(conn_handle, stream_id));
                }
                // else: full write — chunk drops → recycles to pool
                released_units
            }
            Err(e) => {
                batch.push(JsH3Event::error(
                    conn_handle,
                    stream_id as i64,
                    0,
                    format!("stream send failed: {e}"),
                ));
                outbound_units
            }
        }
    }

    pub fn close_stream(&mut self, stream_id: u64, error_code: u32) -> usize {
        match self.conn.stream_close(stream_id, u64::from(error_code)) {
            Ok(()) => remove_pending_write(&mut self.pending_writes, &stream_id),
            Err(e) => {
                log::debug!(
                    "client stream_close failed stream_id={stream_id} error_code={error_code}: {e}"
                );
                0
            }
        }
    }

    pub fn send_datagram(&mut self, data: Chunk) -> bool {
        self.conn.send_datagram_chunk(data).is_ok()
    }

    pub fn metrics_snapshot(&self) -> JsSessionMetrics {
        snapshot_metrics(&self.conn)
    }

    pub fn remote_settings(&self) -> Vec<(u64, u64)> {
        self.conn.remote_settings()
    }

    pub fn ping(&mut self) -> bool {
        self.conn.queue_ping().is_ok()
    }

    pub fn qlog_path(&self) -> Option<String> {
        self.conn.qlog_path.clone()
    }

    fn release_outbound_admission(&self, units: usize, batch: &mut Vec<JsH3Event>) {
        if self.outbound_admission.release(units) {
            batch.push(JsH3Event::write_ready(0));
        }
    }

    fn emit_session_close(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        if self.session_closed_emitted {
            return;
        }
        reactor_metrics::record_lifecycle_trace(
            "h3-client",
            "session-close-emitted",
            None,
            None,
            None,
            Some(format!(
                "conn_handle={conn_handle} pending_writes={} blocked_streams={}",
                self.pending_writes.len(),
                self.conn.blocked_set.len()
            )),
        );
        // Audit findings #12 and #35: drain abandoned pending writes here
        // so JS-side stream callbacks fire (via _onReset → destroy) and
        // is_reapable() can return true without waiting on dead-state
        // pending_writes that will never flush.
        for stream_id in self.pending_writes.keys().copied().collect::<Vec<_>>() {
            batch.push(JsH3Event::reset(conn_handle, stream_id, 0));
        }
        let (removed_bytes, removed_units): (usize, usize) = self
            .pending_writes
            .values()
            .fold((0, 0), |(bytes, units), write| {
                (bytes + write.queued_bytes(), units + write.queued_units())
            });
        self.pending_writes.clear();
        reactor_metrics::record_outbound_pending_write_removed(removed_bytes);
        self.release_outbound_admission(removed_units, batch);
        reactor_metrics::record_session_close(SessionKind::H3Client);
        batch.push(self.conn.session_close_event(conn_handle));
        self.session_closed_emitted = true;
    }

    pub fn process_packet_for_handle(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        app_event_budget: usize,
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
        self.conn
            .poll_h3_events(conn_handle, app_event_budget, batch);
        for duration_ms in self.conn.poll_ping_acks() {
            batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
        }
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
        self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);

        if self.conn.is_closed() {
            self.emit_session_close(batch, conn_handle);
        }
    }

    pub fn process_timers_for_handle(
        &mut self,
        now: Instant,
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) {
        if self.timer_deadline.is_some_and(|deadline| deadline <= now) {
            self.conn.on_timeout();
            if self.conn.is_closed() {
                self.emit_session_close(batch, conn_handle);
            } else {
                self.conn
                    .poll_h3_events(conn_handle, app_event_budget, batch);
                for duration_ms in self.conn.poll_ping_acks() {
                    batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
                }
                if let Some(ticket) = self.conn.update_session_ticket() {
                    batch.push(JsH3Event::session_ticket(conn_handle, ticket));
                }
                self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);
            }
        }
    }

    pub fn poll_app_events_for_handle(
        &mut self,
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) {
        if app_event_budget == 0 {
            return;
        }

        self.conn
            .poll_h3_events(conn_handle, app_event_budget, batch);
        for duration_ms in self.conn.poll_ping_acks() {
            batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
        }
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
    }

    /// Write the next outbound datagram, if any (the `flush_sends`
    /// primitive, one packet at a time — a direct-call caller loops on
    /// this until it returns `None`, mirroring the native
    /// `ProtocolHandler::flush_sends` drain loop below).
    pub fn try_send_next(&mut self) -> Option<TxDatagram> {
        let result = Self::try_send_next_with_pool_parts(
            &mut self.conn,
            self.send_buf.as_mut_slice(),
            &mut self.tx_pool,
        );
        if result.is_none() {
            // Deviation (Phase 3 wasm-plan discovery, docs/WASM_CLIENT_PLAN.md):
            // mirrors ProtocolHandler::flush_sends's own post-loop
            // self.refresh_timeout_deadline() call (used by the native
            // reactor loop) for the direct-call surface — wasm's
            // h3c_next_send wraps try_send_next directly, and a
            // direct-call caller loops on it until None exactly like
            // flush_sends's own loop does. Without this,
            // `timer_deadline` (what h3c_timeout_ms/process_timers_for_handle
            // read) stays stale until it happens to naturally elapse on
            // its own — e.g. right after h3c_close, it is still whatever
            // it was *before* the close request (commonly the idle
            // timeout, tens of seconds out), so h3c_on_timeout can never
            // observe the freshly-shortened post-close draining timer.
            // Refreshing only on the "nothing left to send" (None) return
            // matches flush_sends's once-per-flush-cycle timing, rather
            // than refreshing redundantly on every packet.
            self.refresh_timeout_deadline();
        }
        result
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
        let mtu = u16::try_from(conn.quiche_conn.max_send_udp_payload_size()).ok();
        Some(TxDatagram::new(tx_buf, len, send_info.to, mtu))
    }

    pub fn flush_pending_writes_for_handle(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        let flushed = flush_client_pending_writes(
            &mut self.conn,
            &mut self.pending_writes,
            &mut self.pending_write_pool,
        );
        self.release_outbound_admission(flushed.released_units, batch);
        for stream_id in flushed {
            batch.push(JsH3Event::drain(conn_handle, stream_id));
        }
    }

    pub fn poll_drain_events_for_handle(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        if !self.conn.blocked_set.is_empty() {
            self.conn.poll_drain_events(conn_handle, batch);
        }
    }

    pub fn recycle_tx_buffers_into_pool(&mut self, buffers: Vec<Vec<u8>>) {
        reactor_metrics::record_tx_buffers_recycled(buffers.len());
        for buf in buffers {
            self.tx_pool.checkin(buf);
        }
    }

    pub fn is_reapable(&self) -> bool {
        // Audit finding #35: once we've emitted session_close, the connection
        // is terminal. emit_session_close drains pending_writes so this is
        // already a tautology, but the explicit check guards against any
        // future code path that could leave entries behind.
        self.session_closed_emitted
    }

    /// Soonest quiche timeout deadline, or `None` (the "next timeout"
    /// direct-call operation — hoisted out of the `ProtocolHandler` impl
    /// below so a caller without the trait in scope can still read it).
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timer_deadline
    }
}

impl ProtocolHandler for H3ClientHandler {
    type Command = ClientWorkerCommand;

    fn dispatch_command(&mut self, cmd: ClientWorkerCommand, batch: &mut Vec<JsH3Event>) -> bool {
        let outbound_units = client_worker_command_outbound_bytes(&cmd);
        reactor_metrics::record_outbound_command_dequeued(outbound_units);
        match cmd {
            ClientWorkerCommand::Shutdown => {
                if !self.session_closed_emitted {
                    reactor_metrics::record_lifecycle_trace(
                        "h3-client",
                        "shutdown-command",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle=0 pending_writes={} blocked_streams={}",
                            self.pending_writes.len(),
                            self.conn.blocked_set.len()
                        )),
                    );
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
                chunk,
                fin,
            } => {
                let released = self.queue_stream_send(stream_id, chunk, fin, batch, 0);
                self.release_outbound_admission(released, batch);
            }
            ClientWorkerCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                let released = self.close_stream(stream_id, error_code);
                self.release_outbound_admission(released, batch);
            }
            ClientWorkerCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.send_datagram(data));
                self.release_outbound_admission(outbound_units, batch);
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
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
    ) {
        self.process_packet_for_handle(buf, peer, local, app_event_budget, batch, 0);
    }

    fn process_timers(
        &mut self,
        now: Instant,
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
    ) {
        self.process_timers_for_handle(now, app_event_budget, batch, 0);
    }

    fn poll_app_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        self.poll_app_events_for_handle(app_event_budget, batch, 0);
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

    fn poll_drain_events(&mut self, _app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        self.poll_drain_events_for_handle(batch, 0);
    }

    fn drain_recycled_buffers(&mut self) {
        self.conn.drain_recycled();
        self.chunk_pool.drain_returned(&self.chunk_pool_rx);
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        self.recycle_tx_buffers_into_pool(buffers);
    }

    fn cleanup_closed(&mut self, _batch: &mut Vec<JsH3Event>) {
        // Client session_close is emitted in process_packet / process_timers.
    }

    fn emit_session_close_for_all_active(&mut self, batch: &mut Vec<JsH3Event>) {
        // Audit finding #34: H3 client single-session — emit the close so
        // JS doesn't sit waiting on an open session forever.
        if !self.session_closed_emitted {
            batch.push(JsH3Event::session_close(0));
            self.session_closed_emitted = true;
        }
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        self.next_timer_deadline()
    }

    fn is_done(&self) -> bool {
        self.session_closed_emitted
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
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
        datagram_queue_depth: 0,
    }
}

/// Flush buffered partial writes for all streams.
struct PendingWriteFlushEvents<T> {
    drained: Vec<T>,
    released_units: usize,
}

impl<T> IntoIterator for PendingWriteFlushEvents<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.drained.into_iter()
    }
}

struct PendingResponseFlushOutcome {
    done: bool,
    released_units: usize,
}

fn flush_one_h3_pending_response(
    conn: &mut H3Connection,
    stream_id: u64,
    response: &mut PendingResponse,
) -> Result<PendingResponseFlushOutcome, Http3NativeError> {
    if !response.headers_sent {
        let headers = h3_headers(&response.headers);
        let fin = response.headers_fin && response.body.is_none();
        match conn.send_response(stream_id, &headers, fin) {
            Ok(()) => {
                response.headers_sent = true;
            }
            Err(e) if is_h3_stream_blocked(&e) => {
                return Ok(PendingResponseFlushOutcome {
                    done: false,
                    released_units: 0,
                });
            }
            Err(e) => return Err(e),
        }
    }

    let Some(body) = response.body.as_mut() else {
        return Ok(PendingResponseFlushOutcome {
            done: true,
            released_units: 0,
        });
    };

    let outcome = flush_one_h3_pending_write(conn, stream_id, body)?;
    if outcome.done {
        response.body = None;
    }

    Ok(PendingResponseFlushOutcome {
        done: outcome.done,
        released_units: outcome.released_units,
    })
}

/// Flush buffered partial writes for all streams. Always compiled (used by
/// both native's `ProtocolHandler::flush_pending_writes` and the
/// direct-call `flush_all_pending_writes`, since `ConnectionMap` itself no
/// longer requires `os-runtime`).
fn flush_pending_writes(
    conn_map: &mut ConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
    _pool: &mut AdaptiveBufferPool,
) -> PendingWriteFlushEvents<(u32, u64)> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&(conn_handle, stream_id), pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            // PendingWrite drops -> chunk recycles to pool
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        };
        if conn.quiche_conn.stream_closed(stream_id) {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        }
        match flush_one_h3_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push((conn_handle, stream_id));
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                // PendingWrite drops -> chunk recycles to pool
                false
            }
            Ok(outcome) => {
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_change(before, pw.queued_bytes());
                true
            }
            Err(e) => {
                log::warn!("flush pending write failed for stream {stream_id}: {e}");
                reactor_metrics::record_outbound_pending_write_removed(before);
                released_units += before_units;
                false // Remove — stream is dead
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

/// Flush buffered partial writes for client streams.
fn flush_client_pending_writes(
    conn: &mut H3Connection,
    pending: &mut HashMap<u64, PendingWrite>,
    _pool: &mut AdaptiveBufferPool,
) -> PendingWriteFlushEvents<u64> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&stream_id, pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        if conn.quiche_conn.stream_closed(stream_id) {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        }
        match flush_one_h3_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push(stream_id);
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                // PendingWrite drops -> chunk recycles to pool
                false
            }
            Ok(outcome) => {
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_change(before, pw.queued_bytes());
                true
            }
            Err(e) => {
                log::warn!("flush pending write failed for stream {stream_id}: {e}");
                reactor_metrics::record_outbound_pending_write_removed(before);
                released_units += before_units;
                false // Remove — stream is dead
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

fn flush_one_h3_pending_write(
    conn: &mut H3Connection,
    stream_id: u64,
    pw: &mut PendingWrite,
) -> Result<PendingWriteFlushOutcome, Http3NativeError> {
    flush_pending_write_with_progress(pw, |buf, send_fin| {
        let outcome = conn.send_body_arcbuf(stream_id, buf, send_fin)?;
        Ok(PendingWriteSendOutcome {
            written: outcome.written,
            fin_accepted: outcome.fin_accepted,
            remainder: outcome.remainder,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc_buf::ArcBuf;
    use crate::reactor_metrics;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::MutexGuard;

    fn setup_metrics() -> MutexGuard<'static, ()> {
        let guard = reactor_metrics::test_metrics_guard();
        reactor_metrics::reset();
        guard
    }

    /// QUIC path validation happens inside quiche — the worker accepts
    /// packets from any peer.
    #[test]
    fn allows_packets_after_peer_address_change() {
        let _original: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let _migrated: SocketAddr = "127.0.0.1:444".parse().expect("valid addr");
        // Acceptance is unconditional; this test verifies compile-time only.
    }

    #[test]
    fn h3_pending_write_byte_accounting_tracks_queue_lifecycle() {
        let _guard = setup_metrics();
        let mut pending = HashMap::new();

        insert_pending_write(
            &mut pending,
            7_u64,
            PendingWrite::new(ArcBuf::from_vec(vec![0; 4]), false),
        );
        assert_eq!(reactor_metrics::snapshot().outboundPendingWriteBytes, 4);

        let write = pending.get_mut(&7).expect("pending write exists");
        reactor_metrics::record_outbound_pending_write_added(
            write.push_chunk(Chunk::unpooled(vec![0; 6])),
        );
        let snap = reactor_metrics::snapshot();
        assert_eq!(snap.outboundPendingWriteBytes, 10);
        assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 10);

        assert_eq!(remove_pending_write(&mut pending, &7), 10);
        let snap = reactor_metrics::snapshot();
        assert_eq!(snap.outboundPendingWriteBytes, 0);
        assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 10);
    }

    #[test]
    fn h3_command_outbound_bytes_reads_unflattened_chunks_client() {
        let client_cmd = ClientWorkerCommand::StreamSend {
            stream_id: 4,
            chunk: Chunk::unpooled(vec![2; 9]),
            fin: true,
        };
        let (client_resp_tx, _client_resp_rx) = crossbeam_channel::bounded(1);
        let client_datagram_cmd = ClientWorkerCommand::SendDatagram {
            data: Chunk::unpooled(vec![4; 13]),
            resp_tx: client_resp_tx,
        };

        assert_eq!(client_worker_command_outbound_bytes(&client_cmd), 9);
        assert_eq!(
            client_worker_command_outbound_bytes(&client_datagram_cmd),
            13
        );
    }

    #[cfg(feature = "os-runtime")]
    #[test]
    fn h3_command_outbound_bytes_reads_unflattened_chunks_server() {
        let server_cmd = WorkerCommand::StreamSend {
            conn_handle: 1,
            stream_id: 2,
            chunk: Chunk::unpooled(vec![1; 5]),
            fin: false,
        };
        let (server_resp_tx, _server_resp_rx) = crossbeam_channel::bounded(1);
        let server_datagram_cmd = WorkerCommand::SendDatagram {
            conn_handle: 1,
            data: Chunk::unpooled(vec![3; 11]),
            resp_tx: server_resp_tx,
        };

        assert_eq!(worker_command_outbound_bytes(&server_cmd), 5);
        assert_eq!(worker_command_outbound_bytes(&server_datagram_cmd), 11);
    }

    // ── A2 task 6: direct-call surface tests (H3ClientHandler::new_direct) ──
    //
    // Sans-IO packet pump, same shape as connection.rs's
    // `exchange_handshake_packets`/`exchange_h3_packets`: no `Driver`, no
    // thread, no socket — just `H3ClientHandler` on one side and a raw
    // `quiche::Connection` (server role) on the other, datagrams handed off
    // in memory. Exercises `new_direct` (A2 task 2), the always-compiled
    // direct-call methods (A2 task 1), and keylog capture (A2 task 5).
    mod direct_call_h3 {
        use super::*;
        use crate::h3_event::{EVENT_HANDSHAKE_COMPLETE, EVENT_SESSION_CLOSE};
        use std::net::{IpAddr, Ipv4Addr};

        const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

        fn test_addrs() -> (SocketAddr, SocketAddr) {
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_001),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51_001),
            )
        }

        fn build_test_configs() -> (quiche::Config, quiche::Config) {
            use rcgen::{CertificateParams, KeyPair};
            let key_pair =
                KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("test key");
            let mut params =
                CertificateParams::new(vec!["localhost".into()]).expect("test cert params");
            params.distinguished_name = rcgen::DistinguishedName::new();
            let cert = params.self_signed(&key_pair).expect("test cert");
            let (cert_pem, key_pem) = (cert.pem(), key_pair.serialize_pem());

            let id = std::thread::current().id();
            let cert_path =
                std::env::temp_dir().join(format!("h3_direct_test_cert_{id:?}.pem"));
            let key_path = std::env::temp_dir().join(format!("h3_direct_test_key_{id:?}.pem"));
            std::fs::write(&cert_path, cert_pem).expect("write test cert");
            std::fs::write(&key_path, key_pem).expect("write test key");

            let mut server_config =
                quiche::Config::new(quiche::PROTOCOL_VERSION).expect("server cfg");
            server_config
                .load_cert_chain_from_pem_file(cert_path.to_str().expect("cert path"))
                .expect("load cert");
            server_config
                .load_priv_key_from_pem_file(key_path.to_str().expect("key path"))
                .expect("load key");
            server_config
                .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
                .expect("server h3 alpn");
            server_config.set_max_idle_timeout(30_000);
            server_config.set_initial_max_data(1_000_000);
            server_config.set_initial_max_stream_data_bidi_local(100_000);
            server_config.set_initial_max_stream_data_bidi_remote(100_000);
            server_config.set_initial_max_stream_data_uni(100_000);
            server_config.set_initial_max_streams_bidi(100);
            server_config.set_initial_max_streams_uni(100);
            server_config.set_disable_active_migration(true);

            let mut client_config =
                quiche::Config::new(quiche::PROTOCOL_VERSION).expect("client cfg");
            client_config
                .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
                .expect("client h3 alpn");
            client_config.verify_peer(false);
            client_config.set_max_idle_timeout(30_000);
            client_config.set_initial_max_data(1_000_000);
            client_config.set_initial_max_stream_data_bidi_local(100_000);
            client_config.set_initial_max_stream_data_bidi_remote(100_000);
            client_config.set_initial_max_stream_data_uni(100_000);
            client_config.set_initial_max_streams_bidi(100);
            client_config.set_initial_max_streams_uni(100);
            client_config.set_disable_active_migration(true);

            let _ = std::fs::remove_file(cert_path);
            let _ = std::fs::remove_file(key_path);
            (server_config, client_config)
        }

        /// Pump datagrams between the direct-call `H3ClientHandler` and a
        /// raw server-role `quiche::Connection` until both sides report the
        /// handshake established (or panic after a generous iteration cap).
        fn pump_until_established(
            handler: &mut H3ClientHandler,
            server_conn: &mut Option<quiche::Connection<ArcBufFactory>>,
            server_config: &mut quiche::Config,
            client_addr: SocketAddr,
            server_addr: SocketAddr,
            batch: &mut Vec<JsH3Event>,
        ) {
            for _ in 0..200 {
                let mut progressed = false;

                let mut outbound = Vec::new();
                while let Some(pkt) = handler.try_send_next() {
                    outbound.push(pkt);
                }
                for mut pkt in outbound {
                    progressed = true;
                    let payload_len = pkt.payload_len();
                    let mut buf = pkt.payload().to_vec();
                    if server_conn.is_none() {
                        let hdr = quiche::Header::from_slice(&mut buf, quiche::MAX_CONN_ID_LEN)
                            .expect("parse initial header");
                        let server_scid = vec![0xcd; quiche::MAX_CONN_ID_LEN];
                        let server_scid = quiche::ConnectionId::from_ref(&server_scid);
                        *server_conn = Some(
                            quiche::accept_with_buf_factory::<ArcBufFactory>(
                                &server_scid,
                                Some(&hdr.dcid),
                                server_addr,
                                client_addr,
                                server_config,
                            )
                            .expect("accept server conn"),
                        );
                    }
                    server_conn
                        .as_mut()
                        .expect("server conn")
                        .recv(
                            &mut buf[..payload_len],
                            quiche::RecvInfo {
                                from: client_addr,
                                to: server_addr,
                            },
                        )
                        .expect("server recv client packet");
                    let _ = &mut pkt; // buffer recycled implicitly on drop
                }

                if let Some(server) = server_conn.as_mut() {
                    let mut send_buf = vec![0_u8; 65_535];
                    loop {
                        match server.send(&mut send_buf) {
                            Ok((len, _info)) => {
                                progressed = true;
                                handler.process_packet_for_handle(
                                    &mut send_buf[..len],
                                    server_addr,
                                    client_addr,
                                    usize::MAX,
                                    batch,
                                    0,
                                );
                            }
                            Err(quiche::Error::Done) => break,
                            Err(error) => panic!("server send failed: {error}"),
                        }
                    }

                    if handler.conn.quiche_conn.is_established() && server.is_established() {
                        return;
                    }
                }

                assert!(progressed, "handshake made no progress");
            }
            panic!("handshake did not complete");
        }

        /// Same shape as `pump_until_established`, but for the post-close
        /// drain: keeps exchanging datagrams (CONNECTION_CLOSE +
        /// acknowledgment) until the handler has emitted its session-close
        /// event and become reapable.
        ///
        /// Unlike the handshake pump, this genuinely needs wall-clock time:
        /// `quiche::Connection::on_timeout` (called from
        /// `process_timers_for_handle`) checks its *own* internal draining
        /// deadline against the real `Instant::now()` — there is no
        /// injectable clock on the host target (that seam is wasm-only,
        /// §4.7 of docs/WASM_CLIENT_PLAN.md), so a real (short, bounded)
        /// sleep is the only way to cross the 3×PTO close-drain period in a
        /// unit test. `refresh_timeout_deadline` (private, same-crate/
        /// same-module-tree access) picks up the draining deadline —
        /// crucially *after* draining `try_send_next()` each round, since
        /// quiche only arms `draining_timer` as a side effect of actually
        /// writing the CONNECTION_CLOSE frame into a sent packet (not at
        /// `close()` time), and `close_session` itself doesn't refresh it.
        fn pump_until_reapable(
            handler: &mut H3ClientHandler,
            server_conn: &mut Option<quiche::Connection<ArcBufFactory>>,
            client_addr: SocketAddr,
            server_addr: SocketAddr,
            batch: &mut Vec<JsH3Event>,
        ) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if handler.is_reapable() {
                    return;
                }

                let mut outbound = Vec::new();
                while let Some(pkt) = handler.try_send_next() {
                    outbound.push(pkt);
                }
                handler.refresh_timeout_deadline();
                for pkt in &outbound {
                    let payload_len = pkt.payload_len();
                    let mut buf = pkt.payload().to_vec();
                    if let Some(server) = server_conn.as_mut() {
                        // Closing connections can legitimately fail to
                        // recv once the server side is already draining;
                        // that's fine, we only need the handler to react
                        // to whatever *does* get through (or to its own
                        // local close, which needs no round trip at all).
                        let _ = server.recv(
                            &mut buf[..payload_len],
                            quiche::RecvInfo {
                                from: client_addr,
                                to: server_addr,
                            },
                        );
                    }
                }

                if let Some(server) = server_conn.as_mut() {
                    let mut send_buf = vec![0_u8; 65_535];
                    loop {
                        match server.send(&mut send_buf) {
                            Ok((len, _info)) => {
                                handler.process_packet_for_handle(
                                    &mut send_buf[..len],
                                    server_addr,
                                    client_addr,
                                    usize::MAX,
                                    batch,
                                    0,
                                );
                            }
                            Err(_) => break,
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(20));
                handler.process_timers_for_handle(Instant::now(), usize::MAX, batch, 0);
            }

            panic!("handler did not become reapable within the drain deadline");
        }

        #[test]
        fn new_direct_completes_handshake_and_send_request() {
            let _guard = setup_metrics();
            let (mut server_config, mut client_config) = build_test_configs();
            let (client_addr, server_addr) = test_addrs();
            let scid = vec![0x42_u8; TEST_SCID_LEN];
            let outbound_admission = Arc::new(OutboundAdmission::default());

            let mut handler = H3ClientHandler::new_direct(
                scid,
                client_addr,
                server_addr,
                "localhost",
                None,
                None,
                None,
                &mut client_config,
                outbound_admission,
            )
            .expect("new_direct should construct a handler");

            let mut batch = Vec::new();
            let mut server_conn = None;
            pump_until_established(
                &mut handler,
                &mut server_conn,
                &mut server_config,
                client_addr,
                server_addr,
                &mut batch,
            );

            let event_types: Vec<u8> = batch.iter().map(|e| e.event_type).collect();
            assert!(
                event_types.contains(&EVENT_HANDSHAKE_COMPLETE),
                "expected a handshake-complete event, got event types {event_types:?}"
            );
            assert!(!handler.is_reapable(), "handler should not be reapable yet");
            assert!(
                handler.next_timer_deadline().is_some(),
                "expected an armed idle-timeout deadline after handshake"
            );

            let stream_id = handler
                .send_request(
                    vec![
                        (":method".into(), "GET".into()),
                        (":scheme".into(), "https".into()),
                        (":authority".into(), "localhost".into()),
                        (":path".into(), "/".into()),
                    ],
                    true,
                )
                .expect("send_request should succeed after handshake");
            assert_eq!(stream_id, 0, "first client-initiated bidi stream is 0");

            // Drive one more round so the request datagram reaches the server.
            batch.clear();
            pump_until_established(
                &mut handler,
                &mut server_conn,
                &mut server_config,
                client_addr,
                server_addr,
                &mut batch,
            );

            handler.close_session(0, "test done");
            batch.clear();
            pump_until_reapable(
                &mut handler,
                &mut server_conn,
                client_addr,
                server_addr,
                &mut batch,
            );
            assert!(
                batch.iter().any(|e| e.event_type == EVENT_SESSION_CLOSE),
                "expected a session-close event after close_session"
            );
            assert!(
                handler.is_reapable(),
                "handler should be reapable after session close"
            );
        }

        #[test]
        fn new_direct_uses_the_caller_supplied_scid_bytes() {
            // The whole point of `new_direct` (vs. the ring-backed `new`) is
            // that the caller's entropy is what ends up on the wire — prove
            // two handlers built from different SCID bytes report different
            // connection ids, i.e. the bytes are actually threaded through
            // to quiche rather than silently re-randomized internally.
            let (_server_config, mut client_config_a) = build_test_configs();
            let (_server_config_b, mut client_config_b) = build_test_configs();
            let (client_addr, server_addr) = test_addrs();

            let handler_a = H3ClientHandler::new_direct(
                vec![0x11_u8; TEST_SCID_LEN],
                client_addr,
                server_addr,
                "localhost",
                None,
                None,
                None,
                &mut client_config_a,
                Arc::new(OutboundAdmission::default()),
            )
            .expect("new_direct should construct a handler");
            let handler_b = H3ClientHandler::new_direct(
                vec![0x22_u8; TEST_SCID_LEN],
                client_addr,
                server_addr,
                "localhost",
                None,
                None,
                None,
                &mut client_config_b,
                Arc::new(OutboundAdmission::default()),
            )
            .expect("new_direct should construct a handler");

            assert_eq!(handler_a.current_dcid(), vec![0x11_u8; TEST_SCID_LEN]);
            assert_eq!(handler_b.current_dcid(), vec![0x22_u8; TEST_SCID_LEN]);
        }

        #[test]
        fn keylog_capture_accumulates_lines_during_handshake() {
            let _guard = setup_metrics();
            let (mut server_config, mut client_config) = build_test_configs();
            // `Connection::set_keylog` (called by `enable_keylog` below)
            // only supplies *where* to write; the underlying BoringSSL
            // callback is only invoked at all once `Config::log_keys` has
            // registered it on the shared SSL_CTX — matching the native
            // `options.keylog` path (config.rs's `log_keys()` calls).
            client_config.log_keys();
            let (client_addr, server_addr) = test_addrs();
            let scid = vec![0x77_u8; TEST_SCID_LEN];
            let outbound_admission = Arc::new(OutboundAdmission::default());

            let mut handler = H3ClientHandler::new_direct(
                scid,
                client_addr,
                server_addr,
                "localhost",
                None,
                None,
                None,
                &mut client_config,
                outbound_admission,
            )
            .expect("new_direct should construct a handler");
            handler.enable_keylog();

            let mut batch = Vec::new();
            let mut server_conn = None;
            pump_until_established(
                &mut handler,
                &mut server_conn,
                &mut server_config,
                client_addr,
                server_addr,
                &mut batch,
            );

            let lines = handler.take_keylog_lines();
            assert!(
                !lines.is_empty(),
                "expected at least one NSS-format keylog line after handshake"
            );
            assert!(
                lines
                    .windows(b"CLIENT_".len())
                    .any(|w| w == b"CLIENT_"),
                "keylog output should contain NSS CLIENT_* labels"
            );
            // Draining again with no new secrets yields nothing.
            assert!(handler.take_keylog_lines().is_empty());
        }
    }

    /// Lockstep tests for the server-side direct-call surface
    /// (`H3ServerHandler::new_direct` + `process_inbound_packet` +
    /// friends), added alongside server-side wasm ABI support. Mirrors
    /// `direct_call_h3` above's style, but drives **two** real handlers
    /// (`H3ServerHandler` and `H3ClientHandler`, both already
    /// direct-call-constructible) directly against each other — no
    /// hand-rolled `quiche::Connection` or real UDP socket needed, since
    /// this repo's own server handler is now available as a peer.
    mod direct_call_h3_server {
        use super::*;
        use crate::config::{JsClientOptions, JsServerOptions};
        use crate::h3_event::{
            EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_NEW_SESSION,
            EVENT_SESSION_CLOSE,
        };
        use std::net::{IpAddr, Ipv4Addr};

        const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

        fn test_addrs() -> (SocketAddr, SocketAddr) {
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43_001), // client
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53_001), // server
            )
        }

        fn generate_self_signed_pem() -> (Vec<u8>, Vec<u8>) {
            use rcgen::{CertificateParams, KeyPair};
            let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("keypair");
            let mut params = CertificateParams::new(vec!["localhost".into()]).expect("params");
            params.distinguished_name = rcgen::DistinguishedName::new();
            let cert = params.self_signed(&key_pair).expect("self-signed cert");
            (
                cert.pem().into_bytes(),
                key_pair.serialize_pem().into_bytes(),
            )
        }

        /// Builds a real `H3ServerHandler::new_direct` using the new
        /// in-memory config builders (`Http3Config::from_server_options` +
        /// `Http3Config::new_server_quiche_config_in_memory`) — the exact
        /// pair a `hs_new` ABI implementation (Part C) will call.
        fn build_server_direct(client_auth: Option<&str>, ca: Option<Vec<u8>>) -> H3ServerHandler {
            let (cert_pem, key_pem) = generate_self_signed_pem();
            let options = JsServerOptions {
                key: key_pem.into(),
                cert: cert_pem.into(),
                ca: ca.map(Into::into),
                client_auth: client_auth.map(str::to_string),
                runtime_mode: None,
                max_idle_timeout_ms: Some(5_000),
                max_udp_payload_size: Some(1_350),
                initial_max_data: Some(1_000_000),
                initial_max_stream_data_bidi_local: Some(1_000_000),
                initial_max_streams_bidi: Some(100),
                disable_active_migration: Some(true),
                enable_datagrams: None,
                qpack_max_table_capacity: None,
                qpack_blocked_streams: None,
                recv_batch_size: None,
                send_batch_size: None,
                qlog_dir: None,
                qlog_level: None,
                session_ticket_keys: None,
                max_connections: Some(128),
                disable_retry: Some(true),
                reuse_port: None,
                keylog: None,
                quic_lb: None,
                server_id: None,
            };
            let quiche_config =
                Http3Config::new_server_quiche_config_in_memory(&options).expect("server config");
            let http3_config = Http3Config::from_server_options(&options).expect("http3 config");
            H3ServerHandler::new_direct(
                quiche_config,
                http3_config,
                [0x55u8; 32],
                Arc::new(OutboundAdmission::default()),
            )
        }

        fn build_client_direct(scid_byte: u8, client_addr: SocketAddr, server_addr: SocketAddr) -> H3ClientHandler {
            let client_options = JsClientOptions {
                ca: None,
                reject_unauthorized: Some(false),
                runtime_mode: None,
                max_idle_timeout_ms: Some(5_000),
                max_udp_payload_size: Some(1_350),
                initial_max_data: None,
                initial_max_stream_data_bidi_local: None,
                initial_max_streams_bidi: None,
                session_ticket: None,
                allow_0rtt: None,
                enable_datagrams: None,
                keylog: None,
                qlog_dir: None,
                qlog_level: None,
                disable_pacing: Some(true),
            };
            let mut client_config =
                Http3Config::new_client_quiche_config_in_memory(&client_options).expect("client config");
            H3ClientHandler::new_direct(
                vec![scid_byte; TEST_SCID_LEN],
                client_addr,
                server_addr,
                "localhost",
                None,
                None,
                None,
                &mut client_config,
                Arc::new(OutboundAdmission::default()),
            )
            .expect("client new_direct should construct")
        }

        /// One pump round: drain the client's outbound into the server
        /// (via `process_inbound_packet`, delivering any retry/version-
        /// negotiation reply straight back), drain the server's outbound
        /// into the client (via `flush_all_sends`), and process any due
        /// timers on both sides. Returns whether anything happened.
        #[allow(clippy::too_many_arguments)]
        fn pump(
            client: &mut H3ClientHandler,
            server: &mut H3ServerHandler,
            client_addr: SocketAddr,
            server_addr: SocketAddr,
            client_batch: &mut Vec<JsH3Event>,
            server_batch: &mut Vec<JsH3Event>,
        ) -> bool {
            let mut progressed = false;

            while let Some(pkt) = client.try_send_next() {
                progressed = true;
                let mut buf = pkt.payload().to_vec();
                let mut pending_outbound: Vec<TxDatagram> = Vec::new();
                server.process_inbound_packet(
                    &mut buf,
                    client_addr,
                    server_addr,
                    &mut pending_outbound,
                    usize::MAX,
                    server_batch,
                );
                for reply in pending_outbound {
                    let mut reply_buf = reply.payload().to_vec();
                    client.process_packet_for_handle(
                        &mut reply_buf,
                        server_addr,
                        client_addr,
                        usize::MAX,
                        client_batch,
                        0,
                    );
                }
            }

            let mut server_outbound: Vec<TxDatagram> = Vec::new();
            server.flush_all_sends(&mut server_outbound);
            for pkt in server_outbound {
                progressed = true;
                let mut buf = pkt.payload().to_vec();
                client.process_packet_for_handle(
                    &mut buf,
                    server_addr,
                    client_addr,
                    usize::MAX,
                    client_batch,
                    0,
                );
            }

            if client
                .next_timer_deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                client.process_timers_for_handle(Instant::now(), usize::MAX, client_batch, 0);
                progressed = true;
            }
            if server
                .soonest_deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                server.expire_timers(Instant::now(), usize::MAX, server_batch);
                progressed = true;
            }

            server.collect_drain_events(usize::MAX, server_batch);
            server.flush_all_pending_writes(server_batch);
            client.poll_drain_events_for_handle(client_batch, 0);
            client.flush_pending_writes_for_handle(client_batch, 0);

            progressed
        }

        /// Pumps until `done` returns `true` or a 5-second wall-clock
        /// deadline elapses (then panics). Sleeps briefly on rounds that
        /// make no packet progress — needed for timer-driven transitions
        /// (idle timeout, the H3 server's deferred GOAWAY-then-close) that
        /// only fire once real time has actually passed; see
        /// `pump_until_reapable` above for the identical rationale (no
        /// injectable clock on the host target — that seam is wasm-only).
        fn pump_until<F>(
            client: &mut H3ClientHandler,
            server: &mut H3ServerHandler,
            client_addr: SocketAddr,
            server_addr: SocketAddr,
            client_batch: &mut Vec<JsH3Event>,
            server_batch: &mut Vec<JsH3Event>,
            mut done: F,
        ) where
            F: FnMut(&[JsH3Event], &[JsH3Event]) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if done(client_batch, server_batch) {
                    return;
                }
                let progressed = pump(client, server, client_addr, server_addr, client_batch, server_batch);
                if done(client_batch, server_batch) {
                    return;
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            panic!("pump_until exceeded the 5s deadline without reaching the target condition");
        }

        #[test]
        fn new_direct_completes_handshake_and_h3_request_response() {
            let _guard = setup_metrics();
            let (client_addr, server_addr) = test_addrs();
            let mut server = build_server_direct(None, None);
            let mut client = build_client_direct(0x61, client_addr, server_addr);

            let mut client_batch = Vec::new();
            let mut server_batch = Vec::new();

            pump_until(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
                |client_batch, server_batch| {
                    client_batch.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
                        && server_batch.iter().any(|e| e.event_type == EVENT_HANDSHAKE_COMPLETE)
                },
            );

            assert!(
                server_batch.iter().any(|e| e.event_type == EVENT_NEW_SESSION),
                "server should have observed a new session"
            );
            assert_eq!(
                server.connection_count(),
                1,
                "server should be tracking exactly one connection"
            );
            let conn_handle = server_batch
                .iter()
                .find(|e| e.event_type == EVENT_NEW_SESSION)
                .expect("new session event")
                .conn_handle;
            assert!(!server.connection_is_closed(conn_handle));
            assert!(!server.is_idle());

            // --- GET request ---
            let stream_id = client
                .send_request(
                    vec![
                        (":method".into(), "GET".into()),
                        (":scheme".into(), "https".into()),
                        (":authority".into(), "localhost".into()),
                        (":path".into(), "/hello".into()),
                    ],
                    true,
                )
                .expect("send_request should succeed once established");

            server_batch.clear();
            pump_until(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
                |_client_batch, server_batch| server_batch.iter().any(|e| e.event_type == EVENT_HEADERS),
            );

            // --- Respond: headers, then body + FIN ---
            server
                .send_response_headers(
                    conn_handle,
                    stream_id,
                    vec![(":status".into(), "200".into())],
                    false,
                    &mut server_batch,
                )
                .expect("send_response_headers should succeed");
            let released = server.queue_stream_send(
                conn_handle,
                stream_id,
                Chunk::unpooled(b"hello from the direct-call H3 server".to_vec()),
                true,
                &mut server_batch,
            );
            assert!(released > 0, "response body should be admitted, not backpressured");

            client_batch.clear();
            let mut body = Vec::new();
            let mut got_headers = false;
            pump_until(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
                |client_batch, _server_batch| {
                    for event in client_batch.iter() {
                        if event.stream_id as u64 != stream_id {
                            continue;
                        }
                        if event.event_type == EVENT_HEADERS {
                            got_headers = true;
                        }
                        if let Some(data) = &event.data {
                            if event.event_type == EVENT_HEADERS || event.event_type == EVENT_DATA {
                                body.extend_from_slice(data);
                            }
                        }
                    }
                    got_headers && !body.is_empty()
                },
            );

            assert!(got_headers, "client should have observed EVENT_HEADERS");
            assert_eq!(body, b"hello from the direct-call H3 server");

            // --- Graceful per-connection close (GOAWAY, then deferred CONNECTION_CLOSE) ---
            // A dedicated loop rather than `pump_until` here: `pump_until`'s
            // `done` closure only sees the two batches (by design, so
            // callers never need to fight the borrow checker over aliasing
            // `server` both as the loop's `&mut` argument and inside the
            // closure) — this is the one assertion in this test that
            // genuinely needs to inspect `server`'s own connection state
            // directly instead.
            server.close_connection(conn_handle, 0, "server done".to_string());
            let close_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < close_deadline && !server.connection_is_closed(conn_handle) {
                let progressed = pump(
                    &mut client,
                    &mut server,
                    client_addr,
                    server_addr,
                    &mut client_batch,
                    &mut server_batch,
                );
                if !progressed {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert!(
                server.connection_is_closed(conn_handle),
                "server connection should close within 5s of close_connection"
            );

            // Don't clear `server_batch` here: since the connection closed
            // via the idle/draining *timer* path above, `expire_timers`
            // already pushed `EVENT_SESSION_CLOSE` for it during the
            // close-wait loop, and `reap_closed_connections` deliberately
            // skips re-pushing a duplicate for any handle already seen via
            // `last_expired` (the exact same dedup native's own
            // `cleanup_closed` relies on).
            server.reap_closed_connections(&mut server_batch);
            assert!(
                server_batch.iter().any(|e| e.event_type == EVENT_SESSION_CLOSE),
                "expected a session_close event from either the close-wait pump or reap_closed_connections"
            );
            assert!(
                server.is_idle(),
                "server should be idle after reaping the only connection"
            );
            assert_eq!(server.connection_count(), 0);
        }
    }
}
