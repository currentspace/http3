//! `hs_*` extern "C" ABI — HTTP/3 server, see the crate-level doc comment
//! for the shared conventions (handles, error codes, buffer lifetime) and
//! the "client vs. server shape" note.
//!
//! One `hs_new` handle is the whole bound server (its `ConnectionMap` +
//! config + `TimerHeap`) — JS owns the actual UDP socket (bind +
//! recvfrom loop) entirely; there is no separate "connection" handle
//! type here. Connections are identified by the `connHandle: u32` field
//! already present on every `JsH3Event` (exactly like the native server
//! already multiplexes many connections this way).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use http3::wasm_exports::{
    Chunk, EVENT_ERROR, EVENT_STREAM_BLOCKED, H3ServerHandler, Http3Config, JsH3Event,
    OutboundAdmission, TxDatagram,
};

use crate::abi::{
    ERR_AGAIN, ERR_BAD_ARGS, ERR_INVALID_HANDLE, ERR_PROTOCOL, RX_TX_BUFFER_LEN, bytes_in,
    set_global_error, str_in, write_out_message, write_out_ptr_len,
};
use crate::events::serialize_events;
use crate::handle::Slots;
use crate::json_opts::{build_h3_server_options, parse_server_params};

struct H3ServerSession {
    handler: H3ServerHandler,
    /// Fixed at `hs_new` time — JS binds exactly one socket per server
    /// instance and passes every inbound datagram's origin explicitly to
    /// `hs_recv`, but the server's own address never changes.
    local_addr: SocketAddr,
    rx_buffer: Box<[u8]>,
    tx_buffer: Box<[u8]>,
    /// Packets ready to hand out via `hs_next_send`, one at a time.
    /// Refilled from `H3ServerHandler::flush_all_sends` (a whole-server,
    /// round-robin-across-every-connection drain) whenever this empties —
    /// `flush_all_sends` already exhausts everything available in one
    /// call, so refilling once per empty-queue check is sufficient; the
    /// per-packet `hs_next_send`/`hs_next_send_dest` pair mirrors the
    /// client's identical "loop until 0" convention. Also holds any
    /// Retry/Version-Negotiation packet a fresh Initial produced during
    /// `hs_recv` (the one case a server response bypasses a
    /// per-connection `send()` call entirely).
    pending_tx: VecDeque<TxDatagram>,
    /// Destination of the packet most recently written into `tx_buffer`
    /// by `hs_next_send`, read by `hs_next_send_dest`.
    last_send_dest: Option<SocketAddr>,
    pending_events: Vec<JsH3Event>,
    json_scratch: Vec<u8>,
    data_scratch: Vec<u8>,
    dest_scratch: Vec<u8>,
    last_error: Option<String>,
}

thread_local! {
    static SESSIONS: RefCell<Slots<H3ServerSession>> = const { RefCell::new(Slots::new()) };
}

fn with_session_mut<R>(handle: u32, f: impl FnOnce(&mut H3ServerSession) -> R) -> Option<R> {
    SESSIONS.with(|s| s.borrow_mut().get_mut(handle).map(f))
}

fn set_session_error(handle: u32, msg: String) {
    with_session_mut(handle, |sess| sess.last_error = Some(msg));
}

/// `opts` (normative fields — camelCase, matching `lib/server.ts`'s
/// `ServerOptions` / `lib/quic-server.ts`'s `QuicServerOptions` exactly):
/// `key`/`cert` (PEM strings, **mandatory** — a server cannot start
/// without them), `ca` (PEM string, optional client-cert verification),
/// `clientAuth` (`'none'|'request'|'require'`), `maxIdleTimeoutMs`,
/// `maxUdpPayloadSize`, `initialMaxData`, `initialMaxStreamDataBidiLocal`,
/// `initialMaxStreamsBidi`, `disableActiveMigration`, `enableDatagrams`,
/// `qpackMaxTableCapacity`, `qpackBlockedStreams`, `disableRetry`,
/// `maxConnections`, `quicLb`, `serverId` (hex string), `keylog`
/// (boolean only — see `build_h3_server_options`'s doc comment), plus the
/// ABI-only `localAddr` (`"ip:port"`/`"[v6]:port"` — the server's own
/// bound address) and `retryTokenKeyHex` (64 lowercase hex chars = 32
/// bytes, JS-supplied via `crypto.getRandomValues`, mirroring the
/// client's `scidHex` convention exactly).
///
/// # Safety
/// `opts_ptr`/`opts_len` must describe a valid, readable UTF-8 JSON byte
/// range in this module's own linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn hs_new(opts_ptr: u32, opts_len: u32) -> u32 {
    let bytes = unsafe { bytes_in(opts_ptr, opts_len) };
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            set_global_error(format!("[h3:config] invalid options JSON: {e}"));
            return 0;
        }
    };

    let params = match parse_server_params(&value) {
        Ok(p) => p,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let opts = match build_h3_server_options(&value) {
        Ok(o) => o,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let quiche_config = match Http3Config::new_server_quiche_config_in_memory(&opts) {
        Ok(c) => c,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };
    let http3_config = match Http3Config::from_server_options(&opts) {
        Ok(c) => c,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };

    let handler = H3ServerHandler::new_direct(
        quiche_config,
        http3_config,
        params.retry_token_key,
        Arc::new(OutboundAdmission::default()),
    );

    SESSIONS.with(|s| {
        s.borrow_mut().insert(H3ServerSession {
            handler,
            local_addr: params.local_addr,
            rx_buffer: vec![0u8; RX_TX_BUFFER_LEN].into_boxed_slice(),
            tx_buffer: vec![0u8; RX_TX_BUFFER_LEN].into_boxed_slice(),
            pending_tx: VecDeque::new(),
            last_send_dest: None,
            pending_events: Vec::new(),
            json_scratch: Vec::new(),
            data_scratch: Vec::new(),
            dest_scratch: Vec::new(),
            last_error: None,
        })
    })
}

/// `handle = 0` reads the global last-error slot (construction failures).
#[unsafe(no_mangle)]
pub extern "C" fn hs_last_error(handle: u32, buf_ptr: u32, cap: u32) -> i32 {
    let msg = if handle == 0 {
        crate::abi::take_global_error_for_read()
    } else {
        SESSIONS.with(|s| s.borrow().get(handle).and_then(|sess| sess.last_error.clone()))
    };
    write_out_message(msg, buf_ptr, cap)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_rx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.rx_buffer.as_ptr() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_tx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.tx_buffer.as_ptr() as u32).unwrap_or(0)
}

/// JS has already copied an inbound datagram into
/// `hs_rx_buffer()[..len]`, received from `peer_addr` (a UTF-8
/// `"ip:port"`/`"[v6]:port"` string — the server's own local address was
/// fixed at `hs_new` time).
///
/// # Safety
/// `peer_addr_ptr`/`peer_addr_len` must describe a valid, readable UTF-8
/// byte range.
#[unsafe(no_mangle)]
pub extern "C" fn hs_recv(handle: u32, len: u32, peer_addr_ptr: u32, peer_addr_len: u32) -> i64 {
    let Some(peer_addr_str) = (unsafe { str_in(peer_addr_ptr, peer_addr_len) }) else {
        set_session_error(handle, "[h3:config] peer address is not valid UTF-8".to_string());
        return ERR_BAD_ARGS;
    };
    let Ok(peer) = peer_addr_str.parse::<SocketAddr>() else {
        set_session_error(
            handle,
            format!("[h3:config] invalid peer address '{peer_addr_str}'"),
        );
        return ERR_BAD_ARGS;
    };
    with_session_mut(handle, |sess| {
        let len = (len as usize).min(sess.rx_buffer.len());
        let local = sess.local_addr;
        let mut pending_outbound: Vec<TxDatagram> = Vec::new();
        sess.handler.process_inbound_packet(
            &mut sess.rx_buffer[..len],
            peer,
            local,
            &mut pending_outbound,
            usize::MAX,
            &mut sess.pending_events,
        );
        sess.pending_tx.extend(pending_outbound);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Writes the next outbound datagram into `hs_tx_buffer()`; loop until 0.
/// Call `hs_next_send_dest` immediately after each non-zero return (before
/// the next `hs_next_send` call) to learn which peer it's for.
#[unsafe(no_mangle)]
pub extern "C" fn hs_next_send(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        if sess.pending_tx.is_empty() {
            let mut refill = Vec::new();
            sess.handler.flush_all_sends(&mut refill);
            sess.pending_tx.extend(refill);
        }
        match sess.pending_tx.pop_front() {
            Some(tx) => {
                let n = tx.payload_len().min(sess.tx_buffer.len());
                sess.tx_buffer[..n].copy_from_slice(&tx.payload()[..n]);
                sess.last_send_dest = Some(tx.to);
                let recycle = tx.into_recycle_buffer();
                sess.handler.recycle_tx_buffers_into_pool(vec![recycle]);
                n as i64
            }
            None => {
                sess.last_send_dest = None;
                0
            }
        }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Destination (`"ip:port"`/`"[v6]:port"`) for the packet most recently
/// written by `hs_next_send`. `0` if `hs_next_send` last returned `0` (or
/// hasn't been called yet).
#[unsafe(no_mangle)]
pub extern "C" fn hs_next_send_dest(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let Some(dest) = sess.last_send_dest else {
            return 0i64;
        };
        sess.dest_scratch.clear();
        sess.dest_scratch.extend_from_slice(dest.to_string().as_bytes());
        write_out_ptr_len(&sess.dest_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// The single soonest deadline across *every* connection this server
/// holds (from the `TimerHeap`) — JS still only ever arms one timer per
/// server instance, exactly like the client pattern. `-1` = no timer
/// pending (also returned, harmlessly, for an invalid handle).
#[unsafe(no_mangle)]
pub extern "C" fn hs_timeout_ms(handle: u32) -> i64 {
    with_session_mut(handle, |sess| match sess.handler.soonest_deadline() {
        Some(deadline) => {
            let now = Instant::now();
            if deadline <= now {
                0
            } else {
                i64::try_from(deadline.duration_since(now).as_millis()).unwrap_or(i64::MAX)
            }
        }
        None => -1,
    })
    .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_on_timeout(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .expire_timers(Instant::now(), usize::MAX, &mut sess.pending_events);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Settles app events / drain events / retried pending writes / closed
/// connections (mirroring the native per-tick sweep order:
/// `poll_app_events` → `poll_drain_events` → `flush_pending_writes` →
/// `cleanup_closed`) and then serializes + drains the accumulated batch.
/// Safe to call as the one "did anything change?" checkpoint after any
/// `hs_recv`/`hs_on_timeout`/per-connection call — the settle calls are
/// cheap no-ops when there is nothing new to find. `connHandle` in each
/// event distinguishes which connection it belongs to; a brand-new
/// connection surfaces via `EVENT_NEW_SESSION`.
#[unsafe(no_mangle)]
pub extern "C" fn hs_drain_events(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .collect_app_events(usize::MAX, &mut sess.pending_events);
        sess.handler
            .collect_drain_events(usize::MAX, &mut sess.pending_events);
        sess.handler.flush_all_pending_writes(&mut sess.pending_events);
        // Server-only (no client equivalent): reap connections that
        // closed via a path other than the timer sweep above (e.g. the
        // peer's own CONNECTION_CLOSE arriving in `hs_recv`) so their
        // final `session_close` event isn't lost and the connection map
        // doesn't leak entries forever.
        sess.handler.reap_closed_connections(&mut sess.pending_events);

        let events = std::mem::take(&mut sess.pending_events);
        serialize_events(&events, &mut sess.json_scratch, &mut sess.data_scratch);
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Send response headers for `stream_id` on `conn_handle`. `status = 0`
/// on success; a negative code otherwise — a flow-control block's
/// fetched message (`hs_last_error`) contains the substring
/// `StreamBlocked` (same convention as `h3c_send_request`).
///
/// # Safety
/// `headers_json_ptr`/`len` must describe a valid, readable UTF-8 JSON
/// array-of-`{name,value}` byte range.
#[unsafe(no_mangle)]
pub extern "C" fn hs_send_response_headers(
    handle: u32,
    conn_handle: u32,
    stream_id: u64,
    headers_json_ptr: u32,
    headers_json_len: u32,
    fin: i32,
) -> i64 {
    let Some(headers) = parse_headers_json(headers_json_ptr, headers_json_len) else {
        set_session_error(handle, "[h3:config] invalid headers JSON".to_string());
        return ERR_BAD_ARGS;
    };
    with_session_mut(handle, |sess| {
        match sess.handler.send_response_headers(
            conn_handle,
            stream_id,
            headers,
            fin != 0,
            &mut sess.pending_events,
        ) {
            Ok(()) => 0i64,
            Err(msg) => {
                let blocked = msg.contains("StreamBlocked");
                sess.last_error = Some(format!("[h3:h3] {msg}"));
                if blocked { ERR_AGAIN } else { ERR_PROTOCOL }
            }
        }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `>=0` admitted bytes (FIN-only accept = 1), `-1` backpressure, `-2`
/// protocol error — same convention as `h3c_stream_send`.
///
/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn hs_stream_send(
    handle: u32,
    conn_handle: u32,
    stream_id: u64,
    ptr: u32,
    len: u32,
    fin: i32,
) -> i64 {
    let data = unsafe { bytes_in(ptr, len) }.to_vec();
    with_session_mut(handle, |sess| {
        let chunk = if data.is_empty() {
            Chunk::empty()
        } else {
            Chunk::unpooled(data)
        };
        let before = sess.pending_events.len();
        let released = sess.handler.queue_stream_send(
            conn_handle,
            stream_id,
            chunk,
            fin != 0,
            &mut sess.pending_events,
        );
        classify_send_outcome(&sess.pending_events[before..], released)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Send trailing headers for `stream_id` on `conn_handle` — the
/// `hs_send_trailers` primitive.
///
/// Deviation (server-side wasm support): the inherited `hs_*` ABI surface
/// omitted this export even though `H3ServerHandler::send_trailers`
/// (worker.rs) already exists and is a genuinely distinct H3 frame from
/// `hs_send_response_headers` — quiche's `send_additional_headers` with
/// `is_trailer_section=true`. Calling `hs_send_response_headers` a second
/// time is not equivalent: it would re-emit/silently drop the initial
/// response frame instead of sending trailers (see
/// `H3Connection::send_trailers`'s own doc comment in `src/connection.rs`).
/// `lib/event-loop.ts`'s `ServerEventLoopLike.sendTrailers` requires this
/// primitive, so it is added here as a minimal, additive export mirroring
/// `hs_send_response_headers`'s exact shape minus the `fin`/blocked-retry
/// handling (`H3ServerHandler::send_trailers` has no pending-write buffering
/// path — trailers always carry an implicit FIN and are not retried on
/// `StreamBlocked` the way headers-before-any-body are).
///
/// # Safety
/// `headers_json_ptr`/`len` must describe a valid, readable UTF-8 JSON
/// array-of-`{name,value}` byte range.
#[unsafe(no_mangle)]
pub extern "C" fn hs_send_trailers(
    handle: u32,
    conn_handle: u32,
    stream_id: u64,
    headers_json_ptr: u32,
    headers_json_len: u32,
) -> i64 {
    let Some(headers) = parse_headers_json(headers_json_ptr, headers_json_len) else {
        set_session_error(handle, "[h3:config] invalid headers JSON".to_string());
        return ERR_BAD_ARGS;
    };
    with_session_mut(handle, |sess| {
        match sess.handler.send_trailers(conn_handle, stream_id, &headers) {
            Ok(()) => 0i64,
            Err(msg) => {
                sess.last_error = Some(format!("[h3:h3] {msg}"));
                ERR_PROTOCOL
            }
        }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

fn classify_send_outcome(newly_pushed: &[JsH3Event], released_units: usize) -> i64 {
    let has_error = newly_pushed.iter().any(|e| e.event_type == EVENT_ERROR);
    let has_blocked = newly_pushed
        .iter()
        .any(|e| e.event_type == EVENT_STREAM_BLOCKED);
    if has_error {
        ERR_PROTOCOL
    } else if released_units == 0 {
        let _ = has_blocked;
        ERR_AGAIN
    } else {
        released_units as i64
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_stream_close(
    handle: u32,
    conn_handle: u32,
    stream_id: u64,
    error_code: u32,
) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .close_stream(conn_handle, stream_id, error_code, &mut sess.pending_events);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn hs_send_datagram(handle: u32, conn_handle: u32, ptr: u32, len: u32) -> i64 {
    let data = unsafe { bytes_in(ptr, len) }.to_vec();
    with_session_mut(handle, |sess| {
        if sess.handler.send_datagram(conn_handle, Chunk::unpooled(data)) {
            0i64
        } else {
            ERR_AGAIN
        }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_ping(handle: u32, conn_handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        if sess.handler.ping(conn_handle) { 0i64 } else { ERR_AGAIN }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `-3` (invalid handle) is reused here for "no such connection" too —
/// both mean "this locator doesn't refer to anything live" from the
/// caller's point of view.
#[unsafe(no_mangle)]
pub extern "C" fn hs_session_metrics(handle: u32, conn_handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| match sess.handler.session_metrics(conn_handle) {
        Some(metrics) => {
            let json = crate::events::metrics_only_json(&metrics);
            sess.json_scratch.clear();
            sess.json_scratch.extend_from_slice(json.as_bytes());
            write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
        }
        None => ERR_INVALID_HANDLE,
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_remote_settings(handle: u32, conn_handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let settings = sess.handler.remote_settings(conn_handle);
        let arr: Vec<serde_json::Value> = settings
            .iter()
            .map(|(id, value)| serde_json::json!({ "id": id, "value": value }))
            .collect();
        sess.json_scratch.clear();
        let _ = serde_json::to_writer(&mut sess.json_scratch, &serde_json::Value::Array(arr));
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Gracefully close a single connection (GOAWAY, then a deferred
/// CONNECTION_CLOSE once in-flight responses have a chance to land — see
/// `H3ServerHandler::close_connection`'s doc comment).
///
/// # Safety
/// `reason_ptr`/`reason_len` must describe a valid, readable UTF-8 byte
/// range (may be zero-length).
#[unsafe(no_mangle)]
pub extern "C" fn hs_close_connection(
    handle: u32,
    conn_handle: u32,
    code: u32,
    reason_ptr: u32,
    reason_len: u32,
) -> i64 {
    let reason = unsafe { str_in(reason_ptr, reason_len) }.unwrap_or("");
    with_session_mut(handle, |sess| {
        sess.handler.close_connection(conn_handle, code, reason.to_string());
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `true` if `conn_handle` is closed (or no longer tracked at all).
#[unsafe(no_mangle)]
pub extern "C" fn hs_connection_is_closed(handle: u32, conn_handle: u32) -> i32 {
    with_session_mut(handle, |sess| {
        i32::from(sess.handler.connection_is_closed(conn_handle))
    })
    .unwrap_or(1)
}

/// `true` once the server has no live connections and no pending
/// graceful closes.
#[unsafe(no_mangle)]
pub extern "C" fn hs_is_done(handle: u32) -> i32 {
    with_session_mut(handle, |sess| i32::from(sess.handler.is_idle())).unwrap_or(1)
}

/// Graceful shutdown of the whole server: closes every live connection
/// with an application-level CONNECTION_CLOSE (mirrors native's
/// `WorkerCommand::Shutdown` handling). Keep pumping
/// `hs_next_send`/`hs_on_timeout`/`hs_drain_events` afterward until
/// `hs_is_done` so the CLOSE frames actually flush and every connection's
/// final `session_close` event is observed.
#[unsafe(no_mangle)]
pub extern "C" fn hs_shutdown(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler.shutdown_all_connections();
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn hs_free(handle: u32) {
    SESSIONS.with(|s| {
        s.borrow_mut().remove(handle);
    });
}

/// Parses `[{"name": "...", "value": "..."}]` into the `(String, String)`
/// pairs `H3ServerHandler::send_response_headers` expects. Small,
/// per-file duplicate of `h3.rs`'s identical private helper — this
/// crate's existing convention (`classify_send_outcome` is likewise
/// duplicated verbatim between `h3.rs` and `quic.rs`) rather than a new
/// shared module for a ~10-line function.
fn parse_headers_json(ptr: u32, len: u32) -> Option<Vec<(String, String)>> {
    let bytes = unsafe { bytes_in(ptr, len) };
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let arr = value.as_array()?;
    arr.iter()
        .map(|h| {
            let name = h.get("name")?.as_str()?.to_string();
            let value = h.get("value")?.as_str()?.to_string();
            Some((name, value))
        })
        .collect()
}
