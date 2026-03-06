//! Worker thread mode: a dedicated OS thread runs the QUIC/H3 hot loop,
//! delivering batched events to the JS main thread via ThreadsafeFunction.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use mio::net::UdpSocket as MioUdpSocket;
use mio::{Events, Interest, Poll, Token, Waker};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use ring::rand::SecureRandom;

use crate::buffer_pool::BufferPool;
use crate::config::Http3Config;
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::connection_map::ConnectionMap;
use crate::error::Http3NativeError;
use crate::h3_event::{JsH3Event, JsSessionMetrics};
use crate::timer_heap::TimerHeap;

const SOCKET_TOKEN: Token = Token(0);
const WAKER_TOKEN: Token = Token(1);
/// Max events per TSFN call. Sized for high-concurrency workloads (1000+ streams)
/// while keeping JS callbacks short enough to avoid blocking the event loop.
const MAX_BATCH_SIZE: usize = 512;
/// Per-connection QUIC packet scratch buffer size.
const SEND_BUF_SIZE: usize = 65535;

/// TSFN type for delivering event batches to the JS main thread.
/// Uses default const generics: CalleeHandled=true, Weak=false, MaxQueueSize=0 (unbounded).
pub type EventTsfn = ThreadsafeFunction<Vec<JsH3Event>>;

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
    /// Waker to interrupt the worker's mio::Poll when commands are enqueued.
    waker: Arc<Waker>,
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
    waker: Arc<Waker>,
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

/// Spawn a client worker thread that owns UDP I/O and QUIC/H3 processing.
pub fn spawn_client_worker(
    mut quiche_config: quiche::Config,
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
    let _ = set_socket_buffers(&std_socket, 2 * 1024 * 1024);
    let local_addr = std_socket.local_addr().map_err(Http3NativeError::Io)?;

    let poll = Poll::new().map_err(Http3NativeError::Io)?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).map_err(Http3NativeError::Io)?);
    let waker_clone = waker.clone();

    let join_handle = thread::spawn(move || {
        client_worker_loop(
            std_socket,
            local_addr,
            server_addr,
            server_name,
            session_ticket,
            qlog_dir,
            qlog_level,
            &mut quiche_config,
            poll,
            cmd_rx,
            tsfn,
        );
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
    mut quiche_config: quiche::Config,
    http3_config: Http3Config,
    bind_addr: SocketAddr,
    tsfn: EventTsfn,
) -> Result<WorkerHandle, Http3NativeError> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();

    let std_socket = bind_worker_socket(bind_addr, http3_config.reuse_port)?;
    std_socket
        .set_nonblocking(true)
        .map_err(Http3NativeError::Io)?;
    // Increase OS socket buffers for high-throughput burst scenarios
    let _ = set_socket_buffers(&std_socket, 2 * 1024 * 1024);
    let local_addr = std_socket.local_addr().map_err(Http3NativeError::Io)?;

    // Create poll + waker on the main thread so we can wake the worker
    // when JS sends commands via the crossbeam channel.
    let poll = Poll::new().map_err(Http3NativeError::Io)?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).map_err(Http3NativeError::Io)?);
    let waker_clone = waker.clone();

    let join_handle = thread::spawn(move || {
        worker_loop(
            std_socket,
            local_addr,
            &mut quiche_config,
            &http3_config,
            poll,
            cmd_rx,
            tsfn,
        );
    });

    let handle = WorkerHandle {
        cmd_tx,
        join_handle: Some(join_handle),
        local_addr,
        waker: waker_clone,
    };

    Ok(handle)
}

/// The worker thread's main loop.
#[allow(clippy::too_many_lines)]
fn worker_loop(
    std_socket: UdpSocket,
    local_addr: SocketAddr,
    quiche_config: &mut quiche::Config,
    http3_config: &Http3Config,
    mut poll: Poll,
    cmd_rx: Receiver<WorkerCommand>,
    tsfn: EventTsfn,
) {
    let disable_retry = http3_config.disable_retry;

    // Convert std UdpSocket to mio UdpSocket for non-blocking poll
    let mut mio_socket = MioUdpSocket::from_std(std_socket);

    if poll
        .registry()
        .register(&mut mio_socket, SOCKET_TOKEN, Interest::READABLE)
        .is_err()
    {
        return;
    }

    let mut conn_map = ConnectionMap::with_max_connections_and_cid(
        http3_config.max_connections,
        http3_config.cid_encoding.clone(),
    );
    let mut timer_heap = TimerHeap::new();
    let mut buffer_pool = BufferPool::default();
    let mut events = Events::with_capacity(256);
    let mut recv_buf = vec![0u8; 65535];
    let mut pending_outbound: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
    let mut events_dropped: u64 = 0;
    // Reusable handles buffer — avoids allocation on every iteration
    let mut handles_buf: Vec<usize> = Vec::new();
    // Buffered partial writes: when send_body can't write all data due to
    // flow control, the remainder is stored here and retried when the stream
    // becomes writable. Key is (conn_handle, stream_id).
    let mut pending_writes: HashMap<(u32, u64), PendingWrite> = HashMap::new();
    // Graceful close requests that should send GOAWAY first, then CONNECTION_CLOSE.
    let mut pending_session_closes: HashMap<u32, (u32, String, Instant)> = HashMap::new();
    // Dedicated send scratch buffer per connection handle.
    let mut conn_send_buffers: HashMap<usize, Vec<u8>> = HashMap::new();
    // Packets that couldn't be sent because the OS UDP buffer was full.
    // Retried when the socket becomes writable again.
    let mut unsent_packets: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
    let mut socket_writable = true;

    // Try to send a UDP packet. If the OS buffer is full, queue it for retry.
    let try_send = |socket: &MioUdpSocket,
                    data: &[u8],
                    to: SocketAddr,
                    unsent: &mut Vec<(Vec<u8>, SocketAddr)>,
                    writable: &mut bool| {
        if !*writable {
            unsent.push((data.to_vec(), to));
            return;
        }
        match socket.send_to(data, to) {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                unsent.push((data.to_vec(), to));
                *writable = false;
            }
            Ok(_) | Err(_) => {}
        }
    };

    // Drain packets that were buffered due to WouldBlock.
    let drain_unsent =
        |socket: &MioUdpSocket, unsent: &mut Vec<(Vec<u8>, SocketAddr)>, writable: &mut bool| {
            while let Some((data, to)) = unsent.first() {
                match socket.send_to(data, *to) {
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        *writable = false;
                        return;
                    }
                    Ok(_) | Err(_) => {
                        unsent.remove(0);
                    }
                }
            }
            *writable = true;
        };

    // Flush the batch to JS when it reaches MAX_BATCH_SIZE.
    // Returns true if the TSFN is still alive; false if it's been finalized
    // (JS is shutting down) and the worker should exit.
    let flush_batch = |batch: &mut Vec<JsH3Event>, tsfn: &EventTsfn, dropped: &mut u64| -> bool {
        if batch.is_empty() {
            return true;
        }
        let count = batch.len();
        let to_send = std::mem::take(batch);
        match tsfn.call(Ok(to_send), ThreadsafeFunctionCallMode::NonBlocking) {
            napi::Status::Ok => true,
            napi::Status::Closing => {
                // JS thread finalized the TSFN — stop the worker loop.
                *dropped += count as u64;
                log::debug!("TSFN closing, dropped {count} events");
                false
            }
            status => {
                // Queue is unbounded (MaxQueueSize=0) so QueueFull shouldn't
                // happen, but log it defensively.
                *dropped += count as u64;
                log::warn!(
                    "TSFN call failed ({status:?}), dropped {count} events (total dropped: {})",
                    *dropped
                );
                true
            }
        }
    };

    loop {
        // Calculate next timeout from timer heap
        let timeout = timer_heap
            .next_deadline()
            .map_or(Duration::from_millis(100), |d| {
                let now = Instant::now();
                if d <= now {
                    Duration::ZERO
                } else {
                    d.duration_since(now)
                }
            }); // Default 100ms poll

        if poll.poll(&mut events, Some(timeout)).is_err() {
            break;
        }

        // 0. Handle socket writability — drain buffered packets
        for event in &events {
            if event.token() == SOCKET_TOKEN && event.is_writable() {
                socket_writable = true;
                drain_unsent(&mio_socket, &mut unsent_packets, &mut socket_writable);
            }
        }

        // Re-register socket interest based on whether we have unsent packets
        let interest = if unsent_packets.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        let _ = poll
            .registry()
            .reregister(&mut mio_socket, SOCKET_TOKEN, interest);

        // 1. Drain command channel
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::Shutdown => return,
                WorkerCommand::SendResponseHeaders {
                    conn_handle,
                    stream_id,
                    headers,
                    fin,
                } => {
                    if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
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
                    if let Some(pw) = pending_writes.get_mut(&key) {
                        // Already have buffered data — append to avoid reordering
                        pw.data.extend_from_slice(&data);
                        pw.fin = pw.fin || fin;
                    } else if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
                        let written = conn.send_body(stream_id, &data, fin).unwrap_or(0);
                        if written < data.len() {
                            pending_writes.insert(
                                key,
                                PendingWrite {
                                    data: data[written..].to_vec(),
                                    fin,
                                },
                            );
                        } else if fin && written == 0 && data.is_empty() {
                            // FIN-only send blocked by flow control
                            pending_writes.insert(
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
                    if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
                        let _ = conn.stream_close(stream_id, u64::from(error_code));
                    }
                }
                WorkerCommand::SendTrailers {
                    conn_handle,
                    stream_id,
                    headers,
                } => {
                    if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
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
                    if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
                        if conn.send_goaway().is_ok() {
                            pending_session_closes.insert(
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
                    let ok = conn_map
                        .get_mut(conn_handle as usize)
                        .is_some_and(|conn| conn.send_datagram(&data).is_ok());
                    let _ = resp_tx.send(ok);
                }
                WorkerCommand::GetSessionMetrics {
                    conn_handle,
                    resp_tx,
                } => {
                    let metrics = conn_map.get(conn_handle as usize).map(snapshot_metrics);
                    let _ = resp_tx.send(metrics);
                }
                WorkerCommand::GetRemoteSettings {
                    conn_handle,
                    resp_tx,
                } => {
                    let settings = conn_map
                        .get(conn_handle as usize)
                        .map_or_else(Vec::new, H3Connection::remote_settings);
                    let _ = resp_tx.send(settings);
                }
                WorkerCommand::GetQlogPath {
                    conn_handle,
                    resp_tx,
                } => {
                    let path = conn_map
                        .get(conn_handle as usize)
                        .and_then(|conn| conn.qlog_path.clone());
                    let _ = resp_tx.send(path);
                }
                WorkerCommand::PingSession {
                    conn_handle,
                    resp_tx,
                } => {
                    let ok = if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
                        conn.quiche_conn.send_ack_eliciting().is_ok()
                    } else {
                        false
                    };
                    let _ = resp_tx.send(ok);
                }
            }
        }

        // 1b. Flush sends after commands (response data needs to go out immediately)
        {
            conn_map.fill_handles(&mut handles_buf);
            for handle in &handles_buf {
                let send_buf = conn_send_buffers
                    .entry(*handle)
                    .or_insert_with(|| vec![0u8; SEND_BUF_SIZE]);
                if let Some(conn) = conn_map.get_mut(*handle) {
                    while let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
                        try_send(
                            &mio_socket,
                            &send_buf[..len],
                            send_info.to,
                            &mut unsent_packets,
                            &mut socket_writable,
                        );
                    }
                }
            }
        }

        // 2. Read incoming packets — flush batch when it hits the cap
        let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);
        loop {
            match mio_socket.recv_from(&mut recv_buf) {
                Ok((len, peer)) => {
                    process_packet(
                        &mut recv_buf[..len],
                        peer,
                        local_addr,
                        &mut conn_map,
                        &mut timer_heap,
                        http3_config,
                        quiche_config,
                        &mut pending_outbound,
                        disable_retry,
                        &mut batch,
                        &mut buffer_pool,
                    );
                    if batch.len() >= MAX_BATCH_SIZE
                        && !flush_batch(&mut batch, &tsfn, &mut events_dropped)
                    {
                        return;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // 3. Process expired timers
        let now = Instant::now();
        let expired = timer_heap.pop_expired(now);
        for &handle in &expired {
            if let Some(conn) = conn_map.get_mut(handle) {
                conn.on_timeout();
                if conn.is_closed() {
                    batch.push(JsH3Event::session_close(handle as u32));
                } else {
                    conn.poll_h3_events(handle as u32, &mut batch);
                    if let Some(timeout) = conn.timeout() {
                        timer_heap.schedule(handle, Instant::now() + timeout);
                    }
                }
            }
        }

        // Check all connections for drain events
        conn_map.fill_handles(&mut handles_buf);
        for handle in &handles_buf {
            if expired.contains(handle) {
                continue;
            }
            if let Some(conn) = conn_map.get_mut(*handle) {
                if !conn.blocked_streams.is_empty() {
                    conn.poll_drain_events(*handle as u32, &mut batch);
                }
            }
        }

        // 3b. Flush pending writes — flow control windows may have opened
        // after receiving ACKs or processing timeouts.
        let flushed = flush_pending_writes(&mut conn_map, &mut pending_writes);
        for (conn_handle, stream_id) in flushed {
            batch.push(JsH3Event::drain(conn_handle, stream_id));
        }

        // 3c. Complete deferred graceful session closes after GOAWAY had
        // a brief opportunity to flush on the wire.
        let now = Instant::now();
        let due_closes: Vec<u32> = pending_session_closes
            .iter()
            .filter_map(|(handle, (_, _, deadline))| (now >= *deadline).then_some(*handle))
            .collect();
        for conn_handle in due_closes {
            if let Some((error_code, reason, _)) = pending_session_closes.remove(&conn_handle) {
                if let Some(conn) = conn_map.get_mut(conn_handle as usize) {
                    let _ = conn
                        .quiche_conn
                        .close(true, u64::from(error_code), reason.as_bytes());
                }
            }
        }

        // Flush if timer/drain processing pushed us over the cap
        if batch.len() >= MAX_BATCH_SIZE && !flush_batch(&mut batch, &tsfn, &mut events_dropped) {
            return;
        }

        // 4. Flush outbound packets
        for handle in &handles_buf {
            let send_buf = conn_send_buffers
                .entry(*handle)
                .or_insert_with(|| vec![0u8; SEND_BUF_SIZE]);
            if let Some(conn) = conn_map.get_mut(*handle) {
                while let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
                    try_send(
                        &mio_socket,
                        &send_buf[..len],
                        send_info.to,
                        &mut unsent_packets,
                        &mut socket_writable,
                    );
                }
            }
        }

        // Send pending outbound (e.g., Retry packets)
        for (data, addr) in pending_outbound.drain(..) {
            try_send(
                &mio_socket,
                &data,
                addr,
                &mut unsent_packets,
                &mut socket_writable,
            );
        }

        // Register for WRITABLE if we have buffered packets
        if !unsent_packets.is_empty() {
            let _ = poll.registry().reregister(
                &mut mio_socket,
                SOCKET_TOKEN,
                Interest::READABLE | Interest::WRITABLE,
            );
        }

        // Clean up closed connections
        let closed = conn_map.drain_closed();
        for handle in &closed {
            timer_heap.remove_connection(*handle);
            conn_send_buffers.remove(handle);
            pending_session_closes.remove(&(*handle as u32));
            // Clear any buffered pending writes so stale handles aren't reused
            // after the slab slot is recycled for a new connection.
            pending_writes.retain(|&(ch, _), _| ch != *handle as u32);
            if !expired.contains(handle) {
                batch.push(JsH3Event::session_close(*handle as u32));
            }
        }

        // 5. Final flush of remaining events
        if !flush_batch(&mut batch, &tsfn, &mut events_dropped) {
            return;
        }
    }
}

/// Process a single incoming packet through the server's quiche stack.
/// Events are pushed directly into the provided batch.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn process_packet(
    buf: &mut [u8],
    peer: SocketAddr,
    local: SocketAddr,
    conn_map: &mut ConnectionMap,
    timer_heap: &mut TimerHeap,
    http3_config: &Http3Config,
    quiche_config: &mut quiche::Config,
    pending_outbound: &mut Vec<(Vec<u8>, SocketAddr)>,
    disable_retry: bool,
    batch: &mut Vec<JsH3Event>,
    buffer_pool: &mut BufferPool,
) {
    let Ok(hdr) = quiche::Header::from_slice(buf, crate::connection_map::SCID_LEN) else {
        return;
    };

    let handle = if let Some(handle) = conn_map.route_packet(hdr.dcid.as_ref()) {
        handle
    } else {
        if hdr.ty != quiche::Type::Initial {
            return;
        }

        if disable_retry {
            // Retry disabled — accept directly.
            // odcid must be None: setting it would add an
            // original_destination_connection_id transport parameter that
            // the client doesn't expect, stalling the TLS handshake.
            let Ok(scid) = conn_map.generate_scid() else {
                return;
            };
            let client_dcid = hdr.dcid.to_vec();
            match conn_map.accept_new(
                &scid,
                None,
                peer,
                local,
                quiche_config,
                http3_config.qlog_dir.as_deref(),
                http3_config.qlog_level.as_deref(),
                http3_config.qpack_max_table_capacity,
                http3_config.qpack_blocked_streams,
            ) {
                Ok(h) => {
                    conn_map.add_dcid(h, client_dcid);
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
            match conn_map.validate_token(token, &peer) {
                Some(odcid) => {
                    let scid = hdr.dcid.to_vec();
                    let odcid_ref = quiche::ConnectionId::from_ref(&odcid);
                    match conn_map.accept_new(
                        &scid,
                        Some(&odcid_ref),
                        peer,
                        local,
                        quiche_config,
                        http3_config.qlog_dir.as_deref(),
                        http3_config.qlog_level.as_deref(),
                        http3_config.qpack_max_table_capacity,
                        http3_config.qpack_blocked_streams,
                    ) {
                        Ok(h) => {
                            conn_map.add_dcid(h, odcid);
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
            // No token — send Retry using pooled buffer
            let Ok(scid) = conn_map.generate_scid() else {
                return;
            };
            let scid_ref = quiche::ConnectionId::from_ref(&scid);
            let token = conn_map.mint_token(&peer, hdr.dcid.as_ref());
            let mut out = buffer_pool.checkout();
            if let Ok(len) = quiche::retry(
                &hdr.scid,
                &hdr.dcid,
                &scid_ref,
                &token,
                hdr.version,
                &mut out,
            ) {
                pending_outbound.push((out[..len].to_vec(), peer));
            }
            buffer_pool.checkin(out);
            return;
        }
    };

    let recv_info = quiche::RecvInfo {
        from: peer,
        to: local,
    };

    let (timeout, current_scid, needs_dcid_update, retired_scids) = {
        let Some(conn) = conn_map.get_mut(handle) else {
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
        conn_map.add_dcid(handle, current_scid);
    }

    for retired_scid in retired_scids {
        conn_map.remove_dcid(&retired_scid);
    }

    top_up_server_scids(conn_map, handle);

    if let Some(timeout) = timeout {
        timer_heap.schedule(handle, Instant::now() + timeout);
    }
}

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
/// Called after receiving packets or processing timeouts, when flow control
/// windows may have opened.
/// Returns a list of (conn_handle, stream_id) pairs that were fully flushed,
/// so the caller can emit drain events for them.
fn flush_pending_writes(
    conn_map: &mut ConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
) -> Vec<(u32, u64)> {
    let mut flushed = Vec::new();
    pending.retain(|&(conn_handle, stream_id), pw| {
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            return false; // Connection gone, drop buffered data
        };
        let written = conn.send_body(stream_id, &pw.data, pw.fin).unwrap_or(0);
        if written >= pw.data.len() {
            // Fully flushed (data + fin if set)
            flushed.push((conn_handle, stream_id));
            false
        } else {
            // Partial or blocked — keep remainder
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

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn client_worker_loop(
    std_socket: UdpSocket,
    local_addr: SocketAddr,
    server_addr: SocketAddr,
    server_name: String,
    session_ticket: Option<Vec<u8>>,
    qlog_dir: Option<String>,
    qlog_level: Option<String>,
    quiche_config: &mut quiche::Config,
    mut poll: Poll,
    cmd_rx: Receiver<ClientWorkerCommand>,
    tsfn: EventTsfn,
) {
    let mut mio_socket = MioUdpSocket::from_std(std_socket);
    if poll
        .registry()
        .register(&mut mio_socket, SOCKET_TOKEN, Interest::READABLE)
        .is_err()
    {
        return;
    }

    let Ok(scid) = ConnectionMap::generate_random_scid() else {
        return;
    };
    let scid_ref = quiche::ConnectionId::from_ref(&scid);
    let Ok(mut quiche_conn) = quiche::connect(
        Some(&server_name),
        &scid_ref,
        local_addr,
        server_addr,
        quiche_config,
    ) else {
        return;
    };
    if let Some(ticket) = session_ticket.as_ref() {
        let _ = quiche_conn.set_session(ticket);
    }
    let mut conn = H3Connection::new(
        quiche_conn,
        scid,
        H3ConnectionInit {
            role: "client",
            qlog_dir: qlog_dir.as_deref(),
            qlog_level: qlog_level.as_deref(),
            qpack_max_table_capacity: None,
            qpack_blocked_streams: None,
        },
    );

    let mut events = Events::with_capacity(256);
    let mut recv_buf = vec![0u8; 65535];
    let mut send_buf = vec![0u8; SEND_BUF_SIZE];
    let mut pending_writes: HashMap<u64, PendingWrite> = HashMap::new();
    let mut unsent_packets: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
    let mut socket_writable = true;
    let mut session_closed_emitted = false;

    let try_send = |socket: &MioUdpSocket,
                    data: &[u8],
                    to: SocketAddr,
                    unsent: &mut Vec<(Vec<u8>, SocketAddr)>,
                    writable: &mut bool| {
        if !*writable {
            unsent.push((data.to_vec(), to));
            return;
        }
        match socket.send_to(data, to) {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                unsent.push((data.to_vec(), to));
                *writable = false;
            }
            Ok(_) | Err(_) => {}
        }
    };

    let drain_unsent =
        |socket: &MioUdpSocket, unsent: &mut Vec<(Vec<u8>, SocketAddr)>, writable: &mut bool| {
            while let Some((data, to)) = unsent.first() {
                match socket.send_to(data, *to) {
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        *writable = false;
                        return;
                    }
                    Ok(_) | Err(_) => {
                        unsent.remove(0);
                    }
                }
            }
            *writable = true;
        };

    let flush_batch = |batch: &mut Vec<JsH3Event>, tsfn: &EventTsfn, dropped: &mut u64| -> bool {
        if batch.is_empty() {
            return true;
        }
        let count = batch.len();
        let to_send = std::mem::take(batch);
        match tsfn.call(Ok(to_send), ThreadsafeFunctionCallMode::NonBlocking) {
            napi::Status::Ok => true,
            napi::Status::Closing => {
                *dropped += count as u64;
                false
            }
            _ => {
                *dropped += count as u64;
                true
            }
        }
    };

    let mut events_dropped: u64 = 0;
    let mut timer_deadline = conn.timeout().map(|t| Instant::now() + t);
    let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);

    while let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
        try_send(
            &mio_socket,
            &send_buf[..len],
            send_info.to,
            &mut unsent_packets,
            &mut socket_writable,
        );
    }

    loop {
        let timeout = timer_deadline.map_or(Duration::from_millis(100), |deadline| {
            let now = Instant::now();
            if deadline <= now {
                Duration::ZERO
            } else {
                deadline.duration_since(now)
            }
        });

        if poll.poll(&mut events, Some(timeout)).is_err() {
            break;
        }
        let now = Instant::now();

        for event in &events {
            if event.token() == SOCKET_TOKEN && event.is_writable() {
                socket_writable = true;
                drain_unsent(&mio_socket, &mut unsent_packets, &mut socket_writable);
            }
        }

        let interest = if unsent_packets.is_empty() {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        let _ = poll
            .registry()
            .reregister(&mut mio_socket, SOCKET_TOKEN, interest);

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ClientWorkerCommand::Shutdown => return,
                ClientWorkerCommand::Close { error_code, reason } => {
                    let _ = conn
                        .quiche_conn
                        .close(true, u64::from(error_code), reason.as_bytes());
                }
                ClientWorkerCommand::SendRequest {
                    headers,
                    fin,
                    resp_tx,
                } => {
                    if (conn.quiche_conn.is_established() || conn.quiche_conn.is_in_early_data())
                        && !conn.is_established
                    {
                        let _ = conn.init_h3();
                    }
                    let h3_headers: Vec<quiche::h3::Header> = headers
                        .iter()
                        .map(|(n, v)| quiche::h3::Header::new(n.as_bytes(), v.as_bytes()))
                        .collect();
                    let result = conn
                        .send_request(&h3_headers, fin)
                        .map_err(|e| format!("send_request failed: {e}"));
                    let _ = resp_tx.send(result);
                }
                ClientWorkerCommand::StreamSend {
                    stream_id,
                    data,
                    fin,
                } => {
                    if let Some(pw) = pending_writes.get_mut(&stream_id) {
                        pw.data.extend_from_slice(&data);
                        pw.fin = pw.fin || fin;
                    } else {
                        let written = conn.send_body(stream_id, &data, fin).unwrap_or(0);
                        if written < data.len() {
                            pending_writes.insert(
                                stream_id,
                                PendingWrite {
                                    data: data[written..].to_vec(),
                                    fin,
                                },
                            );
                        } else if fin && written == 0 && data.is_empty() {
                            pending_writes.insert(
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
                    let _ = conn.stream_close(stream_id, u64::from(error_code));
                }
                ClientWorkerCommand::SendDatagram { data, resp_tx } => {
                    let _ = resp_tx.send(conn.send_datagram(&data).is_ok());
                }
                ClientWorkerCommand::GetSessionMetrics { resp_tx } => {
                    let _ = resp_tx.send(Some(snapshot_metrics(&conn)));
                }
                ClientWorkerCommand::GetRemoteSettings { resp_tx } => {
                    let _ = resp_tx.send(conn.remote_settings());
                }
                ClientWorkerCommand::GetQlogPath { resp_tx } => {
                    let _ = resp_tx.send(conn.qlog_path.clone());
                }
                ClientWorkerCommand::Ping { resp_tx } => {
                    let _ = resp_tx.send(conn.quiche_conn.send_ack_eliciting().is_ok());
                }
            }
        }

        while let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
            try_send(
                &mio_socket,
                &send_buf[..len],
                send_info.to,
                &mut unsent_packets,
                &mut socket_writable,
            );
        }

        loop {
            match mio_socket.recv_from(&mut recv_buf) {
                Ok((len, peer)) => {
                    if !should_accept_client_peer(server_addr, peer) {
                        continue;
                    }
                    let recv_info = quiche::RecvInfo {
                        from: peer,
                        to: local_addr,
                    };
                    if conn.recv(&mut recv_buf[..len], recv_info).is_err() {
                        continue;
                    }

                    if (conn.quiche_conn.is_established() || conn.quiche_conn.is_in_early_data())
                        && !conn.is_established
                    {
                        let _ = conn.init_h3();
                    }
                    if conn.quiche_conn.is_established() && !conn.handshake_complete_emitted {
                        conn.handshake_complete_emitted = true;
                        batch.push(JsH3Event::handshake_complete(0));
                    }
                    conn.poll_h3_events(0, &mut batch);
                    if let Some(ticket) = conn.update_session_ticket() {
                        batch.push(JsH3Event::session_ticket(0, ticket));
                    }
                    timer_deadline = conn.timeout().map(|t| Instant::now() + t);

                    if conn.is_closed() && !session_closed_emitted {
                        batch.push(JsH3Event::session_close(0));
                        session_closed_emitted = true;
                    }

                    if batch.len() >= MAX_BATCH_SIZE
                        && !flush_batch(&mut batch, &tsfn, &mut events_dropped)
                    {
                        return;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        if timer_deadline.is_some_and(|d| d <= now) {
            conn.on_timeout();
            if conn.is_closed() && !session_closed_emitted {
                batch.push(JsH3Event::session_close(0));
                session_closed_emitted = true;
            } else {
                conn.poll_h3_events(0, &mut batch);
                if let Some(ticket) = conn.update_session_ticket() {
                    batch.push(JsH3Event::session_ticket(0, ticket));
                }
                timer_deadline = conn.timeout().map(|t| Instant::now() + t);
            }
        }

        if !conn.blocked_streams.is_empty() {
            conn.poll_drain_events(0, &mut batch);
        }

        let flushed = flush_client_pending_writes(&mut conn, &mut pending_writes);
        for stream_id in flushed {
            batch.push(JsH3Event::drain(0, stream_id));
        }

        while let Ok((len, send_info)) = conn.send(send_buf.as_mut_slice()) {
            try_send(
                &mio_socket,
                &send_buf[..len],
                send_info.to,
                &mut unsent_packets,
                &mut socket_writable,
            );
        }

        if !unsent_packets.is_empty() {
            let _ = poll.registry().reregister(
                &mut mio_socket,
                SOCKET_TOKEN,
                Interest::READABLE | Interest::WRITABLE,
            );
        }

        if !flush_batch(&mut batch, &tsfn, &mut events_dropped) {
            return;
        }

        if session_closed_emitted && unsent_packets.is_empty() {
            return;
        }
    }
}

fn set_socket_buffers(socket: &UdpSocket, size: usize) -> Result<(), std::io::Error> {
    let sock_ref = socket2::SockRef::from(socket);
    sock_ref.set_send_buffer_size(size)?;
    sock_ref.set_recv_buffer_size(size)?;
    Ok(())
}

fn bind_worker_socket(
    bind_addr: SocketAddr,
    reuse_port: bool,
) -> Result<UdpSocket, Http3NativeError> {
    if !reuse_port {
        return UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io);
    }

    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket =
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(Http3NativeError::Io)?;
    socket
        .set_reuse_address(true)
        .map_err(Http3NativeError::Io)?;
    #[cfg(unix)]
    set_unix_reuse_port(&socket).map_err(Http3NativeError::Io)?;
    socket
        .bind(&bind_addr.into())
        .map_err(Http3NativeError::Io)?;
    Ok(socket.into())
}

#[cfg(unix)]
fn set_unix_reuse_port(socket: &socket2::Socket) -> Result<(), std::io::Error> {
    use std::os::fd::AsRawFd;

    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: `fd` is a valid socket descriptor and we pass a valid pointer
    // to an initialized integer option value with the correct length.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[inline]
fn should_accept_client_peer(_server_addr: SocketAddr, _peer: SocketAddr) -> bool {
    // Accept packets from any peer address. QUIC path validation inside quiche
    // decides whether the packet is valid for this connection.
    true
}

#[cfg(test)]
mod tests {
    use super::should_accept_client_peer;
    use std::net::SocketAddr;

    #[test]
    fn allows_packets_after_peer_address_change() {
        let original: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let migrated: SocketAddr = "127.0.0.1:444".parse().expect("valid addr");
        assert!(should_accept_client_peer(original, migrated));
    }
}
