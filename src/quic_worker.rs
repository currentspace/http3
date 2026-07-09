//! Worker thread loops for raw QUIC (no HTTP/3 framing).
//! Shares the event delivery mechanism (TSFN) with the H3 worker
//! but uses direct `stream_send` / `stream_recv` instead of H3 framing.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
#[cfg(feature = "os-runtime")]
use ring::rand::SecureRandom;
use slab::Slab;

use crate::arc_buf::ArcBufFactory;
use crate::buffer_pool::BufferPool;
use crate::chunk_pool::{Chunk, ChunkPool};
use crate::cid::CidEncoding;
#[cfg(feature = "os-runtime")]
use crate::client_topology::{
    ClientSocketStrategy, SharedClientWorkerKey as SharedQuicClientWorkerKey,
    default_quic_client_socket_strategy, shared_client_bind_addr, shared_client_worker_key,
};
use crate::config::{ClientAuthMode, TransportRuntimeMode};
use crate::datagram::TxDatagram;
use crate::error::Http3NativeError;
#[cfg(feature = "node-api")]
use crate::event_loop::EventTsfn;
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
use crate::quic_connection::{QuicConnection, QuicConnectionInit};
use crate::reactor_metrics::{
    self, RawQuicClientCloseCause, SessionKind, WorkerLoopExitCause, WorkerSpawnKind,
};
use crate::retry_token::{self, DeterministicScidSource};
#[cfg(feature = "os-runtime")]
use crate::shared_client_reactor;
use crate::timer_heap::TimerHeap;
#[cfg(feature = "os-runtime")]
use crate::transport::{self, ErasedWaker};

const SCID_LEN: usize = crate::cid::SCID_LEN;
const TOKEN_LIFETIME_SECS: u64 = 60;

// ── Server command/handle ──────────────────────────────────────────

#[cfg(feature = "os-runtime")]
pub enum QuicServerCommand {
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

/// Per-worker state inside a `QuicServerHandle`.
#[cfg(feature = "os-runtime")]
pub struct QuicServerWorker {
    pub cmd_tx: Sender<QuicServerCommand>,
    pub join_handle: Option<thread::JoinHandle<()>>,
    pub waker: Arc<dyn ErasedWaker>,
    outbound_admission: Arc<OutboundAdmission>,
}

#[cfg(feature = "os-runtime")]
use crate::server_sharding;

#[cfg(feature = "os-runtime")]
pub struct QuicServerHandle {
    workers: Vec<QuicServerWorker>,
    local_addr: SocketAddr,
}

#[cfg(feature = "os-runtime")]
impl QuicServerHandle {
    pub fn from_workers(workers: Vec<QuicServerWorker>, local_addr: SocketAddr) -> Self {
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
        self.workers[server_sharding::worker_index(conn_handle)]
            .outbound_admission
            .try_admit(outbound_payload_units(payload_len, fin))
    }

    pub(crate) fn release_outbound_admission(
        &self,
        conn_handle: u32,
        payload_len: usize,
        fin: bool,
    ) {
        let _ = self.workers[server_sharding::worker_index(conn_handle)]
            .outbound_admission
            .release(outbound_payload_units(payload_len, fin));
    }

    /// Route a command to the worker that owns `conn_handle`.
    pub fn send_command(&self, cmd: QuicServerCommand) -> bool {
        let conn_handle = command_conn_handle(&cmd);
        let worker = &self.workers[server_sharding::worker_index(conn_handle)];
        let local = remap_command_handle(cmd);
        let queued_bytes = quic_server_command_outbound_bytes(&local);
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
        let worker = &self.workers[server_sharding::worker_index(conn_handle)];
        let local_handle = server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(QuicServerCommand::GetSessionMetrics {
                conn_handle: local_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for metrics".into()))
    }

    pub fn send_datagram<D>(&self, conn_handle: u32, data: D) -> Result<bool, Http3NativeError>
    where
        D: Into<Chunk>,
    {
        let worker = &self.workers[server_sharding::worker_index(conn_handle)];
        let local_handle = server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(QuicServerCommand::SendDatagram {
                conn_handle: local_handle,
                data: data.into(),
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = worker.waker.wake();
        Ok(resp_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false))
    }

    pub fn ping_session(&self, conn_handle: u32) -> Result<bool, Http3NativeError> {
        let worker = &self.workers[server_sharding::worker_index(conn_handle)];
        let local_handle = server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(QuicServerCommand::PingSession {
                conn_handle: local_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for ping".into()))
    }

    pub fn get_qlog_path(&self, conn_handle: u32) -> Result<Option<String>, Http3NativeError> {
        let worker = &self.workers[server_sharding::worker_index(conn_handle)];
        let local_handle = server_sharding::local_conn_handle(conn_handle);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        worker
            .cmd_tx
            .send(QuicServerCommand::GetQlogPath {
                conn_handle: local_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("quic worker not running".into()))?;
        let _ = worker.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for qlog path".into()))
    }

    /// Send the Shutdown command to all workers without joining threads.
    pub fn request_shutdown(&self) {
        for worker in &self.workers {
            let _ = worker.cmd_tx.send(QuicServerCommand::Shutdown);
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
fn shutdown_spawned_quic_server_workers(workers: &mut Vec<QuicServerWorker>) {
    for worker in workers.iter() {
        let _ = worker.cmd_tx.send(QuicServerCommand::Shutdown);
        let _ = worker.waker.wake();
    }
    for worker in workers.iter_mut() {
        if let Some(handle) = worker.join_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "os-runtime")]
impl Drop for QuicServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Extract the conn_handle from a command for routing purposes.
#[cfg(feature = "os-runtime")]
fn command_conn_handle(cmd: &QuicServerCommand) -> u32 {
    match cmd {
        QuicServerCommand::StreamSend { conn_handle, .. }
        | QuicServerCommand::StreamClose { conn_handle, .. }
        | QuicServerCommand::CloseSession { conn_handle, .. }
        | QuicServerCommand::SendDatagram { conn_handle, .. }
        | QuicServerCommand::GetSessionMetrics { conn_handle, .. }
        | QuicServerCommand::PingSession { conn_handle, .. }
        | QuicServerCommand::GetQlogPath { conn_handle, .. } => *conn_handle,
        QuicServerCommand::Shutdown => 0,
    }
}

/// Remap a command's conn_handle from global to local (strip worker bits).
#[cfg(feature = "os-runtime")]
fn remap_command_handle(cmd: QuicServerCommand) -> QuicServerCommand {
    match cmd {
        QuicServerCommand::StreamSend {
            conn_handle,
            stream_id,
            chunk,
            fin,
        } => QuicServerCommand::StreamSend {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            stream_id,
            chunk,
            fin,
        },
        QuicServerCommand::StreamClose {
            conn_handle,
            stream_id,
            error_code,
        } => QuicServerCommand::StreamClose {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            stream_id,
            error_code,
        },
        QuicServerCommand::CloseSession {
            conn_handle,
            error_code,
            reason,
        } => QuicServerCommand::CloseSession {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            error_code,
            reason,
        },
        QuicServerCommand::SendDatagram {
            conn_handle,
            data,
            resp_tx,
        } => QuicServerCommand::SendDatagram {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            data,
            resp_tx,
        },
        QuicServerCommand::GetSessionMetrics {
            conn_handle,
            resp_tx,
        } => QuicServerCommand::GetSessionMetrics {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            resp_tx,
        },
        QuicServerCommand::PingSession {
            conn_handle,
            resp_tx,
        } => QuicServerCommand::PingSession {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            resp_tx,
        },
        QuicServerCommand::GetQlogPath {
            conn_handle,
            resp_tx,
        } => QuicServerCommand::GetQlogPath {
            conn_handle: server_sharding::local_conn_handle(conn_handle),
            resp_tx,
        },
        QuicServerCommand::Shutdown => QuicServerCommand::Shutdown,
    }
}

// ── Client command/handle ──────────────────────────────────────────

pub enum QuicClientCommand {
    OpenStream {
        resp_tx: Sender<Result<u64, Http3NativeError>>,
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

#[cfg(feature = "os-runtime")]
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
    OpenStream {
        session_handle: u32,
        resp_tx: Sender<Result<u64, Http3NativeError>>,
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

#[cfg(feature = "os-runtime")]
struct SharedQuicClientWorkerControl {
    cmd_tx: Sender<SharedQuicClientCommand>,
    waker: Arc<dyn ErasedWaker>,
    local_addr: SocketAddr,
    outbound_admission: Arc<OutboundAdmission>,
    join_handle: Mutex<Option<thread::JoinHandle<()>>>,
    running: AtomicBool,
    session_count: AtomicUsize,
    key: SharedQuicClientWorkerKey,
}

#[cfg(feature = "os-runtime")]
impl SharedQuicClientWorkerControl {
    fn wake(&self) {
        let _ = self.waker.wake();
    }
}

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
pub struct QuicClientHandle {
    kind: Option<QuicClientHandleKind>,
    local_addr: SocketAddr,
    outbound_admission: Arc<OutboundAdmission>,
}

#[cfg(feature = "os-runtime")]
impl QuicClientHandle {
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
            Some(QuicClientHandleKind::Dedicated { waker, .. }) => {
                let _ = waker.wake();
            }
            Some(QuicClientHandleKind::Shared { worker, .. }) => {
                worker.wake();
            }
            None => {}
        }
    }

    pub fn open_stream(&self) -> Result<u64, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                cmd_tx
                    .send(QuicClientCommand::OpenStream { resp_tx })
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
                    .send(SharedQuicClientCommand::OpenStream {
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
            None => {
                return Err(Http3NativeError::InvalidState(
                    "quic client not running".into(),
                ));
            }
        }

        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out opening stream".into()))?
    }

    pub fn stream_send(&self, stream_id: u64, data: Vec<u8>, fin: bool) -> bool {
        self.stream_send_chunk(stream_id, Chunk::unpooled(data), fin)
    }

    pub fn stream_send_chunk(&self, stream_id: u64, chunk: Chunk, fin: bool) -> bool {
        match &self.kind {
            Some(QuicClientHandleKind::Dedicated { cmd_tx, waker, .. }) => {
                let queued_bytes = chunk.remaining_len();
                if cmd_tx
                    .send(QuicClientCommand::StreamSend {
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
            Some(QuicClientHandleKind::Shared {
                session_handle,
                worker,
            }) => {
                let queued_bytes = chunk.remaining_len();
                if worker
                    .cmd_tx
                    .send(SharedQuicClientCommand::StreamSend {
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

    pub fn send_datagram<D>(&self, data: D) -> Result<bool, Http3NativeError>
    where
        D: Into<Chunk>,
    {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        let data = data.into();
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
            None => {
                return Err(Http3NativeError::InvalidState(
                    "quic client not running".into(),
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
            None => {
                return Err(Http3NativeError::InvalidState(
                    "quic client not running".into(),
                ));
            }
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
            None => {
                return Err(Http3NativeError::InvalidState(
                    "quic client not running".into(),
                ));
            }
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
            None => {
                return Err(Http3NativeError::InvalidState(
                    "quic client not running".into(),
                ));
            }
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

    /// Send the Shutdown (or ReleaseSession) command without joining threads.
    pub fn request_shutdown(&self) {
        let Some(kind) = &self.kind else { return };
        match kind {
            QuicClientHandleKind::Dedicated { cmd_tx, waker, .. } => {
                let _ = cmd_tx.send(QuicClientCommand::Shutdown);
                let _ = waker.wake();
            }
            QuicClientHandleKind::Shared {
                session_handle,
                worker,
            } => {
                let _ = worker.cmd_tx.send(SharedQuicClientCommand::ReleaseSession {
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
            QuicClientHandleKind::Dedicated {
                mut join_handle, ..
            } => {
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
            }
            QuicClientHandleKind::Shared { worker, .. } => {
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

    pub fn shutdown(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

#[cfg(feature = "os-runtime")]
impl Drop for QuicClientHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Minimal connection map for QUIC ────────────────────────────────
//
// Always compiled (see `connection_map.rs`'s module doc comment for the
// full rationale — this type follows the exact same pattern). `ring` stays
// confined to the native `new` constructor.

struct QuicConnectionMap {
    by_dcid: HashMap<Vec<u8>, usize>,
    connections: Slab<QuicConnection>,
    /// HMAC-SHA256 key for minting/validating retry tokens (raw bytes, not
    /// `ring::hmac::Key` — see `retry_token.rs`).
    token_key: [u8; 32],
    max_connections: usize,
    cid_encoding: CidEncoding,
    /// Sans-IO SCID generator for
    /// [`QuicConnectionMap::generate_scid_direct`] — see
    /// `retry_token.rs` / `connection_map.rs::ConnectionMap` for the
    /// identical H3-side pattern this mirrors.
    scid_source: DeterministicScidSource,
}

impl QuicConnectionMap {
    /// Native constructor: sources the 32-byte HMAC key from `ring`'s
    /// system RNG. Requires `os-runtime` for the same reason as
    /// `ConnectionMap::with_max_connections_and_cid`.
    #[cfg(feature = "os-runtime")]
    fn new(max_connections: usize, cid_encoding: CidEncoding) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let mut key_bytes = [0u8; 32];
        #[allow(clippy::expect_used)]
        rng.fill(&mut key_bytes)
            .expect("system RNG should not fail");
        Self::with_key_bytes(max_connections, cid_encoding, key_bytes)
    }

    /// Sans-IO constructor for a direct-call caller (a wasm ABI, or the
    /// unit tests below): the caller supplies the 32-byte HMAC key
    /// directly instead of requiring `ring` — mirrors
    /// `ConnectionMap::with_key_bytes` exactly.
    fn with_key_bytes(max_connections: usize, cid_encoding: CidEncoding, key_bytes: [u8; 32]) -> Self {
        Self {
            by_dcid: HashMap::new(),
            connections: Slab::new(),
            token_key: key_bytes,
            max_connections,
            cid_encoding,
            scid_source: DeterministicScidSource::new(key_bytes),
        }
    }

    /// Native only: sources entropy from `ring`'s system RNG via `CidEncoding`.
    #[cfg(feature = "os-runtime")]
    fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        self.cid_encoding.generate_scid()
    }

    /// Sans-IO alternative for the direct-call / wasm server surface — see
    /// `ConnectionMap::generate_scid_direct`.
    fn generate_scid_direct(&mut self) -> Result<Vec<u8>, Http3NativeError> {
        self.scid_source.next_scid(&self.cid_encoding)
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
        let tag = retry_token::hmac_sha256(&self.token_key, &payload);
        let mut token = tag.to_vec();
        token.extend_from_slice(&payload);
        token
    }

    fn validate_token(&self, token: &[u8], peer: &SocketAddr) -> Option<Vec<u8>> {
        if token.len() < 32 {
            return None;
        }
        let (tag_bytes, payload) = token.split_at(32);
        if !retry_token::hmac_sha256_verify(&self.token_key, payload, tag_bytes) {
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
        // Audit finding #4: signed window guards both directions of clock
        // skew. saturating_sub by itself would underflow to 0 on a
        // backwards jump and accept all in-flight tokens.
        let skew = (now as i64).saturating_sub(timestamp as i64).abs();
        if skew > TOKEN_LIFETIME_SECS as i64 {
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
        let quiche_conn =
            quiche::accept_with_buf_factory::<ArcBufFactory>(&scid_ref, odcid, local, peer, config)
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

    /// Number of live connections currently tracked (used by the
    /// direct-call/wasm surface's "is the whole server idle" check).
    fn len(&self) -> usize {
        self.connections.len()
    }

    fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    fn drain_closed(&mut self) -> Vec<(usize, QuicConnection)> {
        let closed: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, conn)| conn.is_closed())
            .map(|(handle, _)| handle)
            .collect();
        closed
            .into_iter()
            .filter_map(|handle| self.remove(handle).map(|conn| (handle, conn)))
            .collect()
    }
}

// ── Pending write ──────────────────────────────────────────────────

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

#[cfg(feature = "os-runtime")]
fn quic_server_command_outbound_bytes(cmd: &QuicServerCommand) -> usize {
    match cmd {
        QuicServerCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        QuicServerCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

fn quic_client_command_outbound_bytes(cmd: &QuicClientCommand) -> usize {
    match cmd {
        QuicClientCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        QuicClientCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

#[cfg(feature = "os-runtime")]
fn shared_quic_client_command_outbound_bytes(cmd: &SharedQuicClientCommand) -> usize {
    match cmd {
        SharedQuicClientCommand::StreamSend { chunk, fin, .. } => {
            outbound_payload_units(chunk.remaining_len(), *fin)
        }
        SharedQuicClientCommand::SendDatagram { data, .. } => data.remaining_len(),
        _ => 0,
    }
}

// ── Spawn functions ────────────────────────────────────────────────

/// Always compiled (needed by `QuicServerHandler::new_direct`, which is
/// itself always compiled) — none of these fields are `os-runtime`-only
/// types.
pub struct QuicServerConfig {
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub max_connections: usize,
    pub disable_retry: bool,
    pub client_auth: ClientAuthMode,
    pub cid_encoding: CidEncoding,
    pub runtime_mode: TransportRuntimeMode,
}

#[cfg(feature = "os-runtime")]
pub fn spawn_server_worker_on_driver<D>(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    worker_index: u32,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<QuicServerCommand>,
    cmd_rx: Receiver<QuicServerCommand>,
    batcher: EventBatcher,
) -> QuicServerWorker
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    spawn_server_worker_on_driver_with_admission(
        quiche_config,
        server_config,
        worker_index,
        driver,
        waker,
        local_addr,
        cmd_tx,
        cmd_rx,
        batcher,
        Arc::new(OutboundAdmission::default()),
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "os-runtime")]
fn spawn_server_worker_on_driver_with_admission<D>(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    worker_index: u32,
    driver: D,
    waker: D::Waker,
    local_addr: SocketAddr,
    cmd_tx: Sender<QuicServerCommand>,
    cmd_rx: Receiver<QuicServerCommand>,
    batcher: EventBatcher,
    outbound_admission: Arc<OutboundAdmission>,
) -> QuicServerWorker
where
    D: transport::Driver + Send + 'static,
    D::Waker: Send + Sync + Clone + 'static,
{
    let waker_arc: Arc<dyn ErasedWaker> = Arc::new(waker);
    let waker_clone = waker_arc.clone();
    let worker_outbound_admission = Arc::clone(&outbound_admission);

    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::RawQuicServer);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        let mut handler = QuicServerHandler::new(
            quiche_config,
            server_config,
            worker_index,
            outbound_admission,
        );
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, batcher, local_addr);
    });

    QuicServerWorker {
        cmd_tx,
        join_handle: Some(join_handle),
        waker: waker_clone,
        outbound_admission: worker_outbound_admission,
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "os-runtime")]
pub fn spawn_dedicated_quic_client_on_driver<D>(
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
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let admission_ref = Arc::clone(&outbound_admission);

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
            admission_ref,
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
        outbound_admission,
    })
}

#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
pub fn spawn_quic_server(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    tsfn: std::sync::Arc<EventTsfn>,
) -> Result<QuicServerHandle, Http3NativeError> {
    spawn_quic_server_with_batcher(
        quiche_config,
        server_config,
        bind_addr,
        EventBatcher::new_shared_tsfn(tsfn),
    )
}

#[cfg(feature = "os-runtime")]
pub(crate) fn spawn_quic_server_with_batcher(
    quiche_config: quiche::Config,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    batcher: EventBatcher,
) -> Result<QuicServerHandle, Http3NativeError> {
    let mut config_slot = Some(quiche_config);
    let mut batcher_slot = Some(batcher);
    spawn_quic_server_sharded(
        || {
            Ok(config_slot
                .take()
                .expect("called exactly once for 1 worker"))
        },
        server_config,
        bind_addr,
        1,
        |_| {
            batcher_slot
                .take()
                .expect("called exactly once for 1 worker")
        },
    )
}

/// Spawn `num_workers` server worker threads, each with its own socket bound
/// to the same address via SO_REUSEPORT.  The kernel distributes incoming
/// packets by 4-tuple hash, so each connection is handled by exactly one
/// worker.  Connection handles encode the worker index in the upper bits so
/// commands can be routed to the correct worker.
///
/// `make_quiche_config` is called once per worker (quiche::Config is not Clone).
/// `make_batcher` is called once per worker with the worker index.
#[cfg(feature = "os-runtime")]
pub(crate) fn spawn_quic_server_sharded<Q, B>(
    mut make_quiche_config: Q,
    server_config: QuicServerConfig,
    bind_addr: SocketAddr,
    num_workers: usize,
    mut make_batcher: B,
) -> Result<QuicServerHandle, Http3NativeError>
where
    Q: FnMut() -> Result<quiche::Config, Http3NativeError>,
    B: FnMut(usize) -> EventBatcher,
{
    assert!(num_workers >= 1, "need at least 1 worker");
    let use_reuse_port = num_workers > 1;

    // Bind the first socket to discover the actual local address (port 0 → ephemeral).
    let first_socket = if use_reuse_port {
        transport::socket::bind_worker_socket(bind_addr, true)?
    } else {
        let s = UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io)?;
        s.set_nonblocking(true).map_err(Http3NativeError::Io)?;
        let _ = transport::socket::set_socket_buffers(&s, 2 * 1024 * 1024);
        s
    };
    let local_addr = first_socket.local_addr().map_err(Http3NativeError::Io)?;

    // Query path MTU from the bound address.  If bound to a specific
    // interface (e.g. 127.0.0.1), this discovers the interface MTU.
    // If bound to 0.0.0.0, query_path_mtu returns None → fallback.
    let server_ceiling = if !local_addr.ip().is_unspecified() {
        crate::config::effective_pmtud_ceiling(&local_addr)
    } else {
        crate::config::FALLBACK_MAX_UDP_PAYLOAD
    };

    let mut workers = Vec::with_capacity(num_workers);

    // Worker 0 uses first_socket directly.
    {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (driver, waker) =
            transport::create_platform_driver(first_socket, server_config.runtime_mode)?;
        let batcher = make_batcher(0);
        let mut quiche_config = make_quiche_config()?;
        quiche_config.set_max_send_udp_payload_size(server_ceiling);
        quiche_config.set_max_recv_udp_payload_size(server_ceiling);
        let outbound_admission = Arc::new(OutboundAdmission::default());
        workers.push(spawn_server_worker_on_driver_with_admission(
            quiche_config,
            QuicServerConfig {
                qlog_dir: server_config.qlog_dir.clone(),
                qlog_level: server_config.qlog_level.clone(),
                max_connections: server_config.max_connections,
                disable_retry: server_config.disable_retry,
                client_auth: server_config.client_auth,
                cid_encoding: server_config.cid_encoding.clone(),
                runtime_mode: server_config.runtime_mode,
            },
            0,
            driver,
            waker,
            local_addr,
            cmd_tx,
            cmd_rx,
            batcher,
            outbound_admission,
        ));
    }

    // Workers 1..N bind new sockets to the same address via SO_REUSEPORT.
    for i in 1..num_workers {
        let socket = match transport::socket::bind_worker_socket(local_addr, true) {
            Ok(socket) => socket,
            Err(err) => {
                shutdown_spawned_quic_server_workers(&mut workers);
                return Err(err);
            }
        };
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (driver, waker) =
            match transport::create_platform_driver(socket, server_config.runtime_mode) {
                Ok(driver_and_waker) => driver_and_waker,
                Err(err) => {
                    shutdown_spawned_quic_server_workers(&mut workers);
                    return Err(err);
                }
            };
        let batcher = make_batcher(i);
        let mut quiche_config = match make_quiche_config() {
            Ok(config) => config,
            Err(err) => {
                shutdown_spawned_quic_server_workers(&mut workers);
                return Err(err);
            }
        };
        quiche_config.set_max_send_udp_payload_size(server_ceiling);
        quiche_config.set_max_recv_udp_payload_size(server_ceiling);
        let outbound_admission = Arc::new(OutboundAdmission::default());
        workers.push(spawn_server_worker_on_driver_with_admission(
            quiche_config,
            QuicServerConfig {
                qlog_dir: server_config.qlog_dir.clone(),
                qlog_level: server_config.qlog_level.clone(),
                max_connections: server_config.max_connections,
                disable_retry: server_config.disable_retry,
                client_auth: server_config.client_auth,
                cid_encoding: server_config.cid_encoding.clone(),
                runtime_mode: server_config.runtime_mode,
            },
            i as u32,
            driver,
            waker,
            local_addr,
            cmd_tx,
            cmd_rx,
            batcher,
            outbound_admission,
        ));
    }

    Ok(QuicServerHandle {
        workers,
        local_addr,
    })
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "os-runtime")]
#[cfg(feature = "node-api")]
pub fn spawn_quic_client(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    tsfn: std::sync::Arc<EventTsfn>,
) -> Result<QuicClientHandle, Http3NativeError> {
    spawn_quic_client_with_batcher(
        quiche_config,
        server_addr,
        server_name,
        session_ticket,
        qlog_dir,
        qlog_level,
        runtime_mode,
        EventBatcher::new_shared_tsfn(tsfn),
    )
}

#[cfg(feature = "os-runtime")]
pub(crate) fn spawn_quic_client_with_batcher(
    mut quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    batcher: EventBatcher,
) -> Result<QuicClientHandle, Http3NativeError> {
    // Query the path MTU to the server and raise the PMTUD ceiling if
    // the path supports larger packets (e.g. loopback = 16383, jumbo = 8972).
    let ceiling = crate::config::effective_pmtud_ceiling(&server_addr);
    quiche_config.set_max_send_udp_payload_size(ceiling);
    quiche_config.set_max_recv_udp_payload_size(ceiling);

    if default_quic_client_socket_strategy(runtime_mode) == ClientSocketStrategy::SharedPerFamily {
        return spawn_shared_quic_client(
            quiche_config,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            runtime_mode,
            batcher,
        );
    }

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let bind_addr = shared_client_bind_addr(server_addr);
    let (driver, waker, local_addr) =
        transport::prepare_client_platform_driver(bind_addr, runtime_mode)?;

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

#[cfg(feature = "os-runtime")]
struct SharedQuicClientSession {
    handler: QuicClientHandler,
    batcher: EventBatcher,
    server_addr: SocketAddr,
}

#[cfg(feature = "os-runtime")]
fn shared_quic_client_worker_registry()
-> &'static Mutex<HashMap<SharedQuicClientWorkerKey, Weak<SharedQuicClientWorkerControl>>> {
    static REGISTRY: OnceLock<
        Mutex<HashMap<SharedQuicClientWorkerKey, Weak<SharedQuicClientWorkerControl>>>,
    > = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(feature = "os-runtime")]
fn acquire_shared_quic_client_worker(
    server_addr: SocketAddr,
    runtime_mode: TransportRuntimeMode,
) -> Result<Arc<SharedQuicClientWorkerControl>, Http3NativeError> {
    let key = shared_client_worker_key(server_addr, runtime_mode);
    let mut registry = shared_quic_client_worker_registry().lock().map_err(|_| {
        Http3NativeError::InvalidState("shared quic client registry poisoned".into())
    })?;
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
    let outbound_admission = Arc::new(OutboundAdmission::default());
    let control = Arc::new(SharedQuicClientWorkerControl {
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
    reactor_metrics::record_worker_thread_spawn(WorkerSpawnKind::RawQuicClientShared);
    let join_handle = thread::spawn(move || {
        let mut driver = driver;
        run_shared_quic_client_event_loop(&mut driver, cmd_rx, local_addr, outbound_admission);
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
fn spawn_shared_quic_client(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    runtime_mode: TransportRuntimeMode,
    batcher: EventBatcher,
) -> Result<QuicClientHandle, Http3NativeError> {
    let worker = acquire_shared_quic_client_worker(server_addr, runtime_mode)?;

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
        .map_err(|_| {
            Http3NativeError::InvalidState("shared quic client worker not running".into())
        })?;
    worker.wake();

    let session_handle = resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
        Http3NativeError::InvalidState("timed out waiting for shared quic session".into())
    })??;
    worker.session_count.fetch_add(1, Ordering::AcqRel);

    Ok(QuicClientHandle {
        kind: Some(QuicClientHandleKind::Shared {
            session_handle,
            worker: Arc::clone(&worker),
        }),
        local_addr: worker.local_addr,
        outbound_admission: Arc::clone(&worker.outbound_admission),
    })
}

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
fn emit_shared_quic_client_write_ready(
    sessions: &mut Slab<SharedQuicClientSession>,
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
        reactor_metrics::record_lifecycle_trace(
            "quic-client",
            "shared-session-release",
            None,
            None,
            None,
            Some(format!(
                "conn_handle={handle} pending_writes={} blocked_streams={} known_streams={}",
                session.handler.pending_writes.len(),
                session.handler.conn.blocked_queue.len(),
                session.handler.conn.known_streams.len()
            )),
        );
        if !session.handler.session_closed_emitted {
            reactor_metrics::record_raw_quic_client_close_cause(RawQuicClientCloseCause::Release);
            reactor_metrics::record_session_close(SessionKind::RawQuicClient);
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

#[cfg(feature = "os-runtime")]
fn sync_shared_quic_client_timer(
    timer_heap: &mut TimerHeap,
    handle: usize,
    session: &SharedQuicClientSession,
) {
    shared_client_reactor::sync_timer(timer_heap, handle, session, |current| {
        current.handler.timer_deadline
    });
}

#[cfg(feature = "os-runtime")]
fn flush_shared_quic_client_sends(
    sessions: &mut Slab<SharedQuicClientSession>,
    handles_buf: &mut Vec<usize>,
    tx_pool: &mut BufferPool,
    outbound: &mut Vec<TxDatagram>,
) {
    shared_client_reactor::flush_round_robin_sends(sessions, handles_buf, outbound, |session| {
        QuicClientHandler::try_send_next_with_pool_parts(
            &mut session.handler.conn,
            session.handler.send_buf.as_mut_slice(),
            tx_pool,
        )
    });
}

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
fn run_shared_quic_client_event_loop<D: transport::Driver>(
    driver: &mut D,
    cmd_rx: crossbeam_channel::Receiver<SharedQuicClientCommand>,
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
    let mut sessions: Slab<SharedQuicClientSession> = Slab::new();
    let mut route_by_dcid: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut timer_heap = TimerHeap::new();
    let mut tx_pool = BufferPool::new(256, 65535);
    let mut handles_buf = Vec::new();
    let mut outbound = Vec::new();
    let mut closed_sessions = Vec::new();
    let mut release_requested = false;
    // Sessions queued by ReleaseSession; deferred until after the
    // per-iteration flush so the CONNECTION_CLOSE frame queued by a
    // preceding `Close` reaches the wire before the session is removed.
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

        let mut commands_drained = 0;
        while commands_drained < event_loop::MAX_COMMANDS_PER_TICK {
            let Ok(cmd) = cmd_rx.try_recv() else {
                break;
            };
            commands_drained += 1;
            let outbound_units = shared_quic_client_command_outbound_bytes(&cmd);
            reactor_metrics::record_outbound_command_dequeued(outbound_units);
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
                        Arc::clone(&outbound_admission),
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
                SharedQuicClientCommand::OpenStream {
                    session_handle,
                    resp_tx,
                } => {
                    let result = sessions
                        .get_mut(session_handle as usize)
                        .map(|session| session.handler.open_bidi_stream())
                        .unwrap_or_else(|| {
                            Err(Http3NativeError::InvalidState(
                                "shared quic client session not found".into(),
                            ))
                        });
                    let _ = resp_tx.send(result);
                }
                SharedQuicClientCommand::StreamSend {
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
                        emit_shared_quic_client_write_ready(&mut sessions, &mut pending_release);
                    }
                    if should_release {
                        pending_release.push(session_handle);
                    }
                }
                SharedQuicClientCommand::StreamClose {
                    session_handle,
                    stream_id,
                    error_code,
                } => {
                    if let Some(session) = sessions.get_mut(session_handle as usize) {
                        let released = session.handler.close_stream(stream_id, error_code);
                        if outbound_admission.release(released) {
                            emit_shared_quic_client_write_ready(
                                &mut sessions,
                                &mut pending_release,
                            );
                        }
                    }
                }
                SharedQuicClientCommand::SendDatagram {
                    session_handle,
                    data,
                    resp_tx,
                } => {
                    let ok = sessions
                        .get_mut(session_handle as usize)
                        .is_some_and(|session| session.handler.send_datagram(data));
                    if outbound_admission.release(outbound_units) {
                        emit_shared_quic_client_write_ready(&mut sessions, &mut pending_release);
                    }
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
                    // Defer until after flush so the CLOSE frame goes out
                    // before the session disappears.
                    release_requested = true;
                    pending_release.push(session_handle);
                }
            }
        }
        if commands_drained == event_loop::MAX_COMMANDS_PER_TICK && !cmd_rx.is_empty() {
            poll_now = true;
        }

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

        // Finalize sessions queued for release: emit SHUTDOWN_COMPLETE so
        // JS-side close() resolves, then remove from routing maps. CLOSE
        // frames have already gone out via the flush + submit above.
        for session_handle in pending_release.drain(..) {
            if let Some(session) = sessions.get_mut(session_handle as usize) {
                event_loop::push_shutdown_complete(&mut session.batcher);
                let _ = session.batcher.flush();
            }
            remove_shared_quic_client_session(
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
            if let Ok(header) = quiche::Header::from_slice(data.as_mut_slice(), SCID_LEN) {
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
                                refresh_shared_quic_client_dcid(
                                    &mut route_by_dcid,
                                    handle,
                                    session,
                                );
                                sync_shared_quic_client_timer(&mut timer_heap, handle, session);
                                if session.batcher.len() >= MAX_BATCH_SIZE
                                    && !session.batcher.flush()
                                {
                                    should_remove = true;
                                }
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
                }
            }
            rx_recycled.push(data);
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
                    refresh_shared_quic_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_quic_client_timer(&mut timer_heap, handle, session);
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
                    refresh_shared_quic_client_dcid(&mut route_by_dcid, handle, session);
                    sync_shared_quic_client_timer(&mut timer_heap, handle, session);
                }
                let app_budget = event_loop::app_event_budget(session.batcher.len());
                let drain_ok = app_ok
                    && session.batcher.collect_atomic(|batch| {
                        session.handler.poll_drain_events_for_handle(
                            app_budget,
                            batch,
                            handle as u32,
                        );
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
            remove_shared_quic_client_session(
                &mut sessions,
                &mut route_by_dcid,
                &mut timer_heap,
                handle,
            );
        }

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
            // QuicClientEventLoop.close() await resolves on auto-reap
            // (peer CONNECTION_CLOSE etc.) just like on explicit release.
            if let Some(session) = sessions.get_mut(handle) {
                event_loop::push_shutdown_complete(&mut session.batcher);
                let _ = session.batcher.flush();
            }
            remove_shared_quic_client_session(
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

// ── QUIC Server Protocol Handler ────────────────────────────────────

/// Raw QUIC server protocol state machine — mirrors `H3ServerHandler`
/// exactly (see that type's doc comment for the full rationale); a
/// `QuicConnectionMap` (many connections) + `TimerHeap` instead of H3's
/// per-stream framing state.
pub struct QuicServerHandler {
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
    /// Offset added to local slab handles before emitting events, so that
    /// connection handles are globally unique across sharded workers.
    /// Worker 0 uses offset 0, worker 1 uses `1 << WORKER_SHIFT`, etc.
    handle_offset: u32,
    chunk_pool: ChunkPool,
    chunk_pool_rx: crossbeam_channel::Receiver<Vec<u8>>,
    outbound_admission: Arc<OutboundAdmission>,
}

impl QuicServerHandler {
    /// Native constructor: sources the `QuicConnectionMap`'s 32-byte
    /// HMAC/SCID key from `ring`'s system RNG. Requires `os-runtime` for
    /// the same reason as `H3ServerHandler::new`.
    #[cfg(feature = "os-runtime")]
    fn new(
        quiche_config: quiche::Config,
        server_config: QuicServerConfig,
        worker_index: u32,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let conn_map = QuicConnectionMap::new(
            server_config.max_connections,
            server_config.cid_encoding.clone(),
        );
        Self::from_parts(
            quiche_config,
            server_config,
            conn_map,
            server_sharding::handle_offset(worker_index),
            outbound_admission,
        )
    }

    /// Direct-call constructor for a sans-IO caller (a wasm ABI, or the
    /// unit tests below) — mirrors `H3ServerHandler::new_direct` exactly.
    /// `handle_offset` is always `0`.
    pub fn new_direct(
        quiche_config: quiche::Config,
        server_config: QuicServerConfig,
        retry_token_key: [u8; 32],
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let conn_map = QuicConnectionMap::with_key_bytes(
            server_config.max_connections,
            server_config.cid_encoding.clone(),
            retry_token_key,
        );
        Self::from_parts(quiche_config, server_config, conn_map, 0, outbound_admission)
    }

    fn from_parts(
        quiche_config: quiche::Config,
        server_config: QuicServerConfig,
        conn_map: QuicConnectionMap,
        handle_offset: u32,
        outbound_admission: Arc<OutboundAdmission>,
    ) -> Self {
        let disable_retry = server_config.disable_retry;
        let (chunk_pool, _chunk_pool_return, chunk_pool_rx) = ChunkPool::with_return_channel(64);
        Self {
            conn_map,
            timer_heap: TimerHeap::new(),
            buffer_pool: BufferPool::default(),
            tx_pool: BufferPool::new(256, 65535),
            pending_writes: HashMap::new(),
            conn_send_buffers: HashMap::new(),
            handles_buf: Vec::new(),
            server_config,
            quiche_config,
            disable_retry,
            last_expired: Vec::new(),
            handle_offset,
            chunk_pool,
            chunk_pool_rx,
            outbound_admission,
        }
    }

    /// Convert a local slab handle to a global conn_handle with worker bits.
    fn global_handle(&self, local: usize) -> u32 {
        self.handle_offset | (local as u32)
    }

    fn release_outbound_admission(&self, units: usize, batch: &mut Vec<JsH3Event>) {
        if self.outbound_admission.release(units) {
            batch.push(JsH3Event::write_ready(0));
        }
    }

    /// Soonest quiche timeout deadline across every connection this
    /// server holds, or `None` — mirrors `H3ServerHandler::soonest_deadline`
    /// (raw QUIC has no `pending_session_closes` concept, so this is just
    /// the timer heap).
    pub fn soonest_deadline(&mut self) -> Option<Instant> {
        self.timer_heap.next_deadline()
    }

    /// `true` once there are no live connections — the "is the whole
    /// server done" direct-call query.
    pub fn is_idle(&self) -> bool {
        self.conn_map.is_empty()
    }

    /// `true` if `conn_handle` refers to a connection that is closed (or
    /// no longer tracked at all) — mirrors
    /// `H3ServerHandler::connection_is_closed`.
    pub fn connection_is_closed(&self, conn_handle: u32) -> bool {
        self.conn_map
            .get(conn_handle as usize)
            .is_none_or(QuicConnection::is_closed)
    }

    /// Number of live connections currently tracked.
    pub fn connection_count(&self) -> usize {
        self.conn_map.len()
    }

    /// Graceful whole-server shutdown (the `qs_shutdown` direct-call
    /// operation) — mirrors `H3ServerHandler::shutdown_all_connections`
    /// exactly (same `WorkerCommand`/`QuicServerCommand::Shutdown` match
    /// arm body in the native path).
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

    /// Direct-call packet routing (the `qs_recv` primitive) — a
    /// **parallel implementation** of the native
    /// `ProtocolHandler::process_packet` below, for the identical reason
    /// `H3ServerHandler::process_inbound_packet` is: fresh SCIDs come from
    /// [`QuicConnectionMap::generate_scid_direct`] (HMAC-PRF, no `ring`)
    /// instead of [`QuicConnectionMap::generate_scid`] (`ring`-backed),
    /// and `top_up_server_scids` (connection-migration SCID issuance,
    /// itself `ring`-backed) is deliberately not called — see that
    /// method's doc comment for the full rationale, which applies
    /// identically here. The client-certificate-required enforcement
    /// below is **not** new here (unlike the H3 side) — native's raw QUIC
    /// server already has it; this just carries it over unchanged.
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
                    self.server_config.qlog_dir.as_deref(),
                    self.server_config.qlog_level.as_deref(),
                ) {
                    Ok(h) => {
                        self.conn_map.add_dcid(h, client_dcid);
                        reactor_metrics::record_session_open(SessionKind::RawQuicServer);
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
                            self.server_config.qlog_dir.as_deref(),
                            self.server_config.qlog_level.as_deref(),
                        ) {
                            Ok(h) => {
                                self.conn_map.add_dcid(h, odcid);
                                reactor_metrics::record_session_open(SessionKind::RawQuicServer);
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
                    pending_outbound.push(TxDatagram::new(out[..len].to_vec(), len, peer, None));
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
                        offset | (handle as u32),
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

            conn.poll_quic_events(offset | (handle as u32), app_event_budget, batch);
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

    /// Expire due timers across every connection (the `qs_on_timeout`
    /// primitive), sweeping for late FINs on every connection too (raw
    /// QUIC has no HEADERS-based stream-finished signal — see
    /// `sweep_finished_streams`). Hoisted from
    /// `ProtocolHandler::process_timers` below.
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
                        "quic-server",
                        "session-close-timeout",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle={} blocked_streams={} known_streams={}",
                            offset | (handle as u32),
                            conn.blocked_set.len(),
                            conn.known_streams.len()
                        )),
                    );
                    reactor_metrics::record_session_close(SessionKind::RawQuicServer);
                    batch.push(conn.session_close_event(offset | (handle as u32)));
                } else {
                    conn.poll_quic_events(offset | (handle as u32), app_event_budget, batch);
                    for duration_ms in conn.poll_ping_acks() {
                        batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
                    }
                    self.timer_heap
                        .set_deadline(handle, conn.timeout().map(|timeout| now + timeout));
                }
            }
        }
        // Sweep ALL connections for late FINs (not just expired ones).
        self.conn_map.fill_handles(&mut self.handles_buf);
        for i in 0..self.handles_buf.len() {
            let handle = self.handles_buf[i];
            if let Some(conn) = self.conn_map.get_mut(handle) {
                if app_event_budget > 0 {
                    conn.sweep_finished_streams(offset | (handle as u32), app_event_budget, batch);
                }
            }
        }
    }

    /// Poll protocol/application events already buffered inside quiche for
    /// every connection. Hoisted from `ProtocolHandler::poll_app_events`
    /// below.
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
                conn.poll_quic_events(offset | (handle as u32), remaining, batch);
                for duration_ms in conn.poll_ping_acks() {
                    batch.push(JsH3Event::ping_ack(offset | (handle as u32), duration_ms));
                }
                remaining = app_event_budget.saturating_sub(batch.len());
            }
        }
    }

    /// Write every connection's next outbound datagram, round-robin.
    /// Hoisted from `ProtocolHandler::flush_sends` below.
    pub fn flush_all_sends(&mut self, outbound: &mut Vec<TxDatagram>) {
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
                    conn.drain_outbound_datagrams();
                    // Write directly into a pool buffer — no intermediate copy.
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

    /// Retry buffered partial stream writes where flow control has
    /// opened, pushing `drain` events. Hoisted from
    /// `ProtocolHandler::flush_pending_writes` below.
    pub fn flush_all_pending_writes(&mut self, batch: &mut Vec<JsH3Event>) {
        let flushed = flush_quic_pending_writes(&mut self.conn_map, &mut self.pending_writes);
        self.release_outbound_admission(flushed.released_units, batch);
        for (local_handle, stream_id) in flushed {
            reactor_metrics::record_raw_quic_drain_event();
            batch.push(JsH3Event::drain(
                local_handle | self.handle_offset,
                stream_id,
            ));
        }
    }

    /// Check blocked streams for writability, pushing `drain` events, and
    /// sweep every connection for late FINs. Hoisted from
    /// `ProtocolHandler::poll_drain_events` below.
    pub fn collect_drain_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        self.conn_map.fill_handles(&mut self.handles_buf);
        for i in 0..self.handles_buf.len() {
            let handle = self.handles_buf[i];
            if let Some(conn) = self.conn_map.get_mut(handle) {
                // Sweep for FIN events that arrived in a separate packet
                // after data was already drained.  Run unconditionally —
                // process_timers doesn't sweep non-expired connections.
                if app_event_budget > 0 {
                    conn.sweep_finished_streams(offset | (handle as u32), app_event_budget, batch);
                }
            }
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
    /// `ProtocolHandler::recycle_tx_buffers` below.
    pub fn recycle_tx_buffers_into_pool(&mut self, buffers: Vec<Vec<u8>>) {
        reactor_metrics::record_tx_buffers_recycled(buffers.len());
        for buf in buffers {
            self.tx_pool.checkin(buf);
        }
    }

    /// Remove closed connections, pushing final `session_close` events.
    /// Hoisted from `ProtocolHandler::cleanup_closed` below.
    pub fn reap_closed_connections(&mut self, batch: &mut Vec<JsH3Event>) {
        let offset = self.handle_offset;
        let closed = self.conn_map.drain_closed();
        for (handle, conn) in closed {
            self.timer_heap.remove_connection(handle);
            self.conn_send_buffers.remove(&handle);
            // Audit finding #12: emit a reset for every abandoned stream
            // before dropping its PendingWrite, so JS-side write callbacks
            // fire (via stream.destroy in _onReset) instead of hanging.
            let abandoned: Vec<u64> = self
                .pending_writes
                .keys()
                .filter(|&&(ch, _)| ch as usize == handle)
                .map(|&(_, sid)| sid)
                .collect();
            for stream_id in abandoned {
                batch.push(JsH3Event::reset(
                    offset | (handle as u32),
                    stream_id,
                    0, // raw QUIC: app error 0 — connection-level close
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
            reactor_metrics::record_outbound_pending_write_removed(removed_bytes);
            self.release_outbound_admission(removed_units, batch);
            if !self.last_expired.contains(&handle) {
                reactor_metrics::record_lifecycle_trace(
                    "quic-server",
                    "session-close-cleanup",
                    None,
                    None,
                    None,
                    Some(format!("conn_handle={}", offset | (handle as u32))),
                );
                reactor_metrics::record_session_close(SessionKind::RawQuicServer);
                batch.push(conn.session_close_event(offset | (handle as u32)));
            }
        }
        self.last_expired.clear();
    }

    // ── Per-connection direct-call operations ───────────────────────
    // Mirrors `H3ServerHandler`'s equivalent section exactly — hoisted
    // from `QuicServerCommand`'s match arms in `dispatch_command` above,
    // minus the `release_outbound_admission`/`EVENT_WRITE_READY` calls
    // (native-worker-thread-only concern; see that section's doc comment
    // for the full rationale).

    /// Queue body bytes (and optionally FIN) for `stream_id` on
    /// `conn_handle` (the `qs_stream_send` primitive) — returns admitted
    /// bytes, or `0` if fully backpressured / the connection no longer
    /// exists. Mirrors `H3ServerHandler::queue_stream_send` (no
    /// `pending_responses` concept in raw QUIC).
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
        match conn.stream_send_chunk(stream_id, chunk, fin) {
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
                batch.push(JsH3Event::error(
                    event_conn_handle,
                    stream_id as i64,
                    0,
                    format!("stream send failed: {e}"),
                ));
                outbound_units
            }
        }
    }

    /// Close `stream_id` on `conn_handle` with `error_code` (the
    /// `qs_stream_close` primitive).
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
                let released = remove_pending_write(&mut self.pending_writes, &(conn_handle, stream_id));
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
                    "quic stream_close failed conn_handle={conn_handle} stream_id={stream_id} error_code={error_code}: {e}"
                );
                0
            }
        }
    }

    /// Send a QUIC DATAGRAM on `conn_handle`.
    pub fn send_datagram(&mut self, conn_handle: u32, data: Chunk) -> bool {
        self.conn_map
            .get_mut(conn_handle as usize)
            .is_some_and(|conn| conn.queue_datagram(data).unwrap_or(false))
    }

    /// Snapshot session metrics for `conn_handle`, or `None` if it no
    /// longer exists (the `qs_session_metrics` primitive).
    pub fn session_metrics(&self, conn_handle: u32) -> Option<JsSessionMetrics> {
        self.conn_map
            .get(conn_handle as usize)
            .map(snapshot_quic_metrics)
    }

    /// Queue a PING on `conn_handle`.
    pub fn ping(&mut self, conn_handle: u32) -> bool {
        self.conn_map
            .get_mut(conn_handle as usize)
            .is_some_and(|conn| conn.queue_ping().is_ok())
    }

    /// Immediately close a single connection with a CONNECTION_CLOSE (the
    /// `qs_close_connection` primitive) — raw QUIC has no GOAWAY concept,
    /// so (unlike `H3ServerHandler::close_connection`) this is not
    /// deferred; mirrors `QuicServerCommand::CloseSession`'s match arm
    /// exactly.
    pub fn close_connection(&mut self, conn_handle: u32, error_code: u32, reason: &str) {
        if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
            let _ = conn
                .quiche_conn
                .close(true, u64::from(error_code), reason.as_bytes());
        }
    }
}

#[cfg(feature = "os-runtime")]
impl ProtocolHandler for QuicServerHandler {
    type Command = QuicServerCommand;

    fn dispatch_command(&mut self, cmd: QuicServerCommand, batch: &mut Vec<JsH3Event>) -> bool {
        let outbound_units = quic_server_command_outbound_bytes(&cmd);
        reactor_metrics::record_outbound_command_dequeued(outbound_units);
        match cmd {
            QuicServerCommand::Shutdown => {
                // Audit finding #11: close every live connection with an
                // application-level CONNECTION_CLOSE so peers see a graceful
                // shutdown instead of waiting on idle timeout.
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
            QuicServerCommand::StreamSend {
                conn_handle,
                stream_id,
                chunk,
                fin,
            } => {
                let key = (conn_handle, stream_id);
                if let Some(pw) = self.pending_writes.get_mut(&key) {
                    reactor_metrics::record_outbound_pending_write_added(pw.push_chunk(chunk));
                    if fin {
                        pw.set_fin();
                    }
                    // chunk backing allocation stays queued via ArcBuf.
                } else if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    let payload_len = chunk.remaining_len();
                    match conn.stream_send_chunk(stream_id, chunk, fin) {
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
                                batch.push(JsH3Event::stream_blocked(conn_handle, stream_id));
                            }
                        }
                        Err(e) => {
                            self.release_outbound_admission(outbound_units, batch);
                            batch.push(JsH3Event::error(
                                conn_handle,
                                stream_id as i64,
                                0,
                                format!("stream send failed: {e}"),
                            ));
                        }
                    }
                } else {
                    self.release_outbound_admission(outbound_units, batch);
                }
            }
            QuicServerCommand::StreamClose {
                conn_handle,
                stream_id,
                error_code,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    match conn.stream_close(stream_id, u64::from(error_code)) {
                        Ok(()) => {
                            let released = remove_pending_write(
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
                                "quic stream_close failed conn_handle={conn_handle} stream_id={stream_id} error_code={error_code}: {e}"
                            );
                        }
                    }
                }
            }
            QuicServerCommand::CloseSession {
                conn_handle,
                error_code,
                reason,
            } => {
                if let Some(conn) = self.conn_map.get_mut(conn_handle as usize) {
                    reactor_metrics::record_lifecycle_trace(
                        "quic-server",
                        "close-session-requested",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle={conn_handle} error_code={error_code} blocked_streams={} known_streams={} reason={}",
                            conn.blocked_set.len(),
                            conn.known_streams.len(),
                            reason.as_str()
                        )),
                    );
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
                    .is_some_and(|conn| conn.queue_datagram(data).unwrap_or(false));
                self.release_outbound_admission(outbound_units, batch);
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
                    .is_some_and(|conn| conn.queue_ping().is_ok());
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
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
    ) {
        let offset = self.handle_offset;
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
                            self.server_config.qlog_dir.as_deref(),
                            self.server_config.qlog_level.as_deref(),
                        ) {
                            Ok(h) => {
                                self.conn_map.add_dcid(h, odcid);
                                reactor_metrics::record_session_open(SessionKind::RawQuicServer);
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
                    pending_outbound.push(TxDatagram::new(out[..len].to_vec(), len, peer, None));
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
                        offset | (handle as u32),
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

            conn.poll_quic_events(offset | (handle as u32), app_event_budget, batch);
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
        self.chunk_pool.drain_returned(&self.chunk_pool_rx);
        self.conn_map.fill_handles(&mut self.handles_buf);
        for &handle in &self.handles_buf {
            if let Some(conn) = self.conn_map.get_mut(handle) {
                conn.drain_recycled();
            }
        }
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        self.recycle_tx_buffers_into_pool(buffers);
    }

    fn cleanup_closed(&mut self, batch: &mut Vec<JsH3Event>) {
        self.reap_closed_connections(batch);
    }

    fn emit_session_close_for_all_active(&mut self, batch: &mut Vec<JsH3Event>) {
        // Audit finding #34: ensure JS-side sessions close on driver
        // runtime errors instead of staying open forever.
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

// ── QUIC Client Protocol Handler ────────────────────────────────────

/// Raw QUIC client protocol state machine (no HTTP/3 framing): a single
/// connection's worth of quiche state plus pending-write/chunk-pool
/// bookkeeping.
///
/// Kept always-compiled (no `os-runtime` gate) and `pub` (re-exported by
/// `wasm_exports` under `wasm-abi`) — see the identical note on
/// [`crate::worker::H3ClientHandler`]. Only [`QuicClientHandler::new`] (the
/// ring-backed SCID native constructor) is `os-runtime`-gated;
/// [`QuicClientHandler::new_direct`] is the always-compiled alternative.
pub struct QuicClientHandler {
    conn: QuicConnection,
    pending_writes: HashMap<u64, PendingWrite>,
    next_bidi_stream_id: u64,
    send_buf: Vec<u8>,
    tx_pool: BufferPool,
    timer_deadline: Option<Instant>,
    session_closed_emitted: bool,
    chunk_pool: ChunkPool,
    chunk_pool_rx: crossbeam_channel::Receiver<Vec<u8>>,
    outbound_admission: Arc<OutboundAdmission>,
    keylog: Option<KeylogBuffer>,
}

impl QuicClientHandler {
    /// Native constructor: sources the SCID from `ring`'s system RNG.
    /// Requires `os-runtime` for the same reason as
    /// `H3ClientHandler::new` — every native call site already runs with
    /// `os-runtime` on (it is in the default feature set).
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
        let Ok(scid) = CidEncoding::random().generate_scid() else {
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
    /// directly instead of requiring `ring`.
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
        let (chunk_pool, _chunk_pool_return, chunk_pool_rx) = ChunkPool::with_return_channel(64);
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            next_bidi_stream_id: 0,
            send_buf: vec![0u8; SEND_BUF_SIZE],
            tx_pool: BufferPool::new(256, 65535),
            timer_deadline,
            session_closed_emitted: false,
            chunk_pool,
            chunk_pool_rx,
            outbound_admission,
            keylog: None,
        })
    }

    /// Enable in-memory NSS-format keylog capture (A2 task 5); see
    /// `H3ClientHandler::enable_keylog` for the full rationale.
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
            "quic-client",
            "close-session-requested",
            None,
            None,
            None,
            Some(format!(
                "conn_handle=0 error_code={error_code} pending_writes={} blocked_streams={} known_streams={} reason={reason}",
                self.pending_writes.len(),
                self.conn.blocked_set.len(),
                self.conn.known_streams.len()
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

    pub fn open_bidi_stream(&mut self) -> Result<u64, Http3NativeError> {
        let stream_id = self.next_bidi_stream_id;
        let next_stream_id = stream_id.checked_add(4).ok_or_else(|| {
            Http3NativeError::InvalidState("client bidirectional stream id overflow".into())
        })?;
        self.conn.reserve_local_bidi_stream(stream_id)?;
        self.next_bidi_stream_id = next_stream_id;
        Ok(stream_id)
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
        match self.conn.stream_send_chunk(stream_id, chunk, fin) {
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
                    reactor_metrics::record_raw_quic_client_pending_writes(
                        self.pending_writes.len(),
                    );
                    batch.push(JsH3Event::stream_blocked(conn_handle, stream_id));
                }
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
            Ok(()) => {
                let released = remove_pending_write(&mut self.pending_writes, &stream_id);
                reactor_metrics::record_raw_quic_client_pending_writes(self.pending_writes.len());
                released
            }
            Err(e) => {
                log::debug!(
                    "quic client stream_close failed stream_id={stream_id} error_code={error_code}: {e}"
                );
                0
            }
        }
    }

    pub fn send_datagram(&mut self, data: Chunk) -> bool {
        self.conn.queue_datagram(data).unwrap_or(false)
    }

    pub fn metrics_snapshot(&self) -> JsSessionMetrics {
        snapshot_quic_metrics(&self.conn)
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

    fn emit_session_close(
        &mut self,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
        cause: RawQuicClientCloseCause,
    ) {
        if self.session_closed_emitted {
            return;
        }
        let peer_error = self.conn.quiche_conn.peer_error();
        let local_error = self.conn.quiche_conn.local_error();
        log::warn!(
            "quic-client session close: cause={cause:?} peer_error={peer_error:?} \
             local_error={local_error:?} pending_writes={} blocked={} known={}",
            self.pending_writes.len(),
            self.conn.blocked_set.len(),
            self.conn.known_streams.len(),
        );
        reactor_metrics::record_lifecycle_trace(
            "quic-client",
            "session-close-emitted",
            None,
            None,
            None,
            Some(format!(
                "conn_handle={conn_handle} cause={cause:?} pending_writes={} blocked_streams={} known_streams={}",
                self.pending_writes.len(),
                self.conn.blocked_set.len(),
                self.conn.known_streams.len()
            )),
        );
        reactor_metrics::record_raw_quic_client_close_cause(cause);
        reactor_metrics::record_session_close(SessionKind::RawQuicClient);
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
        reactor_metrics::record_raw_quic_client_pending_writes(0);
        self.release_outbound_admission(removed_units, batch);
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
        if self.conn.quiche_conn.is_established() && !self.conn.is_established {
            self.conn.mark_established();
        }
        if self.conn.quiche_conn.is_established() && !self.conn.handshake_complete_emitted {
            self.conn.handshake_complete_emitted = true;
            batch.push(JsH3Event::handshake_complete(conn_handle));
        }
        self.conn
            .poll_quic_events(conn_handle, app_event_budget, batch);
        for duration_ms in self.conn.poll_ping_acks() {
            batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
        }
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
        self.timer_deadline = self.conn.timeout().map(|t| Instant::now() + t);

        if self.conn.is_closed() && !self.session_closed_emitted {
            self.emit_session_close(batch, conn_handle, RawQuicClientCloseCause::Packet);
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
            if self.conn.is_closed() && !self.session_closed_emitted {
                self.emit_session_close(batch, conn_handle, RawQuicClientCloseCause::Timeout);
            } else {
                self.conn
                    .poll_quic_events(conn_handle, app_event_budget, batch);
                for duration_ms in self.conn.poll_ping_acks() {
                    batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
                }
                if app_event_budget > 0 {
                    self.conn
                        .sweep_finished_streams(conn_handle, app_event_budget, batch);
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
            .poll_quic_events(conn_handle, app_event_budget, batch);
        for duration_ms in self.conn.poll_ping_acks() {
            batch.push(JsH3Event::ping_ack(conn_handle, duration_ms));
        }
        if let Some(ticket) = self.conn.update_session_ticket() {
            batch.push(JsH3Event::session_ticket(conn_handle, ticket));
        }
    }

    /// Write the next outbound datagram, if any (the `flush_sends`
    /// primitive, one packet at a time).
    pub fn try_send_next(&mut self) -> Option<TxDatagram> {
        let result = Self::try_send_next_with_pool_parts(
            &mut self.conn,
            self.send_buf.as_mut_slice(),
            &mut self.tx_pool,
        );
        if result.is_none() {
            // Deviation (Phase 3 wasm-plan discovery) — see worker.rs's
            // identical fix + comment on H3ClientHandler::try_send_next
            // for the full rationale.
            self.refresh_timeout_deadline();
        }
        result
    }

    fn try_send_next_with_pool_parts(
        conn: &mut QuicConnection,
        _send_buf: &mut [u8],
        tx_pool: &mut BufferPool,
    ) -> Option<TxDatagram> {
        conn.drain_outbound_datagrams();
        // Write directly into pool buffer — no intermediate copy.
        let mut tx_buf = tx_pool.checkout();
        let Ok((len, send_info)) = conn.send(tx_buf.as_mut_slice()) else {
            tx_pool.checkin(tx_buf);
            return None;
        };
        let mtu = u16::try_from(conn.quiche_conn.max_send_udp_payload_size()).ok();
        Some(TxDatagram::new(tx_buf, len, send_info.to, mtu))
    }

    pub fn flush_pending_writes_for_handle(&mut self, batch: &mut Vec<JsH3Event>, conn_handle: u32) {
        let flushed = flush_quic_client_pending_writes(&mut self.conn, &mut self.pending_writes);
        self.release_outbound_admission(flushed.released_units, batch);
        for stream_id in flushed {
            reactor_metrics::record_raw_quic_drain_event();
            batch.push(JsH3Event::drain(conn_handle, stream_id));
        }
    }

    pub fn poll_drain_events_for_handle(
        &mut self,
        app_event_budget: usize,
        batch: &mut Vec<JsH3Event>,
        conn_handle: u32,
    ) {
        if app_event_budget > 0 {
            self.conn
                .sweep_finished_streams(conn_handle, app_event_budget, batch);
        }
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
        self.session_closed_emitted && self.pending_writes.is_empty()
    }

    /// Soonest quiche timeout deadline, or `None` (the "next timeout"
    /// direct-call operation).
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timer_deadline
    }
}

impl ProtocolHandler for QuicClientHandler {
    type Command = QuicClientCommand;

    fn dispatch_command(&mut self, cmd: QuicClientCommand, batch: &mut Vec<JsH3Event>) -> bool {
        let outbound_units = quic_client_command_outbound_bytes(&cmd);
        reactor_metrics::record_outbound_command_dequeued(outbound_units);
        match cmd {
            QuicClientCommand::Shutdown => {
                if !self.session_closed_emitted {
                    reactor_metrics::record_lifecycle_trace(
                        "quic-client",
                        "shutdown-command",
                        None,
                        None,
                        None,
                        Some(format!(
                            "conn_handle=0 pending_writes={} blocked_streams={} known_streams={}",
                            self.pending_writes.len(),
                            self.conn.blocked_set.len(),
                            self.conn.known_streams.len()
                        )),
                    );
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
            QuicClientCommand::OpenStream { resp_tx } => {
                let _ = resp_tx.send(self.open_bidi_stream());
            }
            QuicClientCommand::StreamSend {
                stream_id,
                chunk,
                fin,
            } => {
                let released = self.queue_stream_send(stream_id, chunk, fin, batch, 0);
                self.release_outbound_admission(released, batch);
            }
            QuicClientCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                let released = self.close_stream(stream_id, error_code);
                self.release_outbound_admission(released, batch);
            }
            QuicClientCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.send_datagram(data));
                self.release_outbound_admission(outbound_units, batch);
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

    fn poll_drain_events(&mut self, app_event_budget: usize, batch: &mut Vec<JsH3Event>) {
        self.poll_drain_events_for_handle(app_event_budget, batch, 0);
    }

    fn drain_recycled_buffers(&mut self) {
        self.chunk_pool.drain_returned(&self.chunk_pool_rx);
        self.conn.drain_recycled();
    }

    fn recycle_tx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        self.recycle_tx_buffers_into_pool(buffers);
    }

    fn cleanup_closed(&mut self, _batch: &mut Vec<JsH3Event>) {
        // Client session_close is emitted in process_packet / process_timers.
    }

    fn emit_session_close_for_all_active(&mut self, batch: &mut Vec<JsH3Event>) {
        // Audit finding #34: client side has a single session at handle 0.
        // Skip if we've already emitted to avoid a duplicate close.
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

// ── Helpers ────────────────────────────────────────────────────────

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
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
        pmtu: conn.pmtu() as i64,
        datagram_queue_depth: conn.outbound_datagram_queue_len() as u32,
    }
}

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

/// Always compiled — used by both native's
/// `ProtocolHandler::flush_pending_writes` (via `flush_all_pending_writes`)
/// and the direct-call surface, since `QuicConnectionMap` itself no
/// longer requires `os-runtime`.
fn flush_quic_pending_writes(
    conn_map: &mut QuicConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
) -> PendingWriteFlushEvents<(u32, u64)> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&(conn_handle, stream_id), pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        };
        match flush_one_quic_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push((conn_handle, stream_id));
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
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
                false
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

fn flush_quic_client_pending_writes(
    conn: &mut QuicConnection,
    pending: &mut HashMap<u64, PendingWrite>,
) -> PendingWriteFlushEvents<u64> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&stream_id, pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        match flush_one_quic_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push(stream_id);
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
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
                false
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

fn flush_one_quic_pending_write(
    conn: &mut QuicConnection,
    stream_id: u64,
    pw: &mut PendingWrite,
) -> Result<PendingWriteFlushOutcome, Http3NativeError> {
    flush_pending_write_with_progress(pw, |buf, send_fin| {
        let outcome = conn.stream_send_arcbuf(stream_id, buf, send_fin)?;
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
    use std::sync::MutexGuard;

    fn setup_metrics() -> MutexGuard<'static, ()> {
        let guard = reactor_metrics::test_metrics_guard();
        reactor_metrics::reset();
        guard
    }

    #[test]
    fn quic_pending_write_byte_accounting_tracks_queue_lifecycle() {
        let _guard = setup_metrics();
        let mut pending = HashMap::new();

        insert_pending_write(
            &mut pending,
            11_u64,
            PendingWrite::new(ArcBuf::from_vec(vec![0; 3]), false),
        );
        assert_eq!(reactor_metrics::snapshot().outboundPendingWriteBytes, 3);

        let write = pending.get_mut(&11).expect("pending write exists");
        reactor_metrics::record_outbound_pending_write_added(
            write.push_chunk(Chunk::unpooled(vec![0; 8])),
        );
        let snap = reactor_metrics::snapshot();
        assert_eq!(snap.outboundPendingWriteBytes, 11);
        assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 11);

        assert_eq!(remove_pending_write(&mut pending, &11), 11);
        let snap = reactor_metrics::snapshot();
        assert_eq!(snap.outboundPendingWriteBytes, 0);
        assert_eq!(snap.outboundPendingWriteBytesHighWatermark, 11);
    }

    #[test]
    fn quic_command_outbound_bytes_reads_unflattened_chunks_client() {
        let client_cmd = QuicClientCommand::StreamSend {
            stream_id: 4,
            chunk: Chunk::unpooled(vec![2; 12]),
            fin: true,
        };
        let (client_resp_tx, _client_resp_rx) = crossbeam_channel::bounded(1);
        let client_datagram_cmd = QuicClientCommand::SendDatagram {
            data: Chunk::unpooled(vec![4; 15]),
            resp_tx: client_resp_tx,
        };

        assert_eq!(quic_client_command_outbound_bytes(&client_cmd), 12);
        assert_eq!(quic_client_command_outbound_bytes(&client_datagram_cmd), 15);
    }

    #[cfg(feature = "os-runtime")]
    #[test]
    fn quic_command_outbound_bytes_reads_unflattened_chunks_server() {
        let server_cmd = QuicServerCommand::StreamSend {
            conn_handle: 1,
            stream_id: 2,
            chunk: Chunk::unpooled(vec![1; 7]),
            fin: false,
        };
        let (server_resp_tx, _server_resp_rx) = crossbeam_channel::bounded(1);
        let server_datagram_cmd = QuicServerCommand::SendDatagram {
            conn_handle: 1,
            data: Chunk::unpooled(vec![3; 14]),
            resp_tx: server_resp_tx,
        };

        assert_eq!(quic_server_command_outbound_bytes(&server_cmd), 7);
        assert_eq!(quic_server_command_outbound_bytes(&server_datagram_cmd), 14);
    }

    // ── A2 task 6: direct-call surface tests (QuicClientHandler::new_direct) ──
    //
    // Mirrors `worker.rs`'s `direct_call_h3` module exactly (see the doc
    // comments there for the rationale behind the pump helpers), but drives
    // the raw-QUIC handler surface: `open_bidi_stream`/`queue_stream_send`
    // instead of `send_request`.
    mod direct_call_quic {
        use super::*;
        use crate::h3_event::{EVENT_HANDSHAKE_COMPLETE, EVENT_SESSION_CLOSE};
        use std::net::{IpAddr, Ipv4Addr};

        const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

        fn test_addrs() -> (SocketAddr, SocketAddr) {
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 42_001),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52_001),
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
                std::env::temp_dir().join(format!("quic_direct_test_cert_{id:?}.pem"));
            let key_path = std::env::temp_dir().join(format!("quic_direct_test_key_{id:?}.pem"));
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
                .set_application_protos(&[b"quic"])
                .expect("server alpn");
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
                .set_application_protos(&[b"quic"])
                .expect("client alpn");
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

        fn pump_until_established(
            handler: &mut QuicClientHandler,
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
                for pkt in &outbound {
                    progressed = true;
                    let payload_len = pkt.payload_len();
                    let mut buf = pkt.payload().to_vec();
                    if server_conn.is_none() {
                        let hdr = quiche::Header::from_slice(&mut buf, quiche::MAX_CONN_ID_LEN)
                            .expect("parse initial header");
                        let server_scid = vec![0xef; quiche::MAX_CONN_ID_LEN];
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

        /// See `worker.rs`'s `direct_call_h3::pump_until_reapable` doc
        /// comment for why this needs real wall-clock time.
        fn pump_until_reapable(
            handler: &mut QuicClientHandler,
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
        fn new_direct_completes_handshake_open_stream_and_close() {
            let _guard = setup_metrics();
            let (mut server_config, mut client_config) = build_test_configs();
            let (client_addr, server_addr) = test_addrs();
            let scid = vec![0x33_u8; TEST_SCID_LEN];
            let outbound_admission = Arc::new(OutboundAdmission::default());

            let mut handler = QuicClientHandler::new_direct(
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
            assert!(!handler.is_reapable());
            assert!(handler.next_timer_deadline().is_some());

            let stream_id = handler
                .open_bidi_stream()
                .expect("open_bidi_stream should succeed after handshake");
            assert_eq!(stream_id, 0, "first client-initiated bidi stream is 0");

            batch.clear();
            let released = handler.queue_stream_send(
                stream_id,
                Chunk::unpooled(b"hello".to_vec()),
                true,
                &mut batch,
                0,
            );
            assert!(released > 0, "expected admitted bytes back for the write");

            // Drive the stream data to the server.
            pump_until_established(
                &mut handler,
                &mut server_conn,
                &mut server_config,
                client_addr,
                server_addr,
                &mut batch,
            );
            if let Some(server) = server_conn.as_mut() {
                let mut recv_buf = vec![0_u8; 1024];
                let (len, fin) = server
                    .stream_recv(stream_id, &mut recv_buf)
                    .expect("server should have received the client's stream data");
                assert_eq!(&recv_buf[..len], b"hello");
                assert!(fin, "expected the FIN to be delivered with the data");
            }

            handler.close_session(0, "test done");
            batch.clear();
            pump_until_reapable(
                &mut handler,
                &mut server_conn,
                client_addr,
                server_addr,
                &mut batch,
            );
            let event_types: Vec<u8> = batch.iter().map(|e| e.event_type).collect();
            assert!(
                event_types.contains(&EVENT_SESSION_CLOSE),
                "expected a session-close event after close_session, got {event_types:?}"
            );
            assert!(handler.is_reapable());
        }

        #[test]
        fn new_direct_uses_the_caller_supplied_scid_bytes() {
            let (_server_config_a, mut client_config_a) = build_test_configs();
            let (_server_config_b, mut client_config_b) = build_test_configs();
            let (client_addr, server_addr) = test_addrs();

            let handler_a = QuicClientHandler::new_direct(
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
            let handler_b = QuicClientHandler::new_direct(
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
            client_config.log_keys();
            let (client_addr, server_addr) = test_addrs();
            let scid = vec![0x77_u8; TEST_SCID_LEN];
            let outbound_admission = Arc::new(OutboundAdmission::default());

            let mut handler = QuicClientHandler::new_direct(
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
            assert!(handler.take_keylog_lines().is_empty());
        }
    }

    /// Lockstep tests for the raw-QUIC server-side direct-call surface
    /// (`QuicServerHandler::new_direct` + `process_inbound_packet` +
    /// friends), added alongside server-side wasm ABI support. Mirrors
    /// `direct_call_quic` above's style and `worker.rs`'s
    /// `direct_call_h3_server` sibling module exactly — two real
    /// direct-call handlers pumped against each other, no hand-rolled
    /// `quiche::Connection` or real UDP socket needed.
    mod direct_call_quic_server {
        use super::*;
        use crate::config::{
            JsQuicClientOptions, JsQuicServerOptions, new_quic_client_config_in_memory,
            new_quic_server_config_in_memory,
        };
        use crate::h3_event::{
            EVENT_DATA, EVENT_HANDSHAKE_COMPLETE, EVENT_NEW_SESSION, EVENT_NEW_STREAM,
            EVENT_SESSION_CLOSE,
        };
        use std::net::{IpAddr, Ipv4Addr};

        const TEST_SCID_LEN: usize = crate::cid::SCID_LEN;

        fn test_addrs() -> (SocketAddr, SocketAddr) {
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44_001), // client
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54_001), // server
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

        fn build_server_direct() -> QuicServerHandler {
            let (cert_pem, key_pem) = generate_self_signed_pem();
            let options = JsQuicServerOptions {
                key: key_pem.into(),
                cert: cert_pem.into(),
                ca: None,
                client_auth: None,
                alpn: None,
                runtime_mode: None,
                max_idle_timeout_ms: Some(5_000),
                max_udp_payload_size: Some(1_350),
                initial_max_data: Some(1_000_000),
                initial_max_stream_data_bidi_local: Some(1_000_000),
                initial_max_streams_bidi: Some(100),
                disable_active_migration: Some(true),
                enable_datagrams: None,
                max_connections: Some(128),
                disable_retry: Some(true),
                qlog_dir: None,
                qlog_level: None,
                session_ticket_keys: None,
                keylog: None,
            };
            let quiche_config =
                new_quic_server_config_in_memory(&options).expect("server config");
            let server_config = QuicServerConfig {
                qlog_dir: None,
                qlog_level: None,
                max_connections: options.max_connections.unwrap_or(10_000) as usize,
                disable_retry: options.disable_retry.unwrap_or(false),
                client_auth: ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some())
                    .expect("valid client auth"),
                cid_encoding: CidEncoding::random(),
                runtime_mode: TransportRuntimeMode::Portable,
            };
            QuicServerHandler::new_direct(
                quiche_config,
                server_config,
                [0x66u8; 32],
                Arc::new(OutboundAdmission::default()),
            )
        }

        fn build_client_direct(scid_byte: u8, client_addr: SocketAddr, server_addr: SocketAddr) -> QuicClientHandler {
            let client_options = JsQuicClientOptions {
                ca: None,
                cert: None,
                key: None,
                reject_unauthorized: Some(false),
                alpn: None,
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
                new_quic_client_config_in_memory(&client_options).expect("client config");
            QuicClientHandler::new_direct(
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

        #[allow(clippy::too_many_arguments)]
        fn pump(
            client: &mut QuicClientHandler,
            server: &mut QuicServerHandler,
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
            client.poll_drain_events_for_handle(usize::MAX, client_batch, 0);
            client.flush_pending_writes_for_handle(client_batch, 0);

            progressed
        }

        fn pump_until<F>(
            client: &mut QuicClientHandler,
            server: &mut QuicServerHandler,
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
        fn new_direct_completes_handshake_stream_echo_and_close() {
            let (client_addr, server_addr) = test_addrs();
            let mut server = build_server_direct();
            let mut client = build_client_direct(0x71, client_addr, server_addr);

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

            assert!(server_batch.iter().any(|e| e.event_type == EVENT_NEW_SESSION));
            let conn_handle = server_batch
                .iter()
                .find(|e| e.event_type == EVENT_NEW_SESSION)
                .expect("new session event")
                .conn_handle;
            assert_eq!(server.connection_count(), 1);

            // --- Client opens a bidi stream and sends data; server echoes it ---
            let stream_id = client.open_bidi_stream().expect("open_bidi_stream");
            let released = client.queue_stream_send(
                stream_id,
                Chunk::unpooled(b"ping".to_vec()),
                true,
                &mut client_batch,
                0,
            );
            assert!(released > 0, "client stream send should be admitted");

            // Raw QUIC coalesces a new stream's first recv into the
            // `EVENT_NEW_STREAM` event itself (`data` carried right on
            // it — see `QuicConnection::poll_quic_events`'s "Coalesce
            // first recv into NEW_STREAM event" comment), so — exactly
            // like the H3 lockstep test's HEADERS+coalesced-DATA
            // handling — check `.data` on every event for this stream,
            // not just `EVENT_DATA`-typed ones.
            server_batch.clear();
            pump_until(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
                |_client_batch, server_batch| {
                    server_batch.iter().any(|e| e.stream_id as u64 == stream_id && e.data.is_some())
                },
            );

            assert!(server_batch.iter().any(|e| e.event_type == EVENT_NEW_STREAM));
            let received: Vec<u8> = server_batch
                .iter()
                .filter(|e| e.stream_id as u64 == stream_id)
                .filter_map(|e| e.data.as_deref())
                .flatten()
                .copied()
                .collect();
            assert_eq!(received, b"ping");

            let echoed = server.queue_stream_send(
                conn_handle,
                stream_id,
                Chunk::unpooled(b"pong".to_vec()),
                true,
                &mut server_batch,
            );
            assert!(echoed > 0, "server echo should be admitted");

            client_batch.clear();
            pump_until(
                &mut client,
                &mut server,
                client_addr,
                server_addr,
                &mut client_batch,
                &mut server_batch,
                |client_batch, _server_batch| {
                    client_batch
                        .iter()
                        .any(|e| e.event_type == EVENT_DATA && e.stream_id as u64 == stream_id)
                },
            );
            let echoed_body: Vec<u8> = client_batch
                .iter()
                .filter(|e| e.event_type == EVENT_DATA && e.stream_id as u64 == stream_id)
                .filter_map(|e| e.data.as_deref())
                .flatten()
                .copied()
                .collect();
            assert_eq!(echoed_body, b"pong");

            // --- Close ---
            server.close_connection(conn_handle, 0, "server done");
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
            assert!(server.connection_is_closed(conn_handle));

            server.reap_closed_connections(&mut server_batch);
            assert!(server_batch.iter().any(|e| e.event_type == EVENT_SESSION_CLOSE));
            assert!(server.is_idle());
            assert_eq!(server.connection_count(), 0);
        }
    }
}
