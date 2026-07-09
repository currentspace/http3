//! Per-connection raw QUIC state (no HTTP/3 framing), managing bidirectional
//! streams, send buffering, and event generation for the worker loop.

#![deny(unsafe_code)]

use std::collections::{HashSet, VecDeque};
#[cfg(feature = "qlog-files")]
use std::fmt::Write as _;
#[cfg(feature = "qlog-files")]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::arc_buf::{ArcBuf, ArcBufFactory};
use crate::buffer_pool::{AdaptiveBufferPool, BufferRecycler};
use crate::chunk_pool::Chunk;
use crate::connection::ConnectionMetrics;
use crate::error::Http3NativeError;
use crate::h3_event::{EventRecycler, JsH3Event};
use crate::ping_state::PingState;
use crate::reactor_metrics;

const MAX_OUTBOUND_DATAGRAM_QUEUE: usize = 64;

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
    /// Vec directly, avoiding an intermediate copy. Buffers returned from V8 GC
    /// via the recycler channel are pulled back into this pool periodically.
    pub data_pool: AdaptiveBufferPool,
    /// Thread-safe handle cloned into each data event so V8 GC can return
    /// the buffer to `data_pool` instead of freeing it.
    pub(crate) data_recycler: Arc<BufferRecycler>,
    /// Receiving end of the recycler channel — drained periodically on the
    /// worker thread to pull returned buffers back into `data_pool`.
    pub(crate) data_recycle_rx: crossbeam_channel::Receiver<Vec<u8>>,
    /// Reusable scratch buffer for `quiche_conn.readable()` collection.
    /// Avoids a fresh `Vec<u64>` allocation per `poll_quic_events` call.
    /// Audit finding #25.
    readable_scratch: Vec<u64>,
    /// Bounded retry queue for outbound QUIC DATAGRAM frames that quiche
    /// temporarily refuses with `Done`.
    outbound_datagrams: VecDeque<ArcBuf>,
    ping_state: PingState,
}

pub struct QuicConnectionInit<'a> {
    pub role: &'a str,
    pub qlog_dir: Option<&'a str>,
    pub qlog_level: Option<&'a str>,
}

/// Result of a zero-copy raw QUIC stream send.
pub struct StreamSendOutcome {
    pub written: usize,
    pub fin_accepted: bool,
    pub remainder: Option<ArcBuf>,
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
        let (data_pool, data_recycler, data_recycle_rx) =
            // Raw QUIC stream receive buffers are normally 64 KiB. Keep the
            // pool right-sized so V8 finalizers recycle buffers without
            // retaining unrelated large allocations.
            AdaptiveBufferPool::with_recycler(64, Self::RECV_CHUNK);
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
            data_pool,
            data_recycler,
            data_recycle_rx,
            readable_scratch: Vec::new(),
            outbound_datagrams: VecDeque::new(),
            ping_state: PingState::default(),
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
    /// Max buffer size for stream_recv. quiche returns partial data and we
    /// loop, so this primarily controls JS event volume at the N-API boundary.
    const RECV_CHUNK: usize = 64 * 1024;

    pub fn poll_quic_events(
        &mut self,
        conn_handle: u32,
        app_event_budget: usize,
        events: &mut Vec<JsH3Event>,
    ) {
        let mut app_events_remaining = app_event_budget;
        if app_events_remaining == 0 {
            self.poll_drain_events(conn_handle, events);
            return;
        }

        // Reuse the scratch buffer instead of allocating a fresh Vec per
        // call (audit finding #25).
        self.readable_scratch.clear();
        self.readable_scratch.extend(self.quiche_conn.readable());

        // Borrow-checker dance: take the streams out so the loop body can
        // freely call `&mut self` methods (stream_recv etc.). We restore
        // the empty Vec at the end so the allocation is reused next call.
        let mut readable = std::mem::take(&mut self.readable_scratch);
        for stream_id in readable.drain(..) {
            if app_events_remaining == 0 {
                break;
            }
            let is_new = self.known_streams.insert(stream_id);

            if is_new {
                // Coalesce first recv into NEW_STREAM event.
                // Checkout a pool buffer as the direct recv target — quiche copies
                // into it, then we truncate and move ownership to the event (no
                // intermediate copy).
                let (mut buf, _) = self.data_pool.checkout_recv(Self::RECV_CHUNK);
                match buf.recv_quic_stream(&mut self.quiche_conn, stream_id) {
                    Ok(fin) => {
                        let data = buf.into_initialized_vec();
                        events.push(JsH3Event::new_stream_with_data(
                            conn_handle,
                            stream_id,
                            data,
                            fin,
                            self.event_recycler(),
                        ));
                        app_events_remaining = app_events_remaining.saturating_sub(1);
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
                        self.data_pool.checkin(buf.into_recycle_vec());
                        events.push(JsH3Event::new_stream(conn_handle, stream_id));
                        app_events_remaining = app_events_remaining.saturating_sub(1);
                        continue;
                    }
                    Err(e) => {
                        self.data_pool.checkin(buf.into_recycle_vec());
                        events.push(JsH3Event::new_stream(conn_handle, stream_id));
                        app_events_remaining = app_events_remaining.saturating_sub(1);
                        if app_events_remaining == 0 {
                            self.known_streams.remove(&stream_id);
                            continue;
                        }
                        events.push(JsH3Event::error(
                            conn_handle,
                            stream_id as i64,
                            0,
                            e.to_string(),
                        ));
                        app_events_remaining = app_events_remaining.saturating_sub(1);
                        self.known_streams.remove(&stream_id);
                        continue;
                    }
                }
            }

            loop {
                if app_events_remaining == 0 {
                    break;
                }
                let (mut buf, _) = self.data_pool.checkout_recv(Self::RECV_CHUNK);
                match buf.recv_quic_stream(&mut self.quiche_conn, stream_id) {
                    Ok(fin) => {
                        let len = buf.initialized_len();
                        let delivered_data = len > 0;
                        if len > 0 {
                            events.push(JsH3Event::data(
                                conn_handle,
                                stream_id,
                                buf.into_initialized_vec(),
                                fin,
                                self.event_recycler(),
                            ));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
                        } else {
                            self.data_pool.checkin(buf.into_recycle_vec());
                        }
                        if fin {
                            reactor_metrics::record_raw_quic_fin_observed();
                            if delivered_data {
                                reactor_metrics::record_raw_quic_finished_event();
                                self.known_streams.remove(&stream_id);
                                self.quiche_conn
                                    .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                                    .ok();
                            } else if app_events_remaining > 0 {
                                reactor_metrics::record_raw_quic_finished_event();
                                events.push(JsH3Event::finished(conn_handle, stream_id));
                                app_events_remaining = app_events_remaining.saturating_sub(1);
                                self.known_streams.remove(&stream_id);
                                self.quiche_conn
                                    .stream_shutdown(stream_id, quiche::Shutdown::Read, 0)
                                    .ok();
                            } else {
                                self.pending_fin.insert(stream_id);
                            }
                            break;
                        }
                        if len == 0 || fin {
                            break;
                        }
                    }
                    Err(quiche::Error::Done) => {
                        self.data_pool.checkin(buf.into_recycle_vec());
                        if self.quiche_conn.stream_finished(stream_id) && app_events_remaining > 0 {
                            reactor_metrics::record_raw_quic_fin_observed();
                            reactor_metrics::record_raw_quic_finished_event();
                            events.push(JsH3Event::finished(conn_handle, stream_id));
                            app_events_remaining = app_events_remaining.saturating_sub(1);
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
                        self.data_pool.checkin(buf.into_recycle_vec());
                        events.push(JsH3Event::error(
                            conn_handle,
                            stream_id as i64,
                            0,
                            e.to_string(),
                        ));
                        app_events_remaining = app_events_remaining.saturating_sub(1);
                        self.known_streams.remove(&stream_id);
                        break;
                    }
                }
            }
        }
        // Restore the empty (but capacity-preserving) Vec for next call.
        self.readable_scratch = readable;

        // NOTE: sweep_finished_streams is called by the handler at lower frequency
        // (timer ticks) to avoid O(pending_fin) on every packet.
        if app_events_remaining > 0 {
            self.poll_datagram_events(conn_handle, app_events_remaining, events);
        }
        self.poll_drain_events(conn_handle, events);
    }

    /// Check only the `pending_fin` set for streams where quiche received FIN
    /// after we already drained all data via `stream_recv`. This is O(pending)
    /// instead of the previous O(all_known_streams) — typically near zero.
    pub fn sweep_finished_streams(
        &mut self,
        conn_handle: u32,
        app_event_budget: usize,
        events: &mut Vec<JsH3Event>,
    ) {
        if self.pending_fin.is_empty() || app_event_budget == 0 {
            return;
        }
        let mut remaining = app_event_budget;
        let mut newly_finished = Vec::new();
        for &stream_id in &self.pending_fin {
            if remaining == 0 {
                break;
            }
            if self.quiche_conn.stream_finished(stream_id)
                && !self.quiche_conn.stream_readable(stream_id)
            {
                newly_finished.push(stream_id);
                remaining = remaining.saturating_sub(1);
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
        let outcome = self.stream_send_arcbuf(stream_id, ArcBuf::from_vec(data), fin)?;
        let remainder = outcome.remainder.map(|r| r.as_ref().to_vec());
        Ok((outcome.written, remainder))
    }

    pub fn stream_send_chunk(
        &mut self,
        stream_id: u64,
        chunk: Chunk,
        fin: bool,
    ) -> Result<StreamSendOutcome, Http3NativeError> {
        self.stream_send_arcbuf(stream_id, ArcBuf::from_chunk(chunk), fin)
    }

    pub fn stream_send_arcbuf(
        &mut self,
        stream_id: u64,
        buf: ArcBuf,
        fin: bool,
    ) -> Result<StreamSendOutcome, Http3NativeError> {
        let data_len = buf.remaining_len();
        if data_len == 0 {
            if !fin {
                return Ok(StreamSendOutcome {
                    written: 0,
                    fin_accepted: false,
                    remainder: None,
                });
            }

            return match self.quiche_conn.stream_send(stream_id, &[], true) {
                Ok(written) => {
                    if self.is_local_stream(stream_id) {
                        self.known_streams.insert(stream_id);
                    }
                    Ok(StreamSendOutcome {
                        written: if written == 0 { 1 } else { written },
                        fin_accepted: true,
                        remainder: None,
                    })
                }
                Err(quiche::Error::Done) => {
                    if self.blocked_set.insert(stream_id) {
                        self.blocked_queue.push_back(stream_id);
                        reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                    }
                    Ok(StreamSendOutcome {
                        written: 0,
                        fin_accepted: false,
                        remainder: Some(buf),
                    })
                }
                Err(e) => Err(Http3NativeError::Quiche(e)),
            };
        }

        let original = buf.clone();
        match self.quiche_conn.stream_send_zc(stream_id, buf, fin) {
            Ok((written, remaining)) => {
                if written < data_len && self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
                if self.is_local_stream(stream_id) {
                    self.known_streams.insert(stream_id);
                }
                Ok(StreamSendOutcome {
                    written,
                    fin_accepted: fin && remaining.is_none(),
                    remainder: remaining,
                })
            }
            Err(quiche::Error::Done) => {
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
                Ok(StreamSendOutcome {
                    written: 0,
                    fin_accepted: false,
                    remainder: Some(original),
                })
            }
            Err(e) => Err(Http3NativeError::Quiche(e)),
        }
    }

    pub fn reserve_local_bidi_stream(&mut self, stream_id: u64) -> Result<(), Http3NativeError> {
        if !self.is_local_stream(stream_id) || (stream_id & 0x2) != 0 {
            return Err(Http3NativeError::InvalidState(format!(
                "stream {stream_id} is not a local bidirectional stream"
            )));
        }

        match self.quiche_conn.stream_send(stream_id, &[], false) {
            Ok(0) => {
                self.known_streams.insert(stream_id);
                Ok(())
            }
            Ok(written) => Err(Http3NativeError::InvalidState(format!(
                "empty stream reservation wrote {written} bytes"
            ))),
            Err(e) => Err(Http3NativeError::Quiche(e)),
        }
    }

    /// Borrow-based stream send: writes data without taking ownership.
    /// The caller retains the data and can track partial writes via the
    /// returned byte count. Uses quiche's non-zero-copy path.
    pub fn stream_send_borrowed(
        &mut self,
        stream_id: u64,
        data: &[u8],
        fin: bool,
    ) -> Result<usize, Http3NativeError> {
        match self.quiche_conn.stream_send(stream_id, data, fin) {
            Ok(written) => {
                if written < data.len() && self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
                if self.is_local_stream(stream_id) {
                    self.known_streams.insert(stream_id);
                }
                // Sentinel for FIN-only sends.
                if data.is_empty() && fin && written == 0 {
                    Ok(1)
                } else {
                    Ok(written)
                }
            }
            Err(quiche::Error::Done) => {
                if self.blocked_set.insert(stream_id) {
                    self.blocked_queue.push_back(stream_id);
                    reactor_metrics::record_raw_quic_blocked_streams(self.blocked_queue.len());
                }
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

    pub fn send_datagram(&mut self, data: &[u8]) -> Result<(), Http3NativeError> {
        self.quiche_conn
            .dgram_send(data)
            .map(|_| ())
            .map_err(Http3NativeError::Quiche)
    }

    pub fn queue_datagram(&mut self, data: Chunk) -> Result<bool, Http3NativeError> {
        let data = ArcBuf::from_chunk(data);

        if self.quiche_conn.is_dgram_send_queue_full() {
            if self.outbound_datagrams.len() >= MAX_OUTBOUND_DATAGRAM_QUEUE {
                return Ok(false);
            }
            self.outbound_datagrams.push_back(data);
            return Ok(true);
        }

        self.quiche_conn
            .dgram_send_buf(data)
            .map(|_| true)
            .map_err(Http3NativeError::Quiche)
    }

    pub fn queue_datagram_vec(&mut self, data: Vec<u8>) -> Result<bool, Http3NativeError> {
        self.queue_datagram(Chunk::unpooled(data))
    }

    pub fn drain_outbound_datagrams(&mut self) {
        loop {
            if self.quiche_conn.is_dgram_send_queue_full() {
                break;
            }

            let Some(data) = self.outbound_datagrams.pop_front() else {
                break;
            };

            if let Err(e) = self.quiche_conn.dgram_send_buf(data) {
                log::debug!("dropping queued QUIC datagram after send failure: {e}");
                break;
            }
        }
    }

    pub fn outbound_datagram_queue_len(&self) -> usize {
        self.outbound_datagrams.len()
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

#[cfg(feature = "qlog-files")]
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

/// No-op stand-in when `quiche/qlog` isn't compiled in (N5 in
/// `docs/WASM_CLIENT_PLAN.md`: qlog is excluded from the wasm build).
#[cfg(not(feature = "qlog-files"))]
fn maybe_enable_qlog(
    _quiche_conn: &mut quiche::Connection<ArcBufFactory>,
    _conn_id: &[u8],
    _role: &str,
    _qlog_dir: Option<&str>,
    _qlog_level: Option<&str>,
) -> Option<String> {
    None
}
