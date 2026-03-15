use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use crate::connection::ConnectionMetrics;
use crate::error::Http3NativeError;
use crate::h3_event::JsH3Event;

/// A raw QUIC connection (no HTTP/3 framing).
/// Streams carry opaque byte data, not HTTP semantics.
pub struct QuicConnection {
    pub quiche_conn: quiche::Connection,
    pub conn_id: Vec<u8>,
    pub created_at: Instant,
    pub is_established: bool,
    pub handshake_complete_emitted: bool,
    pub metrics: ConnectionMetrics,
    /// Streams blocked on flow control.
    pub blocked_streams: HashSet<u64>,
    /// Tracks which stream IDs we have already emitted NEW_STREAM for.
    pub known_streams: HashSet<u64>,
    pub qlog_path: Option<String>,
    pub session_ticket: Option<Vec<u8>>,
}

pub struct QuicConnectionInit<'a> {
    pub role: &'a str,
    pub qlog_dir: Option<&'a str>,
    pub qlog_level: Option<&'a str>,
}

impl QuicConnection {
    pub fn new(
        mut quiche_conn: quiche::Connection,
        conn_id: Vec<u8>,
        init: QuicConnectionInit<'_>,
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
            conn_id,
            created_at: Instant::now(),
            is_established: false,
            handshake_complete_emitted: false,
            metrics: ConnectionMetrics::new(),
            blocked_streams: HashSet::new(),
            known_streams: HashSet::new(),
            qlog_path,
            session_ticket: None,
        }
    }

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

    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.quiche_conn.timeout()
    }

    pub fn on_timeout(&mut self) {
        self.quiche_conn.on_timeout();
    }

    pub fn mark_established(&mut self) {
        if !self.is_established {
            self.is_established = true;
            self.metrics.handshake_complete_at = Some(Instant::now());
        }
    }

    /// Poll for readable QUIC streams and emit data / finished / new-stream events.
    pub fn poll_quic_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let readable: Vec<u64> = self.quiche_conn.readable().collect();

        let mut recv_buf = [0u8; 65535];
        for stream_id in readable {
            // Emit new-stream event for streams we haven't seen yet
            if self.known_streams.insert(stream_id) {
                events.push(JsH3Event::new_stream(conn_handle, stream_id));
            }

            loop {
                match self.quiche_conn.stream_recv(stream_id, &mut recv_buf) {
                    Ok((len, fin)) => {
                        if len > 0 {
                            events.push(JsH3Event::data(
                                conn_handle,
                                stream_id,
                                recv_buf[..len].to_vec(),
                                fin,
                            ));
                        }
                        if fin {
                            events.push(JsH3Event::finished(conn_handle, stream_id));
                            // Stream is done receiving — remove from tracking set
                            // to bound memory on long-lived connections.
                            self.known_streams.remove(&stream_id);
                            break;
                        }
                        if len == 0 {
                            break;
                        }
                    }
                    Err(quiche::Error::Done) => break,
                    Err(e) => {
                        events.push(JsH3Event::error(
                            conn_handle,
                            stream_id as i64,
                            0,
                            e.to_string(),
                        ));
                        self.known_streams.remove(&stream_id);
                        break;
                    }
                }
            }
        }

        self.poll_datagram_events(conn_handle, events);
        self.poll_drain_events(conn_handle, events);
    }

    pub fn poll_drain_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let blocked: Vec<u64> = self.blocked_streams.iter().copied().collect();
        for stream_id in blocked {
            match self.quiche_conn.stream_writable(stream_id, 1) {
                Ok(true) => {
                    self.blocked_streams.remove(&stream_id);
                    events.push(JsH3Event::drain(conn_handle, stream_id));
                }
                Err(_) => {
                    self.blocked_streams.remove(&stream_id);
                }
                Ok(false) => {}
            }
        }
    }

    /// Send raw data on a QUIC stream. Returns bytes written.
    pub fn stream_send(
        &mut self,
        stream_id: u64,
        data: &[u8],
        fin: bool,
    ) -> Result<usize, Http3NativeError> {
        match self.quiche_conn.stream_send(stream_id, data, fin) {
            Ok(written) => {
                if written < data.len() {
                    self.blocked_streams.insert(stream_id);
                }
                self.known_streams.insert(stream_id);
                Ok(written)
            }
            Err(quiche::Error::Done) => {
                self.blocked_streams.insert(stream_id);
                Ok(0)
            }
            Err(e) => Err(Http3NativeError::Quiche(e)),
        }
    }

    pub fn stream_close(
        &mut self,
        stream_id: u64,
        error_code: u64,
    ) -> Result<(), Http3NativeError> {
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Read, error_code)
            .ok();
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Write, error_code)
            .ok();
        self.blocked_streams.remove(&stream_id);
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.quiche_conn.is_closed()
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
                Ok(len) => {
                    events.push(JsH3Event::datagram(conn_handle, recv_buf[..len].to_vec()));
                }
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
    file_path.push(format!("quic-{role}-{conn_hex}.qlog"));
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
        format!("quic-{role}"),
        "nodejs_http3 QUIC session trace".to_string(),
        level,
    );

    Some(file_path.to_string_lossy().into_owned())
}
