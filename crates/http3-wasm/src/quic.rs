//! `qc_*` extern "C" ABI — raw QUIC client. Mirrors `h3.rs` exactly minus
//! `send_request`/`remote_settings`, plus `qc_open_stream`. See the
//! crate-level doc comment for the shared conventions.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use http3::wasm_exports::{
    Chunk, EVENT_ERROR, EVENT_STREAM_BLOCKED, JsH3Event, OutboundAdmission, QuicClientHandler,
    new_quic_client_config_in_memory,
};

use crate::abi::{
    ERR_AGAIN, ERR_INVALID_HANDLE, ERR_PROTOCOL, RX_TX_BUFFER_LEN, bytes_in, set_global_error,
    str_in, write_out_message, write_out_ptr_len,
};
use crate::events::serialize_events;
use crate::handle::Slots;
use crate::json_opts::{build_quic_options, parse_connect_params};

struct QuicSession {
    handler: QuicClientHandler,
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
    static SESSIONS: RefCell<Slots<QuicSession>> = const { RefCell::new(Slots::new()) };
}

fn with_session_mut<R>(handle: u32, f: impl FnOnce(&mut QuicSession) -> R) -> Option<R> {
    SESSIONS.with(|s| s.borrow_mut().get_mut(handle).map(f))
}

/// # Safety
/// `opts_ptr`/`opts_len` must describe a valid, readable UTF-8 JSON byte
/// range in this module's own linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn qc_new(opts_ptr: u32, opts_len: u32) -> u32 {
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

    let opts = match build_quic_options(&value) {
        Ok(o) => o,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let mut quiche_config = match new_quic_client_config_in_memory(&opts) {
        Ok(c) => c,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };

    let session_ticket = opts.session_ticket.as_deref();
    let handler = QuicClientHandler::new_direct(
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
        s.borrow_mut().insert(QuicSession {
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
pub extern "C" fn qc_last_error(handle: u32, buf_ptr: u32, cap: u32) -> i32 {
    let msg = if handle == 0 {
        crate::abi::take_global_error_for_read()
    } else {
        SESSIONS.with(|s| s.borrow().get(handle).and_then(|sess| sess.last_error.clone()))
    };
    write_out_message(msg, buf_ptr, cap)
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_rx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.rx_buffer.as_ptr() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_tx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.tx_buffer.as_ptr() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_recv(handle: u32, len: u32) -> i64 {
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

#[unsafe(no_mangle)]
pub extern "C" fn qc_next_send(handle: u32) -> i64 {
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
/// handle).
#[unsafe(no_mangle)]
pub extern "C" fn qc_timeout_ms(handle: u32) -> i64 {
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
pub extern "C" fn qc_on_timeout(handle: u32) -> i64 {
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

/// Settles app events / drain events / retried pending writes (same
/// rationale as `h3c_drain_events`) and serializes + drains the batch.
#[unsafe(no_mangle)]
pub extern "C" fn qc_drain_events(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .poll_app_events_for_handle(usize::MAX, &mut sess.pending_events, handle);
        sess.handler.poll_drain_events_for_handle(
            usize::MAX,
            &mut sess.pending_events,
            handle,
        );
        sess.handler
            .flush_pending_writes_for_handle(&mut sess.pending_events, handle);

        let events = std::mem::take(&mut sess.pending_events);
        serialize_events(&events, &mut sess.json_scratch, &mut sess.data_scratch);
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Opens the next local-initiated client bidi stream (ids 0, 4, 8, … per
/// QUIC's stream-id scheme), returning its id or a negative code.
#[unsafe(no_mangle)]
pub extern "C" fn qc_open_stream(handle: u32) -> i64 {
    with_session_mut(handle, |sess| match sess.handler.open_bidi_stream() {
        Ok(stream_id) => i64::try_from(stream_id).unwrap_or(i64::MAX),
        Err(e) => {
            sess.last_error = Some(e.tagged_message());
            ERR_PROTOCOL
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
pub extern "C" fn qc_stream_send(handle: u32, stream_id: u64, ptr: u32, len: u32, fin: i32) -> i64 {
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
        let _ = has_blocked;
        ERR_AGAIN
    } else {
        released_units as i64
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_stream_close(handle: u32, stream_id: u64, error_code: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler.close_stream(stream_id, error_code);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn qc_send_datagram(handle: u32, ptr: u32, len: u32) -> i64 {
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
pub extern "C" fn qc_ping(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        if sess.handler.ping() { 0i64 } else { ERR_AGAIN }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_session_metrics(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        let metrics = sess.handler.metrics_snapshot();
        let json = crate::events::metrics_only_json(&metrics);
        sess.json_scratch.clear();
        sess.json_scratch.extend_from_slice(json.as_bytes());
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// # Safety
/// `reason_ptr`/`reason_len` must describe a valid, readable UTF-8 byte
/// range (may be zero-length).
#[unsafe(no_mangle)]
pub extern "C" fn qc_close(handle: u32, code: u32, reason_ptr: u32, reason_len: u32) -> i64 {
    let reason = unsafe { str_in(reason_ptr, reason_len) }.unwrap_or("");
    with_session_mut(handle, |sess| {
        sess.handler.close_session(code, reason);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `0` = no keylog lines accumulated since the last call.
#[unsafe(no_mangle)]
pub extern "C" fn qc_take_keylog(handle: u32, out_ptr_ptr: u32) -> i64 {
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
pub extern "C" fn qc_is_done(handle: u32) -> i32 {
    with_session_mut(handle, |sess| i32::from(sess.handler.is_reapable())).unwrap_or(1)
}

#[unsafe(no_mangle)]
pub extern "C" fn qc_free(handle: u32) {
    SESSIONS.with(|s| {
        s.borrow_mut().remove(handle);
    });
}
