//! Per-connection raw QUIC state (no HTTP/3 framing), managing bidirectional
//! streams, send buffering, and event generation for the worker loop.

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use crate::arc_buf::{ArcBuf, ArcBufFactory};
use crate::buffer_pool::AdaptiveBufferPool;
use crate::connection::ConnectionMetrics;
use crate::error::Http3NativeError;
use crate::h3_event::JsH3Event;
use crate::reactor_metrics;

/// A raw QUIC connection (no HTTP/3 framing).
/// Streams carry opaque byte data, not HTTP semantics.
pub struct QuicConnection {
    pub quiche_conn: quiche::Connection<ArcBufFactory>,
    pub conn_id: Vec<u8>,
    pub created_at: Instant,
    pub is_established: bool,
    pub handshake_complete_emitted: bool,
    pub metrics: ConnectionMetrics,
    /// Blocked-stream iteration queue (front-to-back, checked once per call).
    pub blocked_queue: VecDeque<u64>,
    /// Dedup set for blocked_queue — prevents duplicate entries.
    pub blocked_set: HashSet<u64>,
    /// Tracks which stream IDs we have already emitted NEW_STREAM for.
    pub known_streams: HashSet<u64>,
    /// Streams where stream_recv returned Done but stream_finished() was false.
    /// Only these are checked in sweep_finished_streams (avoids O(all_streams) scan).
    pub pending_fin: HashSet<u64>,
    pub qlog_path: Option<String>,
    pub session_ticket: Option<Vec<u8>>,
    /// Pool for stream_recv output buffers — each checkout becomes the event data
    /// Vec directly, avoiding an intermediate copy. Buffers are NOT returned to
    /// the pool (they're consumed by TSFN/JS), but the pool reduces malloc churn
    /// when quiche delivers data faster than JS consumes events.
    pub data_pool: AdaptiveBufferPool,
}

pub struct QuicConnectionInit<'a> {
    pub role: &'a str,
    pub qlog_dir: Option<&'a str>,
    pub qlog_level: Option<&'a str>,
}

impl QuicConnection {
    fn is_local_stream(&self, stream_id: u64) -> bool {
        let local_initiator_bit = u64::from(self.quiche_conn.is_server());
        (stream_id & 0x1) == local_initiator_bit
    }

    pub fn new(
        mut quiche_conn: quiche::Connection<ArcBufFactory>,
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
            blocked_queue: VecDeque::new(),
            blocked_set: HashSet::new(),
            known_streams: HashSet::new(),
            pending_fin: HashSet::new(),
            qlog_path,
            session_ticket: None,
            data_pool: AdaptiveBufferPool::new(64, 4096),
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
    ///
    /// For new streams, the first `stream_recv` is coalesced into the
    /// NEW_STREAM event — saving one TSFN event per new stream (~33% fewer
    /// events for the typical new-stream lifecycle).
    pub fn poll_quic_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let readable: Vec<u64> = self.quiche_conn.readable().collect();

        for stream_id in readable {
            let is_new = self.known_streams.insert(stream_id);

            if is_new {
                // Coalesce first recv into NEW_STREAM event.
                // Checkout a pool buffer as the direct recv target — quiche copies
                // into it, then we truncate and move ownership to the event (no
                // intermediate copy).
                let (mut buf, _) = self.data_pool.checkout(65535);
                match self.quiche_conn.stream_recv(stream_id, &mut buf) {
                    Ok((len, fin)) => {
                        buf.truncate(len);
                        events.push(JsH3Event::new_stream_with_data(
                            conn_handle,
                            stream_id,
                            buf,
                            fin,
                        ));
                        if fin {
                            reactor_metrics::record_raw_quic_fin_observed();
                            self.known_streams.remove(&stream_id);
                            self.quiche_conn
                                .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                                .ok();
                            continue;
                        }
                        // Fall through to drain remaining data on this stream.
                    }
                    Err(quiche::Error::Done) => {
                        self.data_pool.checkin(buf);
                        events.push(JsH3Event::new_stream(conn_handle, stream_id));
                        continue;
                    }
                    Err(e) => {
                        self.data_pool.checkin(buf);
                        events.push(JsH3Event::new_stream(conn_handle, stream_id));
                        events.push(JsH3Event::error(
                            conn_handle,
                            stream_id as i64,
                            0,
                            e.to_string(),
                        ));
                        self.known_streams.remove(&stream_id);
                        continue;
                    }
                }
            }

            loop {
                let (mut buf, _) = self.data_pool.checkout(65535);
                match self.quiche_conn.stream_recv(stream_id, &mut buf) {
                    Ok((len, fin)) => {
                        if len > 0 {
                            buf.truncate(len);
                            events.push(JsH3Event::data(conn_handle, stream_id, buf, fin));
                        } else {
                            self.data_pool.checkin(buf);
                        }
                        if fin {
                            reactor_metrics::record_raw_quic_fin_observed();
                            reactor_metrics::record_raw_quic_finished_event();
                            events.push(JsH3Event::finished(conn_handle, stream_id));
                            self.known_streams.remove(&stream_id);
                            self.quiche_conn
                                .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                                .ok();
                            break;
                        }
                        if len == 0 {
                            break;
                        }
                    }
                    Err(quiche::Error::Done) => {
                        self.data_pool.checkin(buf);
                        if self.quiche_conn.stream_finished(stream_id) {
                            reactor_metrics::record_raw_quic_fin_observed();
                            reactor_metrics::record_raw_quic_finished_event();
                            events.push(JsH3Event::finished(conn_handle, stream_id));
                            self.known_streams.remove(&stream_id);
                            self.pending_fin.remove(&stream_id);
                            self.quiche_conn
                                .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                                .ok();
                        } else {
                            self.pending_fin.insert(stream_id);
                        }
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
                        self.known_streams.remove(&stream_id);
                        break;
                    }
                }
            }
        }

        // NOTE: sweep_finished_streams is called by the handler at lower frequency
        // (timer ticks) to avoid O(pending_fin) on every packet.
        self.poll_datagram_events(conn_handle, events);
        self.poll_drain_events(conn_handle, events);
    }

    /// Check only the `pending_fin` set for streams where quiche received FIN
    /// after we already drained all data via `stream_recv`. This is O(pending)
    /// instead of the previous O(all_known_streams) — typically near zero.
    pub fn sweep_finished_streams(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        if self.pending_fin.is_empty() {
            return;
        }
        let mut newly_finished = Vec::new();
        for &stream_id in &self.pending_fin {
            if self.quiche_conn.stream_finished(stream_id)
                && !self.quiche_conn.stream_readable(stream_id)
            {
                newly_finished.push(stream_id);
            }
        }
        for stream_id in newly_finished {
            self.pending_fin.remove(&stream_id);
            if !self.known_streams.remove(&stream_id) {
                continue;
            }
            reactor_metrics::record_raw_quic_fin_observed();
            reactor_metrics::record_raw_quic_finished_event();
            events.push(JsH3Event::finished(conn_handle, stream_id));
            self.quiche_conn
                .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                .ok();
        }
    }

    pub fn poll_drain_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        let len = self.blocked_queue.len();
        for _ in 0..len {
            let Some(stream_id) = self.blocked_queue.pop_front() else {
                break;
            };
            match self.quiche_conn.stream_writable(stream_id, 1) {
                Ok(true) => {
                    self.blocked_set.remove(&stream_id);
                    reactor_metrics::record_raw_quic_drain_event();
                    events.push(JsH3Event::drain(conn_handle, stream_id));
                }
                Err(e) => {
                    self.blocked_set.remove(&stream_id);
                    events.push(JsH3Event::error(
                        conn_handle,
                        stream_id as i64,
                        0,
                        format!("stream_writable failed: {e}"),
                    ));
                }
                Ok(false) => {
                    // Still blocked — re-enqueue at back
                    self.blocked_queue.push_back(stream_id);
                }
            }
        }
    }

    /// Send raw data on a QUIC stream using zero-copy. Returns
    /// `(bytes_written, optional remainder)`.
    ///
    /// Takes ownership of the data `Vec`, wraps it in an `ArcBuf`, and hands
    /// it to quiche's `stream_send_zc` — avoiding an internal memcpy.
    ///
    /// For FIN-only sends (empty data + fin=true), quiche returns Ok(0) on
    /// success. To distinguish that from `Err(Done)` (blocked), this method
    /// returns written=1 when a FIN-only send is accepted by quiche — callers
    /// can treat any non-zero return as "progress was made."
    pub fn stream_send(
        &mut self,
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
    ) -> Result<(usize, Option<Vec<u8>>), Http3NativeError> {
        let data_len = data.len();
        let buf = ArcBuf::from_vec(data);
        match self.quiche_conn.stream_send_zc(stream_id, buf, None, fin) {
            Ok((written, remaining)) => {
                if written < data_len && self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
                if self.is_local_stream(stream_id) {
                    self.known_streams.insert(stream_id);
                }
                // Convert remaining ArcBuf back to Vec for pending-write storage.
                let remainder = remaining.map(|r| r.as_ref().to_vec());
                // Sentinel for FIN-only sends.
                if data_len == 0 && fin && written == 0 {
                    Ok((1, None))
                } else {
                    Ok((written, remainder))
                }
            }
            Err(quiche::Error::Done) => {
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
                Ok((0, None))
            }
            Err(e) => Err(Http3NativeError::Quiche(e)),
        }
    }

    pub fn stream_close(
        &mut self,
        stream_id: u64,
        error_code: u64,
    ) -> Result<(), Http3NativeError> {
        reactor_metrics::record_lifecycle_trace(
            "quic-connection",
            "stream-close",
            None,
            None,
            None,
            Some(format!(
                "stream_id={stream_id} error_code={error_code} blocked_streams={} known_streams={}",
                self.blocked_set.len(),
                self.known_streams.len()
            )),
        );
        // .ok() intentional: shutdown may fail with Done (already closed) or
        // InvalidStreamState (peer reset). Either way we want to clean up
        // our tracking state — the stream is going away.
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Read, error_code)
            .ok();
        self.quiche_conn
            .stream_shutdown(stream_id, quiche::Shutdown::Write, error_code)
            .ok();
        if self.blocked_set.remove(&stream_id) {
            self.blocked_queue.retain(|&id| id != stream_id);
        }
        self.known_streams.remove(&stream_id);
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
        loop {
            let (mut buf, _) = self.data_pool.checkout(65535);
            match self.quiche_conn.dgram_recv(&mut buf) {
                Ok(len) => {
                    buf.truncate(len);
                    events.push(JsH3Event::datagram(conn_handle, buf));
                }
                Err(quiche::Error::Done) => {
                    self.data_pool.checkin(buf);
                    break;
                }
                Err(_) => {
                    self.data_pool.checkin(buf);
                    break;
                }
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

    pub fn pmtu(&self) -> usize {
        self.quiche_conn.path_stats().next().map_or(0, |s| s.pmtu)
    }
}

fn maybe_enable_qlog(
    quiche_conn: &mut quiche::Connection<ArcBufFactory>,
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
