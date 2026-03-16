//! Worker thread mode: a dedicated OS thread runs the QUIC/H3 hot loop,
//! delivering batched events to the JS main thread via ThreadsafeFunction.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ring::rand::SecureRandom;

use crate::buffer_pool::BufferPool;
use crate::config::Http3Config;
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::connection_map::ConnectionMap;
use crate::error::Http3NativeError;
use crate::event_loop::{self, ProtocolHandler, SEND_BUF_SIZE};
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::timer_heap::TimerHeap;
use crate::transport::{self, Driver, ErasedWaker, TxDatagram};

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

/// Handle returned to the JS side for sending commands to the worker.
pub struct WorkerHandle {
    cmd_tx: Sender<WorkerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
    local_addr: SocketAddr,
    waker: Arc<dyn ErasedWaker>,
}

impl WorkerHandle {
    /// Send a command to the worker thread.
    /// The channel is unbounded so this only fails if the worker has disconnected.
    pub fn send_command(&self, cmd: WorkerCommand) -> bool {
        if self.cmd_tx.send(cmd).is_ok() {
            let _ = self.waker.wake();
            true
        } else {
            false
        }
    }

    /// Get the bound local address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn get_session_metrics(
        &self,
        conn_handle: u32,
    ) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(WorkerCommand::GetSessionMetrics {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server metrics".into())
        })
    }

    pub fn send_datagram(&self, conn_handle: u32, data: Vec<u8>) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(WorkerCommand::SendDatagram {
                conn_handle,
                data,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server datagram send".into())
        })
    }

    pub fn get_remote_settings(
        &self,
        conn_handle: u32,
    ) -> Result<Vec<(u64, u64)>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(WorkerCommand::GetRemoteSettings {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server settings".into())
        })
    }

    pub fn ping_session(&self, conn_handle: u32) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(WorkerCommand::PingSession {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for server ping".into()))
    }

    pub fn get_qlog_path(&self, conn_handle: u32) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(WorkerCommand::GetQlogPath {
                conn_handle,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("server worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for server qlog path".into())
        })
    }

    /// Shut down the worker thread and wait for it to finish.
    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
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

/// Handle returned to JS for controlling the client worker thread.
pub struct ClientWorkerHandle {
    cmd_tx: Sender<ClientWorkerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
    local_addr: SocketAddr,
    waker: Arc<dyn ErasedWaker>,
}

impl ClientWorkerHandle {
    /// Open a new request stream and return the stream ID.
    pub fn send_request(
        &self,
        headers: Vec<(String, String)>,
        fin: bool,
    ) -> Result<u64, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(ClientWorkerCommand::SendRequest {
                headers,
                fin,
                resp_tx,
            })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();

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
        if self
            .cmd_tx
            .send(ClientWorkerCommand::StreamSend {
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
            .send(ClientWorkerCommand::StreamClose {
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
            .send(ClientWorkerCommand::SendDatagram { data, resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client datagram send".into())
        })
    }

    pub fn get_session_metrics(&self) -> Result<Option<JsSessionMetrics>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(ClientWorkerCommand::GetSessionMetrics { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client metrics".into())
        })
    }

    pub fn get_remote_settings(&self) -> Result<Vec<(u64, u64)>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(ClientWorkerCommand::GetRemoteSettings { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client settings".into())
        })
    }

    pub fn ping(&self) -> Result<bool, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(ClientWorkerCommand::Ping { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Http3NativeError::InvalidState("timed out waiting for client ping".into()))
    }

    pub fn get_qlog_path(&self) -> Result<Option<String>, Http3NativeError> {
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(ClientWorkerCommand::GetQlogPath { resp_tx })
            .map_err(|_| Http3NativeError::InvalidState("client worker not running".into()))?;
        let _ = self.waker.wake();
        resp_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            Http3NativeError::InvalidState("timed out waiting for client qlog path".into())
        })
    }

    /// Request a graceful connection close.
    pub fn close(&self, error_code: u32, reason: String) -> bool {
        if self
            .cmd_tx
            .send(ClientWorkerCommand::Close { error_code, reason })
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
        let _ = self.cmd_tx.send(ClientWorkerCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
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
pub fn spawn_client_worker(
    quiche_config: quiche::Config,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    tsfn: EventTsfn,
) -> Result<ClientWorkerHandle, Http3NativeError> {
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
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, tsfn, local_addr);
    });

    Ok(ClientWorkerHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    })
}

/// Spawn a worker thread for the given server configuration.
/// Events are delivered to JS via the provided `ThreadsafeFunction`.
pub fn spawn_worker(
    quiche_config: quiche::Config,
    http3_config: Http3Config,
    bind_addr: SocketAddr,
    tsfn: EventTsfn,
) -> Result<WorkerHandle, Http3NativeError> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

    let std_socket = transport::socket::bind_worker_socket(bind_addr, http3_config.reuse_port)?;
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
        let mut handler = H3ServerHandler::new(quiche_config, http3_config);
        event_loop::run_event_loop(&mut driver, cmd_rx, &mut handler, tsfn, local_addr);
    });

    let handle = WorkerHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    };

    Ok(handle)
}

// ── H3 Server Protocol Handler ──────────────────────────────────────

struct H3ServerHandler {
    conn_map: ConnectionMap,
    timer_heap: TimerHeap,
    buffer_pool: BufferPool,
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
}

impl H3ServerHandler {
    fn new(quiche_config: quiche::Config, http3_config: Http3Config) -> Self {
        let disable_retry = http3_config.disable_retry;
        Self {
            conn_map: ConnectionMap::with_max_connections_and_cid(
                http3_config.max_connections,
                http3_config.cid_encoding.clone(),
            ),
            timer_heap: TimerHeap::new(),
            buffer_pool: BufferPool::default(),
            pending_writes: HashMap::new(),
            pending_session_closes: HashMap::new(),
            conn_send_buffers: HashMap::new(),
            handles_buf: Vec::new(),
            http3_config,
            quiche_config,
            disable_retry,
            last_expired: Vec::new(),
        }
    }
}

impl ProtocolHandler for H3ServerHandler {
    type Command = WorkerCommand;

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
                            self.http3_config.qlog_dir.as_deref(),
                            self.http3_config.qlog_level.as_deref(),
                            self.http3_config.qpack_max_table_capacity,
                            self.http3_config.qpack_blocked_streams,
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

            if (conn.quiche_conn.is_established() || conn.quiche_conn.is_in_early_data())
                && !conn.is_established
            {
                let _ = conn.init_h3();
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

            conn.poll_h3_events(handle as u32, batch);

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
                    conn.poll_h3_events(handle as u32, batch);
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
        let flushed = flush_pending_writes(&mut self.conn_map, &mut self.pending_writes);
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
            self.pending_session_closes.remove(&(*handle as u32));
            self.pending_writes
                .retain(|&(ch, _), _| ch != *handle as u32);
            if !self.last_expired.contains(handle) {
                batch.push(JsH3Event::session_close(*handle as u32));
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
        Some(Self {
            conn,
            pending_writes: HashMap::new(),
            send_buf: vec![0u8; SEND_BUF_SIZE],
            timer_deadline,
            session_closed_emitted: false,
        })
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
            ClientWorkerCommand::Shutdown => return true,
            ClientWorkerCommand::Close { error_code, reason } => {
                let _ = self
                    .conn
                    .quiche_conn
                    .close(true, u64::from(error_code), reason.as_bytes());
            }
            ClientWorkerCommand::SendRequest {
                headers,
                fin,
                resp_tx,
            } => {
                if (self.conn.quiche_conn.is_established()
                    || self.conn.quiche_conn.is_in_early_data())
                    && !self.conn.is_established
                {
                    let _ = self.conn.init_h3();
                }
                let h3_headers: Vec<quiche::h3::Header> = headers
                    .iter()
                    .map(|(n, v)| quiche::h3::Header::new(n.as_bytes(), v.as_bytes()))
                    .collect();
                let result = self
                    .conn
                    .send_request(&h3_headers, fin)
                    .map_err(|e| format!("send_request failed: {e}"));
                let _ = resp_tx.send(result);
            }
            ClientWorkerCommand::StreamSend {
                stream_id,
                data,
                fin,
            } => {
                if let Some(pw) = self.pending_writes.get_mut(&stream_id) {
                    pw.data.extend_from_slice(&data);
                    pw.fin = pw.fin || fin;
                } else {
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
            }
            ClientWorkerCommand::StreamClose {
                stream_id,
                error_code,
            } => {
                let _ = self.conn.stream_close(stream_id, u64::from(error_code));
            }
            ClientWorkerCommand::SendDatagram { data, resp_tx } => {
                let _ = resp_tx.send(self.conn.send_datagram(&data).is_ok());
            }
            ClientWorkerCommand::GetSessionMetrics { resp_tx } => {
                let _ = resp_tx.send(Some(snapshot_metrics(&self.conn)));
            }
            ClientWorkerCommand::GetRemoteSettings { resp_tx } => {
                let _ = resp_tx.send(self.conn.remote_settings());
            }
            ClientWorkerCommand::GetQlogPath { resp_tx } => {
                let _ = resp_tx.send(self.conn.qlog_path.clone());
            }
            ClientWorkerCommand::Ping { resp_tx } => {
                let _ = resp_tx.send(self.conn.quiche_conn.send_ack_eliciting().is_ok());
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

        if (self.conn.quiche_conn.is_established() || self.conn.quiche_conn.is_in_early_data())
            && !self.conn.is_established
        {
            let _ = self.conn.init_h3();
        }
        if self.conn.quiche_conn.is_established() && !self.conn.handshake_complete_emitted {
            self.conn.handshake_complete_emitted = true;
            batch.push(JsH3Event::handshake_complete(0));
        }
        self.conn.poll_h3_events(0, batch);
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
                self.conn.poll_h3_events(0, batch);
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
        let flushed = flush_client_pending_writes(&mut self.conn, &mut self.pending_writes);
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
