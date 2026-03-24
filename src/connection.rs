//! Per-connection HTTP/3 state machine wrapping a `quiche::h3::Connection`,
//! managing stream tracking, header/data dispatch, and send buffering.

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use quiche::h3::NameValue;

use crate::buffer_pool::AdaptiveBufferPool;
use crate::error::Http3NativeError;
use crate::h3_event::{JsH3Event, JsHeader};
use crate::reactor_metrics;

pub struct H3Connection {
    pub quiche_conn: quiche::Connection,
    pub h3_conn: Option<quiche::h3::Connection>,
    pub conn_id: Vec<u8>,
    pub created_at: Instant,
    pub is_established: bool,
    pub handshake_complete_emitted: bool,
    pub metrics: ConnectionMetrics,
    /// Blocked-stream iteration queue (front-to-back, checked once per call).
    pub blocked_queue: VecDeque<u64>,
    /// Dedup set for blocked_queue — prevents duplicate entries.
    pub blocked_set: HashSet<u64>,
    pub qlog_path: Option<String>,
    pub session_ticket: Option<Vec<u8>>,
    pub qpack_max_table_capacity: Option<u64>,
    pub qpack_blocked_streams: Option<u64>,
    pub last_peer_stream_id: u64,
    /// Pool for recv_body output — buffers become event data directly.
    pub data_pool: AdaptiveBufferPool,
}

pub struct H3ConnectionInit<'a> {
    pub role: &'a str,
    pub qlog_dir: Option<&'a str>,
    pub qlog_level: Option<&'a str>,
    pub qpack_max_table_capacity: Option<u64>,
    pub qpack_blocked_streams: Option<u64>,
}

pub struct ConnectionMetrics {
    pub packets_in: u32,
    pub packets_out: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub handshake_complete_at: Option<Instant>,
}

impl ConnectionMetrics {
    pub fn new() -> Self {
        Self {
            packets_in: 0,
            packets_out: 0,
            bytes_in: 0,
            bytes_out: 0,
            handshake_complete_at: None,
        }
    }
}

impl H3Connection {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        mut quiche_conn: quiche::Connection,
        conn_id: Vec<u8>,
        init: H3ConnectionInit<'_>,
    ) -> Self {
        let qlog_path = maybe_enable_qlog(
            &mut quiche_conn,
            &conn_id,
            init.role,
            init.qlog_dir,
            init.qlog_level,
        );
        Self {
            quiche_conn,
            h3_conn: None,
            conn_id,
            created_at: Instant::now(),
            is_established: false,
            handshake_complete_emitted: false,
            metrics: ConnectionMetrics::new(),
            blocked_queue: VecDeque::new(),
            blocked_set: HashSet::new(),
            qlog_path,
            session_ticket: None,
            qpack_max_table_capacity: init.qpack_max_table_capacity,
            qpack_blocked_streams: init.qpack_blocked_streams,
            last_peer_stream_id: 0,
            data_pool: AdaptiveBufferPool::new(64, 4096),
        }
    }

    /// Feed a received packet into the connection.
    pub fn recv(
        &mut self,
        buf: &mut [u8],
        recv_info: quiche::RecvInfo,
    ) -> Result<usize, Http3NativeError> {
        let len = self
            .quiche_conn
            .recv(buf, recv_info)
            .map_err(Http3NativeError::Quiche)?;
        self.metrics.packets_in += 1;
        self.metrics.bytes_in += len as u64;
        Ok(len)
    }

    /// Initialize the H3 connection once the QUIC handshake completes.
    pub fn init_h3(&mut self) -> Result<(), Http3NativeError> {
        if self.h3_conn.is_some() {
            return Ok(());
        }
        let mut h3_config = quiche::h3::Config::new().map_err(Http3NativeError::H3)?;
        if let Some(capacity) = self.qpack_max_table_capacity {
            h3_config.set_qpack_max_table_capacity(capacity);
        }
        if let Some(blocked) = self.qpack_blocked_streams {
            h3_config.set_qpack_blocked_streams(blocked);
        }
        let h3_conn = quiche::h3::Connection::with_transport(&mut self.quiche_conn, &h3_config)
            .map_err(Http3NativeError::H3)?;
        self.h3_conn = Some(h3_conn);
        self.is_established = true;
        self.metrics.handshake_complete_at = Some(Instant::now());
        Ok(())
    }

    /// Poll for H3 events and push them into the provided batch.
    pub fn poll_h3_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        if let Some(h3_conn) = self.h3_conn.as_mut() {
            loop {
                match h3_conn.poll(&mut self.quiche_conn) {
                    Ok((stream_id, quiche::h3::Event::Headers { list, more_frames })) => {
                        self.last_peer_stream_id = stream_id;
                        let headers: Vec<JsHeader> = list
                            .into_iter()
                            .map(|h| JsHeader {
                                name: String::from_utf8_lossy(h.name()).into_owned(),
                                value: String::from_utf8_lossy(h.value()).into_owned(),
                            })
                            .collect();
                        events.push(JsH3Event::headers(
                            conn_handle,
                            stream_id,
                            headers,
                            !more_frames,
                        ));
                    }
                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        self.last_peer_stream_id = stream_id;
                        loop {
                            let (mut buf, _) = self.data_pool.checkout(16384);
                            match h3_conn.recv_body(
                                &mut self.quiche_conn,
                                stream_id,
                                &mut buf,
                            ) {
                                Ok(len) => {
                                    buf.truncate(len);
                                    events.push(JsH3Event::data(
                                        conn_handle,
                                        stream_id,
                                        buf,
                                        false,
                                    ));
                                }
                                Err(quiche::h3::Error::Done) => {
                                    self.data_pool.checkin(buf);
                                    break;
                                }
                                Err(e) => {
                                    self.data_pool.checkin(buf);
                                    events.push(JsH3Event::error(
                                        conn_handle,
                                        stream_id as i64,
                                        0,
                                        e.to_string(),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                    Ok((stream_id, quiche::h3::Event::Finished)) => {
                        self.last_peer_stream_id = stream_id;
                        events.push(JsH3Event::finished(conn_handle, stream_id));
                    }
                    Ok((stream_id, quiche::h3::Event::Reset(error_code))) => {
                        self.last_peer_stream_id = stream_id;
                        events.push(JsH3Event::reset(conn_handle, stream_id, error_code));
                    }
                    Ok((_, quiche::h3::Event::PriorityUpdate)) => {
                        // Ignore priority updates for now
                    }
                    Ok((stream_id, quiche::h3::Event::GoAway)) => {
                        self.last_peer_stream_id = stream_id;
                        events.push(JsH3Event::goaway(conn_handle, stream_id));
                    }
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => {
                        events.push(JsH3Event::error(conn_handle, -1, 0, e.to_string()));
                        break;
                    }
                }
            }
        }

        self.poll_datagram_events(conn_handle, events);

        // Check for newly writable streams (drain events).
        // Also remove entries for streams that are finished/closed.
        self.poll_drain_events(conn_handle, events);
    }

    /// Check blocked streams for drain events without polling H3 events.
    pub fn poll_drain_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let len = self.blocked_queue.len();
        for _ in 0..len {
            let Some(stream_id) = self.blocked_queue.pop_front() else {
                break;
            };
            match self.quiche_conn.stream_writable(stream_id, 1) {
                Ok(true) => {
                    self.blocked_set.remove(&stream_id);
                    events.push(JsH3Event::drain(conn_handle, stream_id));
                }
                Err(_) => {
                    self.blocked_set.remove(&stream_id);
                }
                Ok(false) => {
                    // Still blocked — re-enqueue at back
                    self.blocked_queue.push_back(stream_id);
                }
            }
        }
    }

    /// Get outbound packets from quiche.
    pub fn send(&mut self, out: &mut [u8]) -> Result<(usize, quiche::SendInfo), Http3NativeError> {
        match self.quiche_conn.send(out) {
            Ok((len, info)) => {
                self.metrics.packets_out += 1;
                self.metrics.bytes_out += len as u64;
                Ok((len, info))
            }
            Err(quiche::Error::Done) => Err(Http3NativeError::Quiche(quiche::Error::Done)),
            Err(e) => Err(Http3NativeError::Quiche(e)),
        }
    }

    /// Get timeout duration for this connection.
    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.quiche_conn.timeout()
    }

    /// Process a timeout event.
    pub fn on_timeout(&mut self) {
        self.quiche_conn.on_timeout();
    }

    /// Send response headers on a stream.
    pub fn send_response(
        &mut self,
        stream_id: u64,
        headers: &[quiche::h3::Header],
        fin: bool,
    ) -> Result<(), Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        h3.send_response(&mut self.quiche_conn, stream_id, headers, fin)
            .map_err(Http3NativeError::H3)
    }

    /// Send a request (client-side). Returns the stream ID.
    pub fn send_request(
        &mut self,
        headers: &[quiche::h3::Header],
        fin: bool,
    ) -> Result<u64, Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        h3.send_request(&mut self.quiche_conn, headers, fin)
            .map_err(Http3NativeError::H3)
    }

    /// Send body data on a stream. Returns bytes written.
    pub fn send_body(
        &mut self,
        stream_id: u64,
        data: &[u8],
        fin: bool,
    ) -> Result<usize, Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        match h3.send_body(&mut self.quiche_conn, stream_id, data, fin) {
            Ok(written) => {
                if written < data.len() && self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                }
                Ok(written)
            }
            Err(quiche::h3::Error::Done) => {
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                }
                Ok(0)
            }
            Err(e) => Err(Http3NativeError::H3(e)),
        }
    }

    /// Close/reset a stream.
    pub fn stream_close(
        &mut self,
        stream_id: u64,
        error_code: u64,
    ) -> Result<(), Http3NativeError> {
        reactor_metrics::record_lifecycle_trace(
            "h3-connection",
            "stream-close",
            None,
            None,
            None,
            Some(format!(
                "stream_id={stream_id} error_code={error_code} blocked_streams={}",
                self.blocked_set.len()
            )),
        );
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Read, error_code)
            .ok(); // Ignore errors if already closed
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Write, error_code)
            .ok();
        if self.blocked_set.remove(&stream_id) {
            self.blocked_queue.retain(|&id| id != stream_id);
        }
        Ok(())
    }

    /// Send trailing headers.
    pub fn send_trailers(
        &mut self,
        stream_id: u64,
        headers: &[quiche::h3::Header],
    ) -> Result<(), Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        // Send headers with fin=true to indicate end of stream
        h3.send_response(&mut self.quiche_conn, stream_id, headers, true)
            .map_err(Http3NativeError::H3)
    }

    /// Is the connection closed?
    pub fn is_closed(&self) -> bool {
        self.quiche_conn.is_closed()
    }

    pub fn send_goaway(&mut self) -> Result<(), Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        reactor_metrics::record_lifecycle_trace(
            "h3-connection",
            "send-goaway",
            None,
            None,
            None,
            Some(format!(
                "last_peer_stream_id={} blocked_streams={}",
                self.last_peer_stream_id,
                self.blocked_set.len()
            )),
        );
        h3.send_goaway(&mut self.quiche_conn, self.last_peer_stream_id)
            .map_err(Http3NativeError::H3)
    }

    pub fn send_datagram(&mut self, data: &[u8]) -> Result<(), Http3NativeError> {
        self.quiche_conn
            .dgram_send(data)
            .map(|_| ())
            .map_err(Http3NativeError::Quiche)
    }

    pub fn poll_datagram_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let mut recv_buf = [0u8; 65535];
        loop {
            match self.quiche_conn.dgram_recv(&mut recv_buf) {
                Ok(len) => events.push(JsH3Event::datagram(conn_handle, recv_buf[..len].to_vec())),
                Err(quiche::Error::Done) => break,
                Err(_) => break,
            }
        }
    }

    pub fn update_session_ticket(&mut self) -> Option<Vec<u8>> {
        let ticket = self.quiche_conn.session()?.to_vec();
        let changed = self
            .session_ticket
            .as_ref()
            .is_none_or(|prev| prev.as_slice() != ticket.as_slice());
        if !changed {
            return None;
        }
        self.session_ticket = Some(ticket.clone());
        Some(ticket)
    }

    pub fn remote_settings(&self) -> Vec<(u64, u64)> {
        self.h3_conn
            .as_ref()
            .and_then(quiche::h3::Connection::peer_settings_raw)
            .map_or_else(Vec::new, <[(u64, u64)]>::to_vec)
    }

    pub fn handshake_time_ms(&self) -> f64 {
        self.metrics.handshake_complete_at.map_or(0.0, |t| {
            t.duration_since(self.created_at).as_secs_f64() * 1000.0
        })
    }

    pub fn rtt_ms(&self) -> f64 {
        self.quiche_conn
            .path_stats()
            .next()
            .map_or(0.0, |s| s.rtt.as_secs_f64() * 1000.0)
    }

    pub fn cwnd(&self) -> u64 {
        self.quiche_conn
            .path_stats()
            .next()
            .map_or(0, |s| s.cwnd as u64)
    }

    pub fn pmtu(&self) -> usize {
        self.quiche_conn.path_stats().next().map_or(0, |s| s.pmtu)
    }
}

fn maybe_enable_qlog(
    quiche_conn: &mut quiche::Connection,
    conn_id: &[u8],
    role: &str,
    qlog_dir: Option<&str>,
    qlog_level: Option<&str>,
) -> Option<String> {
    let dir = qlog_dir?;
    let mut file_path = PathBuf::from(dir);
    if std::fs::create_dir_all(&file_path).is_err() {
        return None;
    }
    let mut conn_hex = String::with_capacity(conn_id.len() * 2);
    for byte in conn_id {
        let _ = write!(&mut conn_hex, "{byte:02x}");
    }
    file_path.push(format!("{role}-{conn_hex}.qlog"));
    let Ok(file) = std::fs::File::create(&file_path) else {
        return None;
    };

    let level = match qlog_level
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("core") => quiche::QlogLevel::Core,
        Some("extra") => quiche::QlogLevel::Extra,
        _ => quiche::QlogLevel::Base,
    };

    quiche_conn.set_qlog_with_level(
        Box::new(file),
        format!("http3-{role}"),
        "nodejs_http3 session trace".to_string(),
        level,
    );

    Some(file_path.to_string_lossy().into_owned())
}
