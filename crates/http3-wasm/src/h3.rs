//! `h3c_*` extern "C" ABI — HTTP/3 client, see the crate-level doc comment
//! for the shared conventions (handles, error codes, buffer lifetime).

use std::cell::RefCell;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use http3::wasm_exports::{
    Chunk, EVENT_ERROR, EVENT_STREAM_BLOCKED, H3ClientHandler, Http3Config, JsH3Event,
    OutboundAdmission,
};

use crate::abi::{
    ERR_AGAIN, ERR_BAD_ARGS, ERR_INVALID_HANDLE, ERR_PROTOCOL, RX_TX_BUFFER_LEN, bytes_in,
    set_global_error, str_in, write_out_message, write_out_ptr_len,
};
use crate::events::serialize_events;
use crate::handle::Slots;
use crate::json_opts::{build_h3_options, parse_connect_params};

struct H3Session {
    handler: H3ClientHandler,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    rx_buffer: Box<[u8]>,
    tx_buffer: Box<[u8]>,
    pending_events: Vec<JsH3Event>,
    json_scratch: Vec<u8>,
    data_scratch: Vec<u8>,
    last_error: Option<String>,
}

thread_local! {
    static SESSIONS: RefCell<Slots<H3Session>> = const { RefCell::new(Slots::new()) };
}

fn with_session_mut<R>(handle: u32, f: impl FnOnce(&mut H3Session) -> R) -> Option<R> {
    SESSIONS.with(|s| s.borrow_mut().get_mut(handle).map(f))
}

/// # Safety
/// `opts_ptr`/`opts_len` must describe a valid, readable UTF-8 JSON byte
/// range in this module's own linear memory (the caller wrote it there via
/// `wasm_alloc`). See the crate-level doc comment.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_new(opts_ptr: u32, opts_len: u32) -> u32 {
    let bytes = unsafe { bytes_in(opts_ptr, opts_len) };
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            set_global_error(format!("[h3:config] invalid options JSON: {e}"));
            return 0;
        }
    };

    let params = match parse_connect_params(&value) {
        Ok(p) => p,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let opts = match build_h3_options(&value) {
        Ok(o) => o,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let mut quiche_config = match Http3Config::new_client_quiche_config_in_memory(&opts) {
        Ok(c) => c,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };

    let session_ticket = opts.session_ticket.as_deref();
    let handler = H3ClientHandler::new_direct(
        params.scid,
        params.local_addr,
        params.server_addr,
        &params.server_name,
        session_ticket,
        None, // qlog_dir: N5, qlog excluded from the wasm build
        None, // qlog_level
        &mut quiche_config,
        Arc::new(OutboundAdmission::default()),
    );

    let Some(mut handler) = handler else {
        set_global_error("[h3:quic] quiche::connect failed (invalid config or SCID)".to_string());
        return 0;
    };

    if params.keylog {
        handler.enable_keylog();
    }

    SESSIONS.with(|s| {
        s.borrow_mut().insert(H3Session {
            handler,
            peer_addr: params.server_addr,
            local_addr: params.local_addr,
            rx_buffer: vec![0u8; RX_TX_BUFFER_LEN].into_boxed_slice(),
            tx_buffer: vec![0u8; RX_TX_BUFFER_LEN].into_boxed_slice(),
            pending_events: Vec::new(),
            json_scratch: Vec::new(),
            data_scratch: Vec::new(),
            last_error: None,
        })
    })
}

/// `handle = 0` reads the global last-error slot (construction failures).
#[unsafe(no_mangle)]
pub extern "C" fn h3c_last_error(handle: u32, buf_ptr: u32, cap: u32) -> i32 {
    let msg = if handle == 0 {
        crate::abi::take_global_error_for_read()
    } else {
        SESSIONS.with(|s| s.borrow().get(handle).and_then(|sess| sess.last_error.clone()))
    };
    write_out_message(msg, buf_ptr, cap)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_rx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.rx_buffer.as_ptr() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_tx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.tx_buffer.as_ptr() as u32).unwrap_or(0)
}

/// JS has already copied a received datagram into `h3c_rx_buffer()[..len]`.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_recv(handle: u32, len: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let len = (len as usize).min(sess.rx_buffer.len());
        let peer = sess.peer_addr;
        let local = sess.local_addr;
        sess.handler.process_packet_for_handle(
            &mut sess.rx_buffer[..len],
            peer,
            local,
            usize::MAX,
            &mut sess.pending_events,
            handle,
        );
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Writes the next outbound datagram into `h3c_tx_buffer()`; loop until 0.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_next_send(handle: u32) -> i64 {
    with_session_mut(handle, |sess| match sess.handler.try_send_next() {
        Some(tx) => {
            let n = tx.payload_len().min(sess.tx_buffer.len());
            sess.tx_buffer[..n].copy_from_slice(&tx.payload()[..n]);
            let recycle = tx.into_recycle_buffer();
            sess.handler.recycle_tx_buffers_into_pool(vec![recycle]);
            n as i64
        }
        None => 0,
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `-1` = no timer pending (also returned, harmlessly, for an invalid
/// handle — see the crate-level doc comment on this dual meaning).
#[unsafe(no_mangle)]
pub extern "C" fn h3c_timeout_ms(handle: u32) -> i64 {
    with_session_mut(handle, |sess| match sess.handler.next_timer_deadline() {
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
pub extern "C" fn h3c_on_timeout(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler.process_timers_for_handle(
            Instant::now(),
            usize::MAX,
            &mut sess.pending_events,
            handle,
        );
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Settles app events / drain events / retried pending writes (mirroring
/// the native per-tick sweep order in `worker.rs`'s shared-client reactor:
/// `poll_app_events` → `poll_drain_events` → `flush_pending_writes`) and
/// then serializes + drains the accumulated batch. Safe to call this as
/// the one "did anything change?" checkpoint after any `h3c_recv` /
/// `h3c_on_timeout` / `h3c_stream_send` — the settle calls are cheap
/// no-ops when there is nothing new to find.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_drain_events(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .poll_app_events_for_handle(usize::MAX, &mut sess.pending_events, handle);
        sess.handler
            .poll_drain_events_for_handle(&mut sess.pending_events, handle);
        sess.handler
            .flush_pending_writes_for_handle(&mut sess.pending_events, handle);

        let events = std::mem::take(&mut sess.pending_events);
        serialize_events(&events, &mut sess.json_scratch, &mut sess.data_scratch);
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Stream id, or a negative code — a flow-control block's fetched message
/// (`h3c_last_error`) contains the substring `StreamBlocked`.
///
/// # Safety
/// `headers_json_ptr`/`len` must describe a valid, readable UTF-8 JSON
/// array-of-`{name,value}` byte range (`[{"name":"...","value":"..."}]`).
#[unsafe(no_mangle)]
pub extern "C" fn h3c_send_request(
    handle: u32,
    headers_json_ptr: u32,
    headers_json_len: u32,
    fin: i32,
) -> i64 {
    let Some(headers) = parse_headers_json(headers_json_ptr, headers_json_len) else {
        set_session_error(handle, "[h3:config] invalid headers JSON".to_string());
        return ERR_BAD_ARGS;
    };
    with_session_mut(handle, |sess| {
        match sess.handler.send_request(headers, fin != 0) {
            Ok(stream_id) => i64::try_from(stream_id).unwrap_or(i64::MAX),
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
/// protocol error.
///
/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_stream_send(handle: u32, stream_id: u64, ptr: u32, len: u32, fin: i32) -> i64 {
    let data = unsafe { bytes_in(ptr, len) }.to_vec();
    with_session_mut(handle, |sess| {
        let chunk = if data.is_empty() {
            Chunk::empty()
        } else {
            Chunk::unpooled(data)
        };
        let before = sess.pending_events.len();
        let released =
            sess.handler
                .queue_stream_send(stream_id, chunk, fin != 0, &mut sess.pending_events, handle);
        classify_send_outcome(&sess.pending_events[before..], released)
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
        // Either newly blocked (has_blocked) or appended to an
        // already-blocked stream's backlog (has_blocked false, released 0
        // either way) — both are backpressure from the caller's view.
        let _ = has_blocked;
        ERR_AGAIN
    } else {
        released_units as i64
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_stream_close(handle: u32, stream_id: u64, error_code: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler.close_stream(stream_id, error_code);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_send_datagram(handle: u32, ptr: u32, len: u32) -> i64 {
    let data = unsafe { bytes_in(ptr, len) }.to_vec();
    with_session_mut(handle, |sess| {
        if sess.handler.send_datagram(Chunk::unpooled(data)) {
            0i64
        } else {
            ERR_AGAIN
        }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_ping(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        if sess.handler.ping() { 0i64 } else { ERR_AGAIN }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_session_metrics(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let metrics = sess.handler.metrics_snapshot();
        let json = crate::events::metrics_only_json(&metrics);
        sess.json_scratch.clear();
        sess.json_scratch.extend_from_slice(json.as_bytes());
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_remote_settings(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let settings = sess.handler.remote_settings();
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

/// # Safety
/// `reason_ptr`/`reason_len` must describe a valid, readable UTF-8 byte
/// range (may be zero-length).
#[unsafe(no_mangle)]
pub extern "C" fn h3c_close(handle: u32, code: u32, reason_ptr: u32, reason_len: u32) -> i64 {
    let reason = unsafe { str_in(reason_ptr, reason_len) }.unwrap_or("");
    with_session_mut(handle, |sess| {
        sess.handler.close_session(code, reason);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `0` = no keylog lines accumulated since the last call.
#[unsafe(no_mangle)]
pub extern "C" fn h3c_take_keylog(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let lines = sess.handler.take_keylog_lines();
        if lines.is_empty() {
            return 0i64;
        }
        sess.data_scratch.clear();
        sess.data_scratch.extend_from_slice(&lines);
        write_out_ptr_len(&sess.data_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_is_done(handle: u32) -> i32 {
    with_session_mut(handle, |sess| i32::from(sess.handler.is_reapable())).unwrap_or(1)
}

#[unsafe(no_mangle)]
pub extern "C" fn h3c_free(handle: u32) {
    SESSIONS.with(|s| {
        s.borrow_mut().remove(handle);
    });
}

fn set_session_error(handle: u32, msg: String) {
    with_session_mut(handle, |sess| sess.last_error = Some(msg));
}

/// Parses `[{"name": "...", "value": "..."}]` into the `(String, String)`
/// pairs `H3ClientHandler::send_request` expects.
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
