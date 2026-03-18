//! Shared event loop and protocol handler trait.
//!
//! `run_event_loop<D, P>()` is the single loop that drives all four worker
//! variants (H3 server/client, QUIC server/client).  Protocol-specific logic
//! lives in the [`ProtocolHandler`] implementations; platform I/O lives in the
//! [`Driver`](crate::transport::Driver) implementations.

use std::net::SocketAddr;
use std::io;
use std::time::Instant;

use crossbeam_channel::Receiver;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use crate::h3_event::JsH3Event;
use crate::transport::{Driver, TxDatagram};

/// TSFN type for delivering event batches to the JS main thread.
/// Uses default const generics: `CalleeHandled=true`, `Weak=false`, `MaxQueueSize=0` (unbounded).
pub type EventTsfn = ThreadsafeFunction<Vec<JsH3Event>>;

/// Max events per TSFN call. Sized for high-concurrency workloads (1000+ streams).
/// At 2048 events × ~50 bytes ≈ 100KB per batch — well within comfort.
/// Larger batches amortize TSFN (Rust→JS thread boundary) overhead: at 10K
/// streams, ~30K events per cycle ÷ 2048 ≈ 15 calls vs 60 at 512.
pub(crate) const MAX_BATCH_SIZE: usize = 2048;

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

pub(crate) struct EventBatcher {
    pub batch: Vec<JsH3Event>,
    tsfn: EventTsfn,
    events_dropped: u64,
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
    pub fn new(tsfn: EventTsfn) -> Self {
        Self {
            batch: Vec::with_capacity(MAX_BATCH_SIZE),
            tsfn,
            events_dropped: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.batch.len()
    }

    /// Flush events to JS.  Returns `false` if TSFN is closing → shut down.
    pub fn flush(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        let to_send = std::mem::take(&mut self.batch);
        let count = to_send.len();
        match self
            .tsfn
            .call(Ok(to_send), ThreadsafeFunctionCallMode::NonBlocking)
        {
            napi::Status::Ok => true,
            napi::Status::Closing => {
                self.events_dropped += count as u64;
                log::debug!("TSFN closing, dropped {count} events");
                false
            }
            status => {
                self.events_dropped += count as u64;
                log::warn!(
                    "TSFN call failed ({status:?}), dropped {count} events (total dropped: {})",
                    self.events_dropped
                );
                true
            }
        }
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
    tsfn: EventTsfn,
    local_addr: SocketAddr,
) {
    let mut batcher = EventBatcher::new(tsfn);
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
        for (rx_idx, mut pkt) in outcome.rx.into_iter().enumerate() {
            pending_outbound.clear();
            handler.process_packet(
                &mut pkt.data,
                pkt.peer,
                local_addr,
                &mut pending_outbound,
                &mut batcher.batch,
            );
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
