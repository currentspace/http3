//! `qs_*` extern "C" ABI — raw QUIC server. Mirrors `h3_server.rs` exactly
//! minus `send_response_headers`/`remote_settings` (no H3 framing/QPACK
//! in raw QUIC) — see that module's doc comment for the full "one handle
//! = the whole bound server" rationale, and the crate-level doc comment
//! for the shared conventions.
//!
//! No server-initiated-stream export: native's `QuicServerHandler` has no
//! such capability today (`QuicServerCommand` has no `OpenStream`
//! variant), so none is invented here either.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use http3::wasm_exports::{
    Chunk, ClientAuthMode, EVENT_ERROR, EVENT_STREAM_BLOCKED, JsH3Event, OutboundAdmission,
    QuicServerConfig, QuicServerHandler, TxDatagram, new_quic_server_config_in_memory,
};

use crate::abi::{
    ERR_AGAIN, ERR_BAD_ARGS, ERR_INVALID_HANDLE, ERR_PROTOCOL, RX_TX_BUFFER_LEN, bytes_in,
    set_global_error, str_in, write_out_message, write_out_ptr_len,
};
use crate::events::serialize_events;
use crate::handle::Slots;
use crate::json_opts::{build_quic_server_options, parse_server_params};

struct QuicServerSession {
    handler: QuicServerHandler,
    local_addr: SocketAddr,
    rx_buffer: Box<[u8]>,
    tx_buffer: Box<[u8]>,
    pending_tx: VecDeque<TxDatagram>,
    last_send_dest: Option<SocketAddr>,
    pending_events: Vec<JsH3Event>,
    json_scratch: Vec<u8>,
    data_scratch: Vec<u8>,
    dest_scratch: Vec<u8>,
    last_error: Option<String>,
}

thread_local! {
    static SESSIONS: RefCell<Slots<QuicServerSession>> = const { RefCell::new(Slots::new()) };
}

fn with_session_mut<R>(handle: u32, f: impl FnOnce(&mut QuicServerSession) -> R) -> Option<R> {
    SESSIONS.with(|s| s.borrow_mut().get_mut(handle).map(f))
}

fn set_session_error(handle: u32, msg: String) {
    with_session_mut(handle, |sess| sess.last_error = Some(msg));
}

/// `opts` — camelCase, matching `lib/quic-server.ts`'s `QuicServerOptions`
/// exactly: `key`/`cert` (PEM, mandatory), `ca` (optional), `clientAuth`,
/// `alpn`, `maxIdleTimeoutMs`, `maxUdpPayloadSize`, `initialMaxData`,
/// `initialMaxStreamDataBidiLocal`, `initialMaxStreamsBidi`,
/// `disableActiveMigration`, `enableDatagrams`, `disableRetry`,
/// `maxConnections`, `keylog`, plus the ABI-only `localAddr` and
/// `retryTokenKeyHex` (see `h3_server.rs`'s `hs_new` doc comment — this
/// is the identical convention).
///
/// # Safety
/// `opts_ptr`/`opts_len` must describe a valid, readable UTF-8 JSON byte
/// range in this module's own linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn qs_new(opts_ptr: u32, opts_len: u32) -> u32 {
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

    let opts = match build_quic_server_options(&value) {
        Ok(o) => o,
        Err(e) => {
            set_global_error(format!("[h3:config] {e}"));
            return 0;
        }
    };

    let quiche_config = match new_quic_server_config_in_memory(&opts) {
        Ok(c) => c,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };

    let client_auth = match ClientAuthMode::parse(opts.client_auth.as_deref(), opts.ca.is_some()) {
        Ok(mode) => mode,
        Err(e) => {
            set_global_error(e.tagged_message());
            return 0;
        }
    };
    let server_config = QuicServerConfig {
        // N5: qlog excluded from the wasm build.
        qlog_dir: None,
        qlog_level: None,
        max_connections: opts.max_connections.unwrap_or(10_000) as usize,
        disable_retry: opts.disable_retry.unwrap_or(false),
        client_auth,
        // No `quicLb`/`serverId` support for the raw QUIC server — mirrors
        // native, which has no such option on `JsQuicServerOptions` either
        // (QUIC-LB is an H3-server-only option in this codebase today).
        cid_encoding: http3::wasm_exports::CidEncoding::random(),
        // Irrelevant on wasm (no OS transport driver to select), but the
        // field exists on `QuicServerConfig` — `Portable` is the value
        // native uses for its own non-`io_uring` fallback path.
        runtime_mode: http3::wasm_exports::TransportRuntimeMode::Portable,
    };

    let handler = QuicServerHandler::new_direct(
        quiche_config,
        server_config,
        params.retry_token_key,
        Arc::new(OutboundAdmission::default()),
    );

    SESSIONS.with(|s| {
        s.borrow_mut().insert(QuicServerSession {
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
pub extern "C" fn qs_last_error(handle: u32, buf_ptr: u32, cap: u32) -> i32 {
    let msg = if handle == 0 {
        crate::abi::take_global_error_for_read()
    } else {
        SESSIONS.with(|s| s.borrow().get(handle).and_then(|sess| sess.last_error.clone()))
    };
    write_out_message(msg, buf_ptr, cap)
}

#[unsafe(no_mangle)]
pub extern "C" fn qs_rx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.rx_buffer.as_ptr() as u32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn qs_tx_buffer(handle: u32) -> u32 {
    with_session_mut(handle, |sess| sess.tx_buffer.as_ptr() as u32).unwrap_or(0)
}

/// # Safety
/// `peer_addr_ptr`/`peer_addr_len` must describe a valid, readable UTF-8
/// byte range.
#[unsafe(no_mangle)]
pub extern "C" fn qs_recv(handle: u32, len: u32, peer_addr_ptr: u32, peer_addr_len: u32) -> i64 {
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

#[unsafe(no_mangle)]
pub extern "C" fn qs_next_send(handle: u32) -> i64 {
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

#[unsafe(no_mangle)]
pub extern "C" fn qs_next_send_dest(handle: u32, out_ptr_ptr: u32) -> i64 {
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

#[unsafe(no_mangle)]
pub extern "C" fn qs_timeout_ms(handle: u32) -> i64 {
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
pub extern "C" fn qs_on_timeout(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .expire_timers(Instant::now(), usize::MAX, &mut sess.pending_events);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// Settles app/drain events + retried pending writes + closed connections
/// (same rationale as `hs_drain_events`) and serializes + drains the batch.
#[unsafe(no_mangle)]
pub extern "C" fn qs_drain_events(handle: u32, out_ptr_ptr: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler
            .collect_app_events(usize::MAX, &mut sess.pending_events);
        sess.handler
            .collect_drain_events(usize::MAX, &mut sess.pending_events);
        sess.handler.flush_all_pending_writes(&mut sess.pending_events);
        sess.handler.reap_closed_connections(&mut sess.pending_events);

        let events = std::mem::take(&mut sess.pending_events);
        serialize_events(&events, &mut sess.json_scratch, &mut sess.data_scratch);
        write_out_ptr_len(&sess.json_scratch, out_ptr_ptr)
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `>=0` admitted bytes (FIN-only accept = 1), `-1` backpressure, `-2`
/// protocol error.
///
/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range.
#[unsafe(no_mangle)]
pub extern "C" fn qs_stream_send(
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
pub extern "C" fn qs_stream_close(
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
pub extern "C" fn qs_send_datagram(handle: u32, conn_handle: u32, ptr: u32, len: u32) -> i64 {
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
pub extern "C" fn qs_ping(handle: u32, conn_handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        if sess.handler.ping(conn_handle) { 0i64 } else { ERR_AGAIN }
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

/// `-3` (invalid handle) doubles as "no such connection" — see
/// `hs_session_metrics`'s identical note.
#[unsafe(no_mangle)]
pub extern "C" fn qs_session_metrics(handle: u32, conn_handle: u32, out_ptr_ptr: u32) -> i64 {
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

/// Immediately closes a single connection with a CONNECTION_CLOSE — raw
/// QUIC has no GOAWAY concept, so (unlike `hs_close_connection`) this is
/// not deferred.
///
/// # Safety
/// `reason_ptr`/`reason_len` must describe a valid, readable UTF-8 byte
/// range (may be zero-length).
#[unsafe(no_mangle)]
pub extern "C" fn qs_close_connection(
    handle: u32,
    conn_handle: u32,
    code: u32,
    reason_ptr: u32,
    reason_len: u32,
) -> i64 {
    let reason = unsafe { str_in(reason_ptr, reason_len) }.unwrap_or("");
    with_session_mut(handle, |sess| {
        sess.handler.close_connection(conn_handle, code, reason);
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn qs_connection_is_closed(handle: u32, conn_handle: u32) -> i32 {
    with_session_mut(handle, |sess| {
        i32::from(sess.handler.connection_is_closed(conn_handle))
    })
    .unwrap_or(1)
}

#[unsafe(no_mangle)]
pub extern "C" fn qs_is_done(handle: u32) -> i32 {
    with_session_mut(handle, |sess| i32::from(sess.handler.is_idle())).unwrap_or(1)
}

/// Graceful shutdown of the whole server (closes every live connection).
#[unsafe(no_mangle)]
pub extern "C" fn qs_shutdown(handle: u32) -> i64 {
    with_session_mut(handle, |sess| {
        sess.handler.shutdown_all_connections();
        0i64
    })
    .unwrap_or(ERR_INVALID_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn qs_free(handle: u32) {
    SESSIONS.with(|s| {
        s.borrow_mut().remove(handle);
    });
}
