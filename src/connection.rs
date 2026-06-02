//! Per-connection HTTP/3 state machine wrapping a `quiche::h3::Connection`,
//! managing stream tracking, header/data dispatch, and send buffering.

#![deny(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use quiche::h3::NameValue;

use crate::arc_buf::{ArcBuf, ArcBufFactory};
use crate::buffer_pool::{AdaptiveBufferPool, BufferRecycler};
use crate::chunk_pool::Chunk;
use crate::error::Http3NativeError;
use crate::h3_event::{EventRecycler, JsH3Event, JsHeader};
use crate::ping_state::PingState;
use crate::reactor_metrics;
use crate::unsafe_boundary::QuicheRecvBuf;

const H3_RECV_CHUNK: usize = 64 * 1024;

struct PendingRecvBody {
    buf: QuicheRecvBuf,
}

fn take_pending_body_event(
    pending_body_data: &mut HashMap<u64, PendingRecvBody>,
    data_pool: &mut AdaptiveBufferPool,
    conn_handle: u32,
    stream_id: u64,
    recycler: EventRecycler,
) -> Option<JsH3Event> {
    let pending = pending_body_data.remove(&stream_id)?;
    if pending.buf.is_empty() {
        data_pool.checkin(pending.buf.into_recycle_vec());
        return None;
    }

    let buf = pending.buf.into_initialized_vec();
    Some(JsH3Event::data(
        conn_handle,
        stream_id,
        buf,
        false,
        recycler,
    ))
}

fn push_pending_body_event(
    pending_body_data: &mut HashMap<u64, PendingRecvBody>,
    data_pool: &mut AdaptiveBufferPool,
    conn_handle: u32,
    stream_id: u64,
    recycler: EventRecycler,
    events: &mut Vec<JsH3Event>,
) -> bool {
    let Some(event) = take_pending_body_event(
        pending_body_data,
        data_pool,
        conn_handle,
        stream_id,
        recycler,
    ) else {
        return false;
    };

    if let Some(last) = events.last_mut() {
        match last.try_coalesce_following_data(event) {
            Ok(()) => return false,
            Err(event) => events.push(event),
        }
    } else {
        events.push(event);
    }
    true
}

pub struct H3Connection {
    pub quiche_conn: quiche::Connection<ArcBufFactory>,
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
    /// Next client-initiated request stream ID we expect quiche::h3 to use.
    ///
    /// quiche keeps this field private, but `send_request()` only advances it
    /// after the request HEADERS have been buffered. Tracking the successful
    /// stream IDs lets us route `StreamBlocked` during request creation into
    /// the existing drain machinery, so JS can retry when quiche reports the
    /// same stream writable again.
    pub next_client_request_stream_id_hint: u64,
    /// Largest peer-initiated request stream ID we have accepted (i.e.
    /// produced a Headers event for). The server's outgoing GOAWAY ID is the
    /// exclusive rejection boundary, so it is derived as this ID + 4.
    pub largest_accepted_request_id: Option<u64>,
    /// Pool for recv_body output — buffers become event data directly.
    pub data_pool: AdaptiveBufferPool,
    /// Thread-safe handle cloned into each data event so V8 GC can return
    /// the buffer to `data_pool` instead of freeing it.
    pub(crate) data_recycler: Arc<BufferRecycler>,
    /// Receiving end of the recycler channel — drained periodically on the
    /// worker thread to pull returned buffers back into `data_pool`.
    pub(crate) data_recycle_rx: crossbeam_channel::Receiver<Vec<u8>>,
    pending_body_data: HashMap<u64, PendingRecvBody>,
    trailer_fin_streams: HashSet<u64>,
    ping_state: PingState,
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

/// Result of a zero-copy H3 body send.
pub struct SendBodyOutcome {
    pub written: usize,
    /// True when a requested FIN was accepted by quiche. Empty-FIN sends
    /// legitimately return `written == 0`, so callers must not infer this
    /// from byte progress.
    pub fin_accepted: bool,
    pub remainder: Option<ArcBuf>,
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
        mut quiche_conn: quiche::Connection<ArcBufFactory>,
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
        let (data_pool, data_recycler, data_recycle_rx) =
            // H3 DATA receive buffers are normally 64 KiB. Keep the pool
            // right-sized so V8 finalizers recycle buffers without retaining
            // unrelated large allocations.
            AdaptiveBufferPool::with_recycler(64, H3_RECV_CHUNK);
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
            next_client_request_stream_id_hint: 0,
            largest_accepted_request_id: None,
            data_pool,
            data_recycler,
            data_recycle_rx,
            pending_body_data: HashMap::new(),
            trailer_fin_streams: HashSet::new(),
            ping_state: PingState::default(),
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
    ///
    /// `app_event_budget` applies only to JS-facing stream/datagram events.
    /// Drain events are still emitted so write-side backpressure can clear
    /// without depending on another inbound packet.
    pub fn poll_h3_events(
        &mut self,
        conn_handle: u32,
        app_event_budget: usize,
        events: &mut Vec<JsH3Event>,
    ) {
        let recycler = self.event_recycler();
        let mut app_events_remaining = app_event_budget;
        let mut closed_stream_candidates = Vec::new();
        let mut reset_streams = Vec::new();
        if app_events_remaining > 0 {
            if let Some(h3_conn) = self.h3_conn.as_mut() {
                loop {
                    if app_events_remaining == 0 {
                        break;
                    }
                    match h3_conn.poll(&mut self.quiche_conn) {
                        Ok((stream_id, quiche::h3::Event::Headers { list, more_frames })) => {
                            if push_pending_body_event(
                                &mut self.pending_body_data,
                                &mut self.data_pool,
                                conn_handle,
                                stream_id,
                                recycler.clone(),
                                events,
                            ) {
                                app_events_remaining = app_events_remaining.saturating_sub(1);
                            }
                            // Track the largest accepted request ID for our own
                            // outgoing GoAway. monotonic since quiche delivers
                            // events in order, but max() is defensive.
                            self.largest_accepted_request_id = Some(
                                self.largest_accepted_request_id
                                    .map_or(stream_id, |id| id.max(stream_id)),
                            );
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
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                        }
                        Ok((stream_id, quiche::h3::Event::Data)) => loop {
                            if app_events_remaining == 0 {
                                break;
                            }

                            let mut drained_current_event = false;
                            let mut error = None;

                            loop {
                                if !self.pending_body_data.contains_key(&stream_id) {
                                    let (buf, _) = self.data_pool.checkout_recv(H3_RECV_CHUNK);
                                    self.pending_body_data
                                        .insert(stream_id, PendingRecvBody { buf });
                                }

                                let Some(pending) = self.pending_body_data.get_mut(&stream_id)
                                else {
                                    break;
                                };
                                if pending.buf.is_full() {
                                    break;
                                }

                                match pending.buf.recv_h3_body(
                                    h3_conn,
                                    &mut self.quiche_conn,
                                    stream_id,
                                ) {
                                    Ok(len) => {
                                        if len == 0 {
                                            break;
                                        }
                                    }
                                    Err(quiche::h3::Error::Done) => {
                                        drained_current_event = true;
                                        break;
                                    }
                                    Err(e) => {
                                        error = Some(e.to_string());
                                        break;
                                    }
                                }
                            }

                            let pending_is_full = self
                                .pending_body_data
                                .get(&stream_id)
                                .is_some_and(|pending| pending.buf.is_full());
                            if self
                                .pending_body_data
                                .get(&stream_id)
                                .is_some_and(|pending| pending.buf.is_empty())
                            {
                                if let Some(pending) = self.pending_body_data.remove(&stream_id) {
                                    self.data_pool.checkin(pending.buf.into_recycle_vec());
                                }
                            }
                            // A DATA frame smaller than H3_RECV_CHUNK still needs to
                            // reach JS before FIN; `Done` marks the current frame
                            // drained, not a reason to keep buffering indefinitely.
                            if pending_is_full || drained_current_event {
                                if push_pending_body_event(
                                    &mut self.pending_body_data,
                                    &mut self.data_pool,
                                    conn_handle,
                                    stream_id,
                                    recycler.clone(),
                                    events,
                                ) {
                                    app_events_remaining = app_events_remaining.saturating_sub(1);
                                }
                            }

                            if let Some(message) = error {
                                if push_pending_body_event(
                                    &mut self.pending_body_data,
                                    &mut self.data_pool,
                                    conn_handle,
                                    stream_id,
                                    recycler.clone(),
                                    events,
                                ) {
                                    app_events_remaining = app_events_remaining.saturating_sub(1);
                                }
                                if app_events_remaining > 0 {
                                    events.push(JsH3Event::error(
                                        conn_handle,
                                        stream_id as i64,
                                        0,
                                        message,
                                    ));
                                    app_events_remaining = app_events_remaining.saturating_sub(1);
                                }
                                break;
                            }

                            if drained_current_event || !pending_is_full {
                                break;
                            }
                        },
                        Ok((stream_id, quiche::h3::Event::Finished)) => {
                            if push_pending_body_event(
                                &mut self.pending_body_data,
                                &mut self.data_pool,
                                conn_handle,
                                stream_id,
                                recycler.clone(),
                                events,
                            ) {
                                app_events_remaining = app_events_remaining.saturating_sub(1);
                            }
                            events.push(JsH3Event::finished(conn_handle, stream_id));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                            closed_stream_candidates.push(stream_id);
                        }
                        Ok((stream_id, quiche::h3::Event::Reset(error_code))) => {
                            if push_pending_body_event(
                                &mut self.pending_body_data,
                                &mut self.data_pool,
                                conn_handle,
                                stream_id,
                                recycler.clone(),
                                events,
                            ) {
                                app_events_remaining = app_events_remaining.saturating_sub(1);
                            }
                            events.push(JsH3Event::reset(conn_handle, stream_id, error_code));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                            reset_streams.push(stream_id);
                        }
                        Ok((_, quiche::h3::Event::PriorityUpdate)) => {
                            // Ignore priority updates for now
                        }
                        Ok((stream_id, quiche::h3::Event::GoAway)) => {
                            // Peer's GoAway carries *their* watermark; do NOT
                            // overwrite our own largest_accepted_request_id with
                            // it (audit finding #1).
                            events.push(JsH3Event::goaway(conn_handle, stream_id));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                        }
                        Err(quiche::h3::Error::Done) => break,
                        Err(e) => {
                            events.push(JsH3Event::error(conn_handle, -1, 0, e.to_string()));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                            break;
                        }
                    }
                }
            }
        }

        for stream_id in reset_streams {
            self.clear_stream_tracking(stream_id);
        }

        for stream_id in closed_stream_candidates {
            self.clear_stream_tracking_if_closed(stream_id);
        }

        if app_events_remaining > 0 {
            self.poll_datagram_events(conn_handle, app_events_remaining, events);
        }

        // Check for newly writable streams (drain events).
        // Also remove entries for streams that are finished/closed.
        self.poll_drain_events(conn_handle, events);
    }

    /// Check blocked streams for drain events without polling H3 events.
    pub fn poll_drain_events(&mut self, conn_handle: u32, events: &mut Vec<JsH3Event>) {
        self.prune_closed_blocked_streams();

        while let Some(stream_id) = self.quiche_conn.stream_writable_next() {
            if self.blocked_set.remove(&stream_id) {
                self.blocked_queue.retain(|&id| id != stream_id);
                events.push(JsH3Event::drain(conn_handle, stream_id));
            }
        }
    }

    fn prune_closed_blocked_streams(&mut self) {
        if self.blocked_set.is_empty() {
            return;
        }

        let closed = self
            .blocked_set
            .iter()
            .copied()
            .filter(|&stream_id| self.quiche_conn.stream_closed(stream_id))
            .collect::<Vec<_>>();

        for stream_id in closed {
            self.clear_stream_tracking(stream_id);
        }
    }

    fn clear_stream_tracking_if_closed(&mut self, stream_id: u64) {
        if self.quiche_conn.stream_closed(stream_id) {
            self.clear_stream_tracking(stream_id);
        }
    }

    fn clear_stream_tracking(&mut self, stream_id: u64) {
        if self.blocked_set.remove(&stream_id) {
            self.blocked_queue.retain(|&id| id != stream_id);
        }
        if let Some(pending) = self.pending_body_data.remove(&stream_id) {
            self.data_pool.checkin(pending.buf.into_recycle_vec());
        }
        self.trailer_fin_streams.remove(&stream_id);
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

    pub fn queue_ping(&mut self) -> Result<(), Http3NativeError> {
        if self.quiche_conn.is_closed() || self.quiche_conn.is_draining() {
            return Err(Http3NativeError::InvalidState(
                "connection is closing".into(),
            ));
        }
        self.quiche_conn
            .send_ack_eliciting()
            .map_err(Http3NativeError::Quiche)?;
        self.ping_state.queue(&self.quiche_conn.stats());
        Ok(())
    }

    pub fn poll_ping_acks(&mut self) -> Vec<f64> {
        self.ping_state.complete_acked(&self.quiche_conn.stats())
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
        match h3.send_request(&mut self.quiche_conn, headers, fin) {
            Ok(stream_id) => {
                self.next_client_request_stream_id_hint = stream_id.saturating_add(4);
                Ok(stream_id)
            }
            Err(quiche::h3::Error::StreamBlocked) => {
                let stream_id = self.next_client_request_stream_id_hint;
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                }
                Err(Http3NativeError::H3(quiche::h3::Error::StreamBlocked))
            }
            Err(error) => Err(Http3NativeError::H3(error)),
        }
    }

    /// Send body data on a stream using zero-copy. Returns bytes written.
    ///
    /// Wraps the input slice into an `ArcBuf` and uses quiche's
    /// `send_body_zc` to avoid an internal copy inside the H3 framer.
    #[deprecated(note = "use send_body_owned to avoid copying outbound body data")]
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
        let mut buf = ArcBuf::from_vec(data.to_vec());
        match h3.send_body_zc(&mut self.quiche_conn, stream_id, &mut buf, fin) {
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

    /// Send body data on a stream using zero-copy from an owned Vec.
    ///
    /// Like `send_body`, but takes ownership of the Vec to avoid an extra
    /// Zero-copy variant: wraps a `Vec<u8>` in `ArcBuf` without copying,
    /// eliminating the `data.to_vec()` copy in [`send_body`].
    ///
    /// Returns `(written, Option<remainder>)`.  On full write, remainder is
    /// `None` — the Vec is donated to quiche's ArcBuf.  On partial write,
    /// the unwritten bytes are copied out of the ArcBuf into a new Vec
    /// (one copy vs two in the old `send_body` path).
    pub fn send_body_owned(
        &mut self,
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
    ) -> Result<(usize, Option<Vec<u8>>), Http3NativeError> {
        let outcome = self.send_body_arcbuf(stream_id, ArcBuf::from_vec(data), fin)?;
        let remainder = outcome.remainder.map(|r| r.as_ref().to_vec());
        Ok((outcome.written, remainder))
    }

    /// Send body data from a pooled outbound chunk without copying.
    pub fn send_body_chunk(
        &mut self,
        stream_id: u64,
        chunk: Chunk,
        fin: bool,
    ) -> Result<SendBodyOutcome, Http3NativeError> {
        self.send_body_arcbuf(stream_id, ArcBuf::from_chunk(chunk), fin)
    }

    /// Send body data from an `ArcBuf`, returning the remaining `ArcBuf` on
    /// flow-control block so the caller can retry without flattening/copying.
    pub fn send_body_arcbuf(
        &mut self,
        stream_id: u64,
        mut buf: ArcBuf,
        fin: bool,
    ) -> Result<SendBodyOutcome, Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        let data_len = buf.remaining_len();
        if data_len == 0 && fin && self.trailer_fin_streams.contains(&stream_id) {
            return Ok(SendBodyOutcome {
                written: 0,
                fin_accepted: true,
                remainder: None,
            });
        }

        if data_len == 0 {
            if !fin {
                return Ok(SendBodyOutcome {
                    written: 0,
                    fin_accepted: false,
                    remainder: None,
                });
            }

            return match h3.send_body(&mut self.quiche_conn, stream_id, &[], true) {
                Ok(written) => Ok(SendBodyOutcome {
                    written,
                    fin_accepted: true,
                    remainder: None,
                }),
                Err(quiche::h3::Error::Done) => {
                    if self.blocked_set.insert(stream_id) {
                        self.blocked_queue.push_back(stream_id);
                    }
                    Ok(SendBodyOutcome {
                        written: 0,
                        fin_accepted: false,
                        remainder: Some(buf),
                    })
                }
                Err(e) => Err(Http3NativeError::H3(e)),
            };
        }

        match h3.send_body_zc(&mut self.quiche_conn, stream_id, &mut buf, fin) {
            Ok(written) => {
                if written < data_len && self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                }
                let remainder = if written < data_len { Some(buf) } else { None };
                Ok(SendBodyOutcome {
                    written,
                    fin_accepted: fin && remainder.is_none(),
                    remainder,
                })
            }
            Err(quiche::h3::Error::Done) => {
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                }
                Ok(SendBodyOutcome {
                    written: 0,
                    fin_accepted: false,
                    remainder: Some(buf),
                })
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
        self.clear_stream_tracking(stream_id);
        Ok(())
    }

    /// Send trailing headers.
    ///
    /// Uses quiche's `send_additional_headers` with `is_trailer_section=true`
    /// — calling `send_response` a second time would re-emit the initial
    /// response frame (invalid per H3 §4.1) and quiche silently drops it,
    /// which is what made trailers never reach the peer before this fix.
    pub fn send_trailers(
        &mut self,
        stream_id: u64,
        headers: &[quiche::h3::Header],
    ) -> Result<(), Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        h3.send_additional_headers(
            &mut self.quiche_conn,
            stream_id,
            headers,
            /* is_trailer_section */ true,
            /* fin */ true,
        )
        .map_err(Http3NativeError::H3)?;
        self.trailer_fin_streams.insert(stream_id);
        Ok(())
    }

    /// Pull returned buffers from the recycler channel back into the data pool.
    /// Called once per event-loop iteration on the worker thread.
    pub fn drain_recycled(&mut self) {
        self.data_pool.drain_returned(&self.data_recycle_rx);
    }

    /// Return the event recycler for data buffers. Under `node-api`, this is
    /// `Some(Arc<BufferRecycler>)` so V8 GC can return buffers to the pool.
    /// Without `node-api`, it is `()` (zero-cost no-op).
    fn event_recycler(&self) -> EventRecycler {
        #[cfg(feature = "node-api")]
        {
            Some(Arc::clone(&self.data_recycler))
        }
        #[cfg(not(feature = "node-api"))]
        {
            ()
        }
    }

    /// Is the connection closed?
    pub fn is_closed(&self) -> bool {
        self.quiche_conn.is_closed()
    }

    pub fn session_close_event(&self, conn_handle: u32) -> JsH3Event {
        if let Some(error) = self
            .quiche_conn
            .peer_error()
            .or_else(|| self.quiche_conn.local_error())
        {
            return JsH3Event::session_close_with_error(
                conn_handle,
                error.error_code,
                String::from_utf8_lossy(&error.reason).into_owned(),
            );
        }

        if self.quiche_conn.is_timed_out() {
            return JsH3Event::session_close_with_error(conn_handle, 0, "idle timeout".into());
        }

        JsH3Event::session_close(conn_handle)
    }

    pub fn send_goaway(&mut self) -> Result<(), Http3NativeError> {
        let h3 = self
            .h3_conn
            .as_mut()
            .ok_or_else(|| Http3NativeError::InvalidState("H3 not initialized".into()))?;
        const MAX_SERVER_GOAWAY_REQUEST_ID: u64 = (1_u64 << 62) - 4;
        let goaway_id = self
            .largest_accepted_request_id
            .map(|id| id.saturating_add(4).min(MAX_SERVER_GOAWAY_REQUEST_ID))
            .unwrap_or(0);
        reactor_metrics::record_lifecycle_trace(
            "h3-connection",
            "send-goaway",
            None,
            None,
            None,
            Some(format!(
                "largest_accepted_request_id={:?} goaway_id={} blocked_streams={}",
                self.largest_accepted_request_id,
                goaway_id,
                self.blocked_set.len()
            )),
        );
        h3.send_goaway(&mut self.quiche_conn, goaway_id)
            .map_err(Http3NativeError::H3)
    }

    pub fn send_datagram(&mut self, data: &[u8]) -> Result<(), Http3NativeError> {
        self.quiche_conn
            .dgram_send(data)
            .map(|_| ())
            .map_err(Http3NativeError::Quiche)
    }

    pub fn send_datagram_chunk(&mut self, data: Chunk) -> Result<(), Http3NativeError> {
        if self.quiche_conn.is_dgram_send_queue_full() {
            return Err(Http3NativeError::Quiche(quiche::Error::Done));
        }

        self.quiche_conn
            .dgram_send_buf(ArcBuf::from_chunk(data))
            .map(|_| ())
            .map_err(Http3NativeError::Quiche)
    }

    pub fn poll_datagram_events(
        &mut self,
        conn_handle: u32,
        app_event_budget: usize,
        events: &mut Vec<JsH3Event>,
    ) {
        let mut remaining = app_event_budget;
        loop {
            if remaining == 0 {
                break;
            }
            match self.quiche_conn.dgram_recv_buf() {
                Ok(buf) => {
                    events.push(JsH3Event::datagram(
                        conn_handle,
                        buf.into_vec(),
                        self.event_recycler(),
                    ));
                    remaining = remaining.saturating_sub(1);
                }
                Err(quiche::Error::Done) => {
                    break;
                }
                Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    const TEST_DATAGRAM_SIZE: usize = 1350;

    fn generate_test_certs() -> (String, String) {
        use rcgen::{CertificateParams, KeyPair};

        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .expect("test key should generate");
        let mut params =
            CertificateParams::new(vec!["localhost".into()]).expect("test cert params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key_pair).expect("test cert");
        (cert.pem(), key_pair.serialize_pem())
    }

    fn build_test_configs() -> (quiche::Config, quiche::Config) {
        let (cert_pem, key_pem) = generate_test_certs();
        let id = std::thread::current().id();
        let cert_path = std::env::temp_dir().join(format!("h3_conn_test_cert_{id:?}.pem"));
        let key_path = std::env::temp_dir().join(format!("h3_conn_test_key_{id:?}.pem"));
        std::fs::write(&cert_path, cert_pem).expect("write test cert");
        std::fs::write(&key_path, key_pem).expect("write test key");

        let mut server_config = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("server cfg");
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
        server_config.set_max_recv_udp_payload_size(TEST_DATAGRAM_SIZE);
        server_config.set_max_send_udp_payload_size(TEST_DATAGRAM_SIZE);
        server_config.set_initial_max_data(600);
        server_config.set_initial_max_stream_data_bidi_local(4_096);
        server_config.set_initial_max_stream_data_bidi_remote(4_096);
        server_config.set_initial_max_stream_data_uni(4_096);
        server_config.set_initial_max_streams_bidi(10_000);
        server_config.set_initial_max_streams_uni(1_000);
        server_config.set_disable_active_migration(true);

        let mut client_config = quiche::Config::new(quiche::PROTOCOL_VERSION).expect("client cfg");
        client_config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .expect("client h3 alpn");
        client_config.verify_peer(false);
        client_config.set_max_idle_timeout(30_000);
        client_config.set_max_recv_udp_payload_size(TEST_DATAGRAM_SIZE);
        client_config.set_max_send_udp_payload_size(TEST_DATAGRAM_SIZE);
        client_config.set_initial_max_data(1_000_000);
        client_config.set_initial_max_stream_data_bidi_local(1_000_000);
        client_config.set_initial_max_stream_data_bidi_remote(1_000_000);
        client_config.set_initial_max_stream_data_uni(1_000_000);
        client_config.set_initial_max_streams_bidi(10_000);
        client_config.set_initial_max_streams_uni(1_000);
        client_config.set_disable_active_migration(true);

        let _ = std::fs::remove_file(cert_path);
        let _ = std::fs::remove_file(key_path);

        (server_config, client_config)
    }

    fn exchange_handshake_packets(
        client: &mut H3Connection,
        server_conn: &mut Option<quiche::Connection<ArcBufFactory>>,
        server_config: &mut quiche::Config,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
    ) {
        let mut buf = vec![0_u8; 65_535];

        for _ in 0..100 {
            let mut progressed = false;

            loop {
                match client.send(&mut buf) {
                    Ok((len, info)) => {
                        progressed = true;
                        if server_conn.is_none() {
                            let hdr = quiche::Header::from_slice(
                                &mut buf[..len],
                                quiche::MAX_CONN_ID_LEN,
                            )
                            .expect("parse initial header");
                            let server_scid = vec![0xab; quiche::MAX_CONN_ID_LEN];
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
                                &mut buf[..len],
                                quiche::RecvInfo {
                                    from: client_addr,
                                    to: info.to,
                                },
                            )
                            .expect("server recv client packet");
                    }
                    Err(Http3NativeError::Quiche(quiche::Error::Done)) => break,
                    Err(error) => panic!("client send failed: {error}"),
                }
            }

            if let Some(server) = server_conn.as_mut() {
                loop {
                    match server.send(&mut buf) {
                        Ok((len, info)) => {
                            progressed = true;
                            client
                                .recv(
                                    &mut buf[..len],
                                    quiche::RecvInfo {
                                        from: server_addr,
                                        to: info.to,
                                    },
                                )
                                .expect("client recv server packet");
                        }
                        Err(quiche::Error::Done) => break,
                        Err(error) => panic!("server send failed: {error}"),
                    }
                }

                if client.quiche_conn.is_established() && server.is_established() {
                    return;
                }
            }

            assert!(progressed, "handshake made no progress");
        }

        panic!("handshake did not complete");
    }

    fn make_direct_h3_pair() -> (
        H3Connection,
        quiche::Connection<ArcBufFactory>,
        quiche::h3::Connection,
        SocketAddr,
        SocketAddr,
    ) {
        let (mut server_config, mut client_config) = build_test_configs();
        let client_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40_001);
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50_001);
        let scid = vec![0xba; quiche::MAX_CONN_ID_LEN];
        let scid_ref = quiche::ConnectionId::from_ref(&scid);
        let quiche_conn = quiche::connect_with_buffer_factory::<ArcBufFactory>(
            Some("localhost"),
            &scid_ref,
            client_addr,
            server_addr,
            &mut client_config,
        )
        .expect("connect client");
        let mut client = H3Connection::new(
            quiche_conn,
            scid,
            H3ConnectionInit {
                role: "client",
                qlog_dir: None,
                qlog_level: None,
                qpack_max_table_capacity: None,
                qpack_blocked_streams: None,
            },
        );
        let mut server_conn = None;
        exchange_handshake_packets(
            &mut client,
            &mut server_conn,
            &mut server_config,
            client_addr,
            server_addr,
        );
        client.init_h3().expect("client h3 init");

        let mut server_conn = server_conn.expect("server conn");
        let h3_config = quiche::h3::Config::new().expect("h3 config");
        let server_h3 = quiche::h3::Connection::with_transport(&mut server_conn, &h3_config)
            .expect("server h3 init");

        (client, server_conn, server_h3, client_addr, server_addr)
    }

    fn exchange_h3_packets(
        client: &mut H3Connection,
        server_conn: &mut quiche::Connection<ArcBufFactory>,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> bool {
        let mut buf = vec![0_u8; 65_535];
        let mut progressed = false;

        loop {
            match client.send(&mut buf) {
                Ok((len, info)) => {
                    progressed = true;
                    server_conn
                        .recv(
                            &mut buf[..len],
                            quiche::RecvInfo {
                                from: client_addr,
                                to: info.to,
                            },
                        )
                        .expect("server recv h3 packet");
                }
                Err(Http3NativeError::Quiche(quiche::Error::Done)) => break,
                Err(error) => panic!("client send failed: {error}"),
            }
        }

        loop {
            match server_conn.send(&mut buf) {
                Ok((len, info)) => {
                    progressed = true;
                    client
                        .recv(
                            &mut buf[..len],
                            quiche::RecvInfo {
                                from: server_addr,
                                to: info.to,
                            },
                        )
                        .expect("client recv h3 packet");
                }
                Err(quiche::Error::Done) => break,
                Err(error) => panic!("server send failed: {error}"),
            }
        }

        progressed
    }

    fn pump_h3_until_client_drain(
        client: &mut H3Connection,
        server_conn: &mut quiche::Connection<ArcBufFactory>,
        server_h3: &mut quiche::h3::Connection,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
        blocked_stream_id: u64,
    ) -> bool {
        let mut buf = vec![0_u8; 65_535];

        for _ in 0..200 {
            let mut progressed = false;

            loop {
                match client.send(&mut buf) {
                    Ok((len, info)) => {
                        progressed = true;
                        server_conn
                            .recv(
                                &mut buf[..len],
                                quiche::RecvInfo {
                                    from: client_addr,
                                    to: info.to,
                                },
                            )
                            .expect("server recv h3 packet");
                    }
                    Err(Http3NativeError::Quiche(quiche::Error::Done)) => break,
                    Err(error) => panic!("client send failed: {error}"),
                }
            }

            loop {
                match server_h3.poll(server_conn) {
                    Ok((_stream_id, _event)) => progressed = true,
                    Err(quiche::h3::Error::Done) => break,
                    Err(error) => panic!("server h3 poll failed: {error}"),
                }
            }

            loop {
                match server_conn.send(&mut buf) {
                    Ok((len, info)) => {
                        progressed = true;
                        client
                            .recv(
                                &mut buf[..len],
                                quiche::RecvInfo {
                                    from: server_addr,
                                    to: info.to,
                                },
                            )
                            .expect("client recv h3 packet");
                    }
                    Err(quiche::Error::Done) => break,
                    Err(error) => panic!("server send failed: {error}"),
                }
            }

            let mut events = Vec::new();
            client.poll_drain_events(7, &mut events);
            if events.iter().any(|event| {
                event.event_type == crate::h3_event::EVENT_DRAIN
                    && event.stream_id == blocked_stream_id as i64
            }) {
                return true;
            }

            if !progressed {
                return false;
            }
        }

        false
    }

    #[test]
    fn send_request_stream_blocked_tracks_and_drains_request_stream() {
        let (mut client, mut server_conn, mut server_h3, client_addr, server_addr) =
            make_direct_h3_pair();
        let pad = "x".repeat(400);
        let mut blocked_stream_id = None;

        for i in 0..256 {
            let headers = vec![
                quiche::h3::Header::new(b":method", b"GET"),
                quiche::h3::Header::new(b":scheme", b"https"),
                quiche::h3::Header::new(b":authority", b"localhost"),
                quiche::h3::Header::new(b":path", format!("/request-blocked-{i}").as_bytes()),
                quiche::h3::Header::new(b"x-pad", pad.as_bytes()),
            ];

            match client.send_request(&headers, true) {
                Ok(_) => {}
                Err(Http3NativeError::H3(quiche::h3::Error::StreamBlocked)) => {
                    blocked_stream_id = Some(client.next_client_request_stream_id_hint);
                    break;
                }
                Err(error) => panic!("unexpected send_request error: {error}"),
            }
        }

        let blocked_stream_id = blocked_stream_id.expect("send_request should hit StreamBlocked");
        assert!(
            client.blocked_set.contains(&blocked_stream_id),
            "StreamBlocked request stream should be tracked for drain"
        );

        assert!(
            pump_h3_until_client_drain(
                &mut client,
                &mut server_conn,
                &mut server_h3,
                client_addr,
                server_addr,
                blocked_stream_id,
            ),
            "peer flow-control progress should emit drain for blocked request stream"
        );
    }

    #[test]
    fn finished_event_prunes_closed_stream_tracking() {
        let (mut client, mut server_conn, mut server_h3, client_addr, server_addr) =
            make_direct_h3_pair();
        let request_headers = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b":authority", b"localhost"),
            quiche::h3::Header::new(b":path", b"/closed-stream-cleanup"),
        ];
        let stream_id = client
            .send_request(&request_headers, true)
            .expect("send request");

        let mut server_saw_finished = false;
        for _ in 0..20 {
            let mut progressed =
                exchange_h3_packets(&mut client, &mut server_conn, client_addr, server_addr);

            loop {
                match server_h3.poll(&mut server_conn) {
                    Ok((sid, quiche::h3::Event::Finished)) if sid == stream_id => {
                        server_saw_finished = true;
                        progressed = true;
                    }
                    Ok((_sid, _event)) => progressed = true,
                    Err(quiche::h3::Error::Done) => break,
                    Err(error) => panic!("server h3 poll failed: {error}"),
                }
            }

            if server_saw_finished {
                break;
            }
            assert!(progressed, "request packets made no progress before FIN");
        }
        assert!(server_saw_finished, "server should receive request FIN");

        let response_headers = vec![quiche::h3::Header::new(b":status", b"200")];
        server_h3
            .send_response(&mut server_conn, stream_id, &response_headers, true)
            .expect("send response");

        assert!(client.blocked_set.insert(stream_id));
        client.blocked_queue.push_back(stream_id);
        assert!(client.trailer_fin_streams.insert(stream_id));

        let mut events = Vec::new();
        for _ in 0..20 {
            let progressed =
                exchange_h3_packets(&mut client, &mut server_conn, client_addr, server_addr);
            client.poll_h3_events(7, 16, &mut events);

            if events.iter().any(|event| {
                event.event_type == crate::h3_event::EVENT_FINISHED
                    && event.stream_id == stream_id as i64
            }) {
                break;
            }
            assert!(progressed, "response packets made no progress before FIN");
        }

        assert!(
            events.iter().any(|event| {
                event.event_type == crate::h3_event::EVENT_FINISHED
                    && event.stream_id == stream_id as i64
            }),
            "client should receive response FIN"
        );
        assert!(
            client.quiche_conn.stream_closed(stream_id),
            "quiche should report the stream closed after both FINs"
        );
        assert!(
            !client.blocked_set.contains(&stream_id),
            "closed streams should be removed from blocked tracking"
        );
        assert!(
            !client.blocked_queue.contains(&stream_id),
            "closed streams should be removed from blocked queue"
        );
        assert!(
            !client.trailer_fin_streams.contains(&stream_id),
            "closed streams should clear deferred trailer FIN tracking"
        );
    }
}
