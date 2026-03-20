//! Shared event loop and protocol handler trait.
//!
//! `run_event_loop<D, P>()` is the single loop that drives all four worker
//! variants (H3 server/client, QUIC server/client).  Protocol-specific logic
//! lives in the [`ProtocolHandler`] implementations; platform I/O lives in the
//! [`Driver`](crate::transport::Driver) implementations.

use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Instant;

use crossbeam_channel::Receiver;
use serde::Serialize;

#[cfg(feature = "node-api")]
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use crate::h3_event::JsH3Event;
use crate::transport::{Driver, TxDatagram};

/// TSFN type for delivering event batches to the JS main thread.
/// Uses default const generics: `CalleeHandled=true`, `Weak=false`, `MaxQueueSize=0` (unbounded).
#[cfg(feature = "node-api")]
pub type EventTsfn = ThreadsafeFunction<Vec<JsH3Event>>;

#[derive(Clone, Debug, Serialize)]
pub struct EventBatcherStatsSnapshot {
    pub sink_kind: &'static str,
    pub flush_count: u64,
    pub attempted_events: u64,
    pub delivered_events: u64,
    pub dropped_events: u64,
    pub sink_errors: u64,
    pub max_batch_size: usize,
}

#[derive(Default)]
struct EventBatcherStatsInner {
    flush_count: AtomicU64,
    attempted_events: AtomicU64,
    dropped_events: AtomicU64,
    sink_errors: AtomicU64,
    max_batch_size: AtomicUsize,
}

#[derive(Clone)]
pub struct EventBatcherStatsHandle {
    sink_kind: &'static str,
    inner: Arc<EventBatcherStatsInner>,
}

impl EventBatcherStatsHandle {
    fn new(sink_kind: &'static str) -> Self {
        Self {
            sink_kind,
            inner: Arc::new(EventBatcherStatsInner::default()),
        }
    }

    fn record_flush(&self, count: usize) {
        self.inner.flush_count.fetch_add(1, Ordering::Relaxed);
        self.inner
            .attempted_events
            .fetch_add(count as u64, Ordering::Relaxed);
        self.inner
            .max_batch_size
            .fetch_max(count, Ordering::Relaxed);
    }

    pub(crate) fn record_drop(&self, count: usize) {
        self.inner
            .dropped_events
            .fetch_add(count as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_sink_error(&self) {
        self.inner.sink_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> EventBatcherStatsSnapshot {
        let attempted_events = self.inner.attempted_events.load(Ordering::Relaxed);
        let dropped_events = self.inner.dropped_events.load(Ordering::Relaxed);
        EventBatcherStatsSnapshot {
            sink_kind: self.sink_kind,
            flush_count: self.inner.flush_count.load(Ordering::Relaxed),
            attempted_events,
            delivered_events: attempted_events.saturating_sub(dropped_events),
            dropped_events,
            sink_errors: self.inner.sink_errors.load(Ordering::Relaxed),
            max_batch_size: self.inner.max_batch_size.load(Ordering::Relaxed),
        }
    }
}

/// Max events per TSFN call. Sized for high-concurrency workloads (1000+ streams).
/// At 2048 events × ~50 bytes ≈ 100KB per batch — well within comfort.
/// Larger batches amortize TSFN (Rust→JS thread boundary) overhead: at 10K
/// streams, ~30K events per cycle ÷ 2048 ≈ 15 calls vs 60 at 512.
pub const MAX_BATCH_SIZE: usize = 2048;

/// Per-connection QUIC packet scratch buffer size.
pub(crate) const SEND_BUF_SIZE: usize = 65535;

// ── Protocol handler trait ──────────────────────────────────────────

/// Protocol-specific logic invoked by [`run_event_loop`].
///
/// Four implementations exist:
/// - `H3ServerHandler` — HTTP/3 server (multi-connection)
/// - `H3ClientHandler` — HTTP/3 client (single connection)
/// - `QuicServerHandler` — raw QUIC server (multi-connection)
/// - `QuicClientHandler` — raw QUIC client (single connection)
pub(crate) trait ProtocolHandler {
    type Command: Send + 'static;

    /// Process one command from the JS thread.  Returns `true` → shut down loop.
    fn dispatch_command(&mut self, cmd: Self::Command, batch: &mut Vec<JsH3Event>) -> bool;

    /// Parse QUIC header, route to connection, `conn.recv()`, poll protocol events.
    /// Pushes retry / version-negotiation packets to `pending_outbound`.
    fn process_packet(
        &mut self,
        buf: &mut [u8],
        peer: SocketAddr,
        local: SocketAddr,
        pending_outbound: &mut Vec<TxDatagram>,
        batch: &mut Vec<JsH3Event>,
    );

    /// Call `on_timeout()` for expired timers, poll events, reschedule.
    fn process_timers(&mut self, now: Instant, batch: &mut Vec<JsH3Event>);

    /// Call `conn.send()` for every connection.  Pushes outbound packets.
    fn flush_sends(&mut self, outbound: &mut Vec<TxDatagram>);

    /// Retry pending writes where flow control has opened.  Push drain events.
    fn flush_pending_writes(&mut self, batch: &mut Vec<JsH3Event>);

    /// Check blocked_streams for writability.  Push drain events.
    fn poll_drain_events(&mut self, batch: &mut Vec<JsH3Event>);

    /// Remove closed connections.  Push session_close events.
    fn cleanup_closed(&mut self, batch: &mut Vec<JsH3Event>);

    /// Soonest quiche timeout, or `None`.
    fn next_deadline(&mut self) -> Option<Instant>;

    /// Recycle TX buffers back into the handler's pool.
    fn recycle_tx_buffers(&mut self, _buffers: Vec<Vec<u8>>) {}

    /// Returns `true` when the handler's work is done and the loop should exit
    /// once all pending TX is flushed (used by client handlers after session close).
    fn is_done(&self) -> bool {
        false
    }
}

// ── Event batcher ───────────────────────────────────────────────────

pub trait EventSink: Send {
    fn kind(&self) -> &'static str;
    fn emit(&mut self, events: Vec<JsH3Event>, stats: &EventBatcherStatsHandle) -> bool;
}

#[cfg(feature = "node-api")]
pub(crate) struct TsfnEventSink {
    tsfn: EventTsfn,
}

#[cfg(feature = "node-api")]
impl TsfnEventSink {
    pub(crate) fn new(tsfn: EventTsfn) -> Self {
        Self { tsfn }
    }
}

#[cfg(feature = "node-api")]
impl EventSink for TsfnEventSink {
    fn kind(&self) -> &'static str {
        "tsfn"
    }

    fn emit(&mut self, events: Vec<JsH3Event>, stats: &EventBatcherStatsHandle) -> bool {
        let count = events.len();
        match self
            .tsfn
            .call(Ok(events), ThreadsafeFunctionCallMode::NonBlocking)
        {
            napi::Status::Ok => true,
            napi::Status::Closing => {
                stats.record_drop(count);
                log::debug!("TSFN closing, dropped {count} events");
                false
            }
            status => {
                stats.record_sink_error();
                stats.record_drop(count);
                log::warn!(
                    "TSFN call failed ({status:?}), dropped {count} events (total dropped: {})",
                    stats.snapshot().dropped_events
                );
                true
            }
        }
    }
}

pub struct EventBatcher {
    pub batch: Vec<JsH3Event>,
    sink: Box<dyn EventSink>,
    stats: EventBatcherStatsHandle,
}

fn flush_runtime_error<D: Driver>(
    batcher: &mut EventBatcher,
    driver: &D,
    syscall: &str,
    reason_code: &str,
    err: &io::Error,
) -> bool {
    batcher.batch.push(JsH3Event::runtime_error(
        0,
        driver.driver_kind().as_str(),
        syscall,
        reason_code,
        err,
    ));
    batcher.flush()
}

impl EventBatcher {
    #[cfg(feature = "node-api")]
    pub fn new_tsfn(tsfn: EventTsfn) -> Self {
        Self::with_sink(TsfnEventSink::new(tsfn))
    }

    pub fn with_sink<S: EventSink + 'static>(sink: S) -> Self {
        let stats = EventBatcherStatsHandle::new(sink.kind());
        Self {
            batch: Vec::with_capacity(MAX_BATCH_SIZE),
            sink: Box::new(sink),
            stats,
        }
    }

    pub fn len(&self) -> usize {
        self.batch.len()
    }

    pub fn stats_handle(&self) -> EventBatcherStatsHandle {
        self.stats.clone()
    }

    /// Flush events to the configured sink. Returns `false` when the sink asks
    /// the worker loop to stop.
    pub fn flush(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        let to_send = std::mem::take(&mut self.batch);
        self.stats.record_flush(to_send.len());
        self.sink.emit(to_send, &self.stats)
    }
}

// ── Shared event loop ───────────────────────────────────────────────

/// The single event loop that drives all four worker variants.
///
/// Blocking.  Runs on the dedicated worker thread until shutdown or TSFN close.
pub(crate) fn run_event_loop<D: Driver, P: ProtocolHandler>(
    driver: &mut D,
    cmd_rx: Receiver<P::Command>,
    handler: &mut P,
    mut batcher: EventBatcher,
    local_addr: SocketAddr,
) {
    // Use pkt.local (from IP_PKTINFO) only when the socket is bound to a
    // specific address.  When bound to a wildcard (0.0.0.0 / [::]), quiche
    // creates connections with that wildcard as the local addr.  Passing a
    // more-specific pktinfo addr (e.g. 127.0.0.1) causes a mismatch and
    // quiche rejects the packet.  This flag gates pkt.local usage.
    let use_pktinfo_local = !local_addr.ip().is_unspecified();
    let mut outbound: Vec<TxDatagram> = Vec::new();
    let mut pending_outbound: Vec<TxDatagram> = Vec::new();

    // Initial flush — sends Client Hello for client handlers, no-op for servers.
    handler.flush_sends(&mut outbound);
    if !outbound.is_empty() {
        if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
            let _ = flush_runtime_error(
                &mut batcher,
                driver,
                "submit_sends",
                "driver-submit-sends-failed",
                &err,
            );
            return;
        }
    }

    loop {
        // 1. Compute deadline from protocol timers
        let deadline = handler.next_deadline();

        // 2. Block until events occur
        let outcome = match driver.poll(deadline) {
            Ok(o) => o,
            Err(err) => {
                let _ = flush_runtime_error(
                    &mut batcher,
                    driver,
                    "poll",
                    "driver-poll-failed",
                    &err,
                );
                return;
            }
        };

        // 3. Drain command channel (unconditional — waker just makes poll return early)
        while let Ok(cmd) = cmd_rx.try_recv() {
            if handler.dispatch_command(cmd, &mut batcher.batch) {
                // Shutdown requested: flush remaining sends before exiting
                handler.flush_sends(&mut outbound);
                if !outbound.is_empty() {
                    if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                        let _ = flush_runtime_error(
                            &mut batcher,
                            driver,
                            "submit_sends",
                            "driver-submit-sends-failed",
                            &err,
                        );
                    }
                }
                return;
            }
        }

        // 3b. Flush sends after commands (response data goes out immediately)
        handler.flush_sends(&mut outbound);
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                let _ = flush_runtime_error(
                    &mut batcher,
                    driver,
                    "submit_sends",
                    "driver-submit-sends-failed",
                    &err,
                );
                return;
            }
        }

        // 4. Process completed RX datagrams.
        //    Every 64 packets, flush outbound sends so ACKs and echo data reach
        //    peers promptly.  Without this, processing hundreds of inbound
        //    packets before any sends causes congestion-window stalls under
        //    fan-out (many connections sending concurrently).
        let rx_count = outcome.rx.len();
        let mut rx_recycled: Vec<Vec<u8>> = Vec::new();
        for (rx_idx, mut pkt) in outcome.rx.into_iter().enumerate() {
            pending_outbound.clear();
            if rx_idx == 0 {
                log::trace!(
                    "event_loop: processing {rx_count} rx pkts, first: len={} peer={} local={} gro={:?} tid={:?}",
                    pkt.data.len(), pkt.peer, pkt.local, pkt.segment_size, std::thread::current().id(),
                );
            }
            let pkt_local = if use_pktinfo_local { pkt.local } else { local_addr };
            // GRO: kernel may coalesce multiple datagrams into one buffer.
            // Split by segment_size and call process_packet for each.
            if let Some(seg_size) = pkt.segment_size {
                let seg = seg_size as usize;
                for chunk in pkt.data.chunks(seg) {
                    let mut buf = chunk.to_vec();
                    handler.process_packet(
                        &mut buf,
                        pkt.peer,
                        pkt_local,
                        &mut pending_outbound,
                        &mut batcher.batch,
                    );
                }
            } else {
                handler.process_packet(
                    &mut pkt.data,
                    pkt.peer,
                    pkt_local,
                    &mut pending_outbound,
                    &mut batcher.batch,
                );
            }
            rx_recycled.push(pkt.data);
            // Submit retry / version-negotiation packets immediately
            if !pending_outbound.is_empty() {
                if let Err(err) = driver.submit_sends(std::mem::take(&mut pending_outbound)) {
                    let _ = flush_runtime_error(
                        &mut batcher,
                        driver,
                        "submit_sends",
                        "driver-submit-sends-failed",
                        &err,
                    );
                    return;
                }
            }
            // Mid-RX flush: send accumulated outbound every 64 packets
            if (rx_idx + 1) % 64 == 0 && rx_idx + 1 < rx_count {
                handler.flush_sends(&mut outbound);
                if !outbound.is_empty() {
                    if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                        let _ = flush_runtime_error(
                            &mut batcher,
                            driver,
                            "submit_sends",
                            "driver-submit-sends-failed",
                            &err,
                        );
                        return;
                    }
                }
            }
            // Mid-batch flush if needed
            if batcher.len() >= MAX_BATCH_SIZE && !batcher.flush() {
                return;
            }
        }
        if !rx_recycled.is_empty() {
            driver.recycle_rx_buffers(rx_recycled);
        }

        // 5. Process protocol timers (always — cheap when nothing is expired)
        handler.process_timers(Instant::now(), &mut batcher.batch);

        // 6. Poll drain events + flush pending writes
        handler.poll_drain_events(&mut batcher.batch);
        handler.flush_pending_writes(&mut batcher.batch);

        // Mid-batch flush if timer/drain processing pushed us over the cap
        if batcher.len() >= MAX_BATCH_SIZE && !batcher.flush() {
            return;
        }

        // 7. Flush outbound from all connections
        handler.flush_sends(&mut outbound);
        if !outbound.is_empty() {
            if let Err(err) = driver.submit_sends(std::mem::take(&mut outbound)) {
                let _ = flush_runtime_error(
                    &mut batcher,
                    driver,
                    "submit_sends",
                    "driver-submit-sends-failed",
                    &err,
                );
                return;
            }
        }

        // 7b. Recycle TX buffers accumulated during this iteration
        let recycled = driver.drain_recycled_tx();
        if !recycled.is_empty() {
            handler.recycle_tx_buffers(recycled);
        }

        // 8. Cleanup closed connections
        handler.cleanup_closed(&mut batcher.batch);

        // 9. Flush events to JS
        if !batcher.flush() {
            return;
        }

        // 10. Client exit: handler done and all packets drained
        if handler.is_done() && driver.pending_tx_count() == 0 {
            return;
        }
    }
}
