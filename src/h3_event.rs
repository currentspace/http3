//! Event types bridged from the Rust worker thread to the JS main thread
//! via a napi `ThreadsafeFunction`.

#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg(feature = "node-api")]
use std::sync::Arc;
#[cfg(feature = "node-api")]
use crate::buffer_pool::BufferRecycler;

#[cfg(not(feature = "node-api"))]
type ByteBuf = Vec<u8>;

/// A buffer that can optionally return to a pool when V8 GC collects it,
/// instead of being freed via glibc. Eliminates malloc fragmentation from
/// high-frequency buffer alloc/free cycles across the FFI boundary.
#[cfg(feature = "node-api")]
pub struct RecyclableBuffer {
    pub(crate) data: Vec<u8>,
    pub(crate) recycler: Option<Arc<BufferRecycler>>,
}

#[cfg(feature = "node-api")]
impl RecyclableBuffer {
    pub fn new(data: Vec<u8>, recycler: Option<Arc<BufferRecycler>>) -> Self {
        Self { data, recycler }
    }

    /// Create a non-recyclable buffer (for rare events like session_ticket).
    pub fn owned(data: Vec<u8>) -> Self {
        Self { data, recycler: None }
    }
}

#[cfg(feature = "node-api")]
impl std::fmt::Debug for RecyclableBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecyclableBuffer")
            .field("len", &self.data.len())
            .field("recyclable", &self.recycler.is_some())
            .finish()
    }
}

#[cfg(feature = "node-api")]
impl From<Vec<u8>> for RecyclableBuffer {
    fn from(data: Vec<u8>) -> Self {
        Self::owned(data)
    }
}

#[cfg(feature = "node-api")]
impl std::ops::Deref for RecyclableBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(feature = "node-api")]
impl AsRef<[u8]> for RecyclableBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

// NAPI trait impls for RecyclableBuffer so it can be used in #[napi(object)] structs.

#[cfg(feature = "node-api")]
impl napi::bindgen_prelude::TypeName for RecyclableBuffer {
    fn type_name() -> &'static str {
        "Buffer"
    }

    fn value_type() -> napi::ValueType {
        napi::ValueType::Object
    }
}

#[cfg(feature = "node-api")]
impl napi::bindgen_prelude::ValidateNapiValue for RecyclableBuffer {}

#[cfg(feature = "node-api")]
impl napi::bindgen_prelude::FromNapiValue for RecyclableBuffer {
    unsafe fn from_napi_value(
        env: napi::sys::napi_env,
        napi_val: napi::sys::napi_value,
    ) -> napi::Result<Self> {
        let buf = unsafe { napi::bindgen_prelude::Buffer::from_napi_value(env, napi_val)? };
        Ok(Self { data: buf.to_vec(), recycler: None })
    }
}

#[cfg(feature = "node-api")]
impl napi::bindgen_prelude::ToNapiValue for RecyclableBuffer {
    #[allow(unsafe_code)]
    unsafe fn to_napi_value(
        env: napi::sys::napi_env,
        val: Self,
    ) -> napi::Result<napi::sys::napi_value> {
        use std::ffi::c_void;

        let mut data = val.data;
        let len = data.len();

        if len == 0 {
            // Empty buffer — use standard NAPI path
            let mut ret = std::ptr::null_mut();
            let status = unsafe {
                napi::sys::napi_create_buffer(env, 0, std::ptr::null_mut(), &mut ret)
            };
            if status != napi::sys::Status::napi_ok {
                return Err(napi::Error::from_status(status.into()));
            }
            return Ok(ret);
        }

        let ptr = data.as_mut_ptr();
        let cap = data.capacity();
        std::mem::forget(data);

        if let Some(recycler) = val.recycler {
            // Recyclable — finalize callback returns to pool
            let hint = Box::new(RecycleHint { ptr, cap, recycler });
            let mut ret = std::ptr::null_mut();
            let status = unsafe {
                napi::sys::napi_create_external_buffer(
                    env,
                    len,
                    ptr as *mut c_void,
                    Some(finalize_recycle),
                    Box::into_raw(hint) as *mut c_void,
                    &mut ret,
                )
            };
            if status == napi::sys::Status::napi_no_external_buffers_allowed {
                // Fallback: reconstruct and use copy path
                let data = unsafe { Vec::from_raw_parts(ptr, len, cap) };
                let buf = napi::bindgen_prelude::Buffer::from(data);
                return unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env, buf) };
            }
            if status != napi::sys::Status::napi_ok {
                return Err(napi::Error::from_status(status.into()));
            }
            Ok(ret)
        } else {
            // Non-recyclable — reconstruct Buffer and use its standard to_napi_value
            let data = unsafe { Vec::from_raw_parts(ptr, len, cap) };
            let buf = napi::bindgen_prelude::Buffer::from(data);
            unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env, buf) }
        }
    }
}

#[cfg(feature = "node-api")]
struct RecycleHint {
    ptr: *mut u8,
    cap: usize,
    recycler: Arc<BufferRecycler>,
}

// SAFETY: The pointer is only accessed in the finalize callback, which runs
// after V8 GC has confirmed no JS references remain.
#[cfg(feature = "node-api")]
unsafe impl Send for RecycleHint {}

#[cfg(feature = "node-api")]
unsafe extern "C" fn finalize_recycle(
    _env: napi::sys::napi_env,
    _data: *mut std::ffi::c_void,
    hint: *mut std::ffi::c_void,
) {
    let hint = unsafe { Box::from_raw(hint as *mut RecycleHint) };
    // Reconstruct Vec with len=0 (content was consumed by JS), preserve capacity
    let buf = unsafe { Vec::from_raw_parts(hint.ptr, 0, hint.cap) };
    hint.recycler.recycle(buf);
}

#[cfg(feature = "node-api")]
type ByteBuf = RecyclableBuffer;

/// Type alias for the recycler parameter on hot-path event constructors.
/// Under `node-api`, carries `Option<Arc<BufferRecycler>>`.
/// Without `node-api`, it is `()` (zero-cost, optimized away).
#[cfg(feature = "node-api")]
pub type EventRecycler = Option<Arc<BufferRecycler>>;
#[cfg(not(feature = "node-api"))]
pub type EventRecycler = ();

/// A recycler value meaning "no pool — use normal free".
/// Works under both feature configurations.
#[cfg(feature = "node-api")]
pub const NO_RECYCLER: EventRecycler = None;
#[cfg(not(feature = "node-api"))]
pub const NO_RECYCLER: EventRecycler = ();

fn make_data_buf(data: Vec<u8>, _recycler: EventRecycler) -> ByteBuf {
    #[cfg(feature = "node-api")]
    { RecyclableBuffer::new(data, _recycler) }
    #[cfg(not(feature = "node-api"))]
    { data }
}

pub const EVENT_NEW_SESSION: u8 = 1;
pub const EVENT_NEW_STREAM: u8 = 2;
pub const EVENT_HEADERS: u8 = 3;
pub const EVENT_DATA: u8 = 4;
pub const EVENT_FINISHED: u8 = 5;
pub const EVENT_RESET: u8 = 6;
pub const EVENT_SESSION_CLOSE: u8 = 7;
pub const EVENT_DRAIN: u8 = 8;
pub const EVENT_GOAWAY: u8 = 9;
pub const EVENT_ERROR: u8 = 10;
pub const EVENT_HANDSHAKE_COMPLETE: u8 = 11;
pub const EVENT_SESSION_TICKET: u8 = 12;
pub const EVENT_METRICS: u8 = 13;
pub const EVENT_DATAGRAM: u8 = 14;
pub const EVENT_SHUTDOWN_COMPLETE: u8 = 15;

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Debug, Clone)]
pub struct JsHeader {
    pub name: String,
    pub value: String,
}

/// Metadata for rare events (new_session, error, reset).
/// Packed into a sub-object to avoid 5 null napi properties on every hot-path event.
#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsEventMeta {
    pub error_code: Option<u32>,
    pub error_reason: Option<String>,
    pub error_category: Option<String>,
    pub remote_addr: Option<String>,
    pub remote_port: Option<u16>,
    pub server_name: Option<String>,
    pub reason_code: Option<String>,
    pub runtime_driver: Option<String>,
    pub runtime_mode: Option<String>,
    pub requested_runtime_mode: Option<String>,
    pub fallback_occurred: Option<bool>,
    pub errno: Option<i32>,
    pub syscall: Option<String>,
    pub peer_certificate_presented: Option<bool>,
    pub peer_certificate_chain: Option<Vec<ByteBuf>>,
}

#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsH3Event {
    pub event_type: u8,
    pub conn_handle: u32,
    pub stream_id: i64,
    pub headers: Option<Vec<JsHeader>>,
    pub data: Option<ByteBuf>,
    pub fin: Option<bool>,
    pub meta: Option<JsEventMeta>,
    pub metrics: Option<JsSessionMetrics>,
}

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Debug, Clone)]
pub struct JsSessionMetrics {
    pub packets_in: u32,
    pub packets_out: u32,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub handshake_time_ms: f64,
    pub rtt_ms: f64,
    pub cwnd: i64,
    pub pmtu: i64,
}

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Debug, Clone)]
pub struct JsSetting {
    pub id: i64,
    pub value: i64,
}

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Debug, Clone)]
pub struct JsAddressInfo {
    pub address: String,
    pub family: String,
    pub port: u32,
}

impl JsH3Event {
    pub fn new_session(
        conn_handle: u32,
        remote_addr: String,
        remote_port: u16,
        server_name: String,
    ) -> Self {
        Self {
            event_type: EVENT_NEW_SESSION,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: Some(JsEventMeta {
                error_code: None,
                error_reason: None,
                error_category: None,
                remote_addr: Some(remote_addr),
                remote_port: Some(remote_port),
                server_name: Some(server_name),
                reason_code: None,
                runtime_driver: None,
                runtime_mode: None,
                requested_runtime_mode: None,
                fallback_occurred: None,
                errno: None,
                syscall: None,
                peer_certificate_presented: None,
                peer_certificate_chain: None,
            }),
            metrics: None,
        }
    }

    pub fn new_stream(conn_handle: u32, stream_id: u64) -> Self {
        Self {
            event_type: EVENT_NEW_STREAM,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    /// NEW_STREAM with first data/fin coalesced — saves one TSFN event per new stream.
    pub fn new_stream_with_data(
        conn_handle: u32,
        stream_id: u64,
        data: Vec<u8>,
        fin: bool,
        recycler: EventRecycler,
    ) -> Self {
        Self {
            event_type: EVENT_NEW_STREAM,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: if data.is_empty() {
                None
            } else {
                Some(make_data_buf(data, recycler))
            },
            fin: Some(fin),
            meta: None,
            metrics: None,
        }
    }

    pub fn headers(conn_handle: u32, stream_id: u64, headers: Vec<JsHeader>, fin: bool) -> Self {
        Self {
            event_type: EVENT_HEADERS,
            conn_handle,
            stream_id: stream_id as i64,
            headers: Some(headers),
            data: None,
            fin: Some(fin),
            meta: None,
            metrics: None,
        }
    }

    pub fn data(conn_handle: u32, stream_id: u64, data: Vec<u8>, fin: bool, recycler: EventRecycler) -> Self {
        Self {
            event_type: EVENT_DATA,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: Some(make_data_buf(data, recycler)),
            fin: Some(fin),
            meta: None,
            metrics: None,
        }
    }

    pub fn finished(conn_handle: u32, stream_id: u64) -> Self {
        Self {
            event_type: EVENT_FINISHED,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn reset(conn_handle: u32, stream_id: u64, error_code: u64) -> Self {
        Self {
            event_type: EVENT_RESET,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: None,
            fin: None,
            meta: Some(JsEventMeta {
                error_code: Some(error_code as u32),
                error_reason: None,
                error_category: None,
                remote_addr: None,
                remote_port: None,
                server_name: None,
                reason_code: None,
                runtime_driver: None,
                runtime_mode: None,
                requested_runtime_mode: None,
                fallback_occurred: None,
                errno: None,
                syscall: None,
                peer_certificate_presented: None,
                peer_certificate_chain: None,
            }),
            metrics: None,
        }
    }

    pub fn session_close(conn_handle: u32) -> Self {
        Self {
            event_type: EVENT_SESSION_CLOSE,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn drain(conn_handle: u32, stream_id: u64) -> Self {
        Self {
            event_type: EVENT_DRAIN,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn goaway(conn_handle: u32, stream_id: u64) -> Self {
        Self {
            event_type: EVENT_GOAWAY,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn error(conn_handle: u32, stream_id: i64, error_code: u32, reason: String) -> Self {
        Self {
            event_type: EVENT_ERROR,
            conn_handle,
            stream_id,
            headers: None,
            data: None,
            fin: None,
            meta: Some(JsEventMeta {
                error_code: Some(error_code),
                error_reason: Some(reason),
                error_category: None,
                remote_addr: None,
                remote_port: None,
                server_name: None,
                reason_code: None,
                runtime_driver: None,
                runtime_mode: None,
                requested_runtime_mode: None,
                fallback_occurred: None,
                errno: None,
                syscall: None,
                peer_certificate_presented: None,
                peer_certificate_chain: None,
            }),
            metrics: None,
        }
    }

    pub fn runtime_error(
        conn_handle: u32,
        driver: &str,
        syscall: &str,
        reason_code: &str,
        err: &std::io::Error,
    ) -> Self {
        Self {
            event_type: EVENT_ERROR,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: Some(JsEventMeta {
                error_code: None,
                error_reason: Some(err.to_string()),
                error_category: Some("runtime".into()),
                remote_addr: None,
                remote_port: None,
                server_name: None,
                reason_code: Some(reason_code.into()),
                runtime_driver: Some(driver.into()),
                runtime_mode: None,
                requested_runtime_mode: None,
                fallback_occurred: None,
                errno: err.raw_os_error(),
                syscall: Some(syscall.into()),
                peer_certificate_presented: None,
                peer_certificate_chain: None,
            }),
            metrics: None,
        }
    }

    pub fn handshake_complete(conn_handle: u32) -> Self {
        Self {
            event_type: EVENT_HANDSHAKE_COMPLETE,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn handshake_complete_with_peer_certificate(
        conn_handle: u32,
        peer_certificate_presented: bool,
        peer_certificate_chain: Option<Vec<Vec<u8>>>,
    ) -> Self {
        Self {
            event_type: EVENT_HANDSHAKE_COMPLETE,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: Some(JsEventMeta {
                error_code: None,
                error_reason: None,
                error_category: None,
                remote_addr: None,
                remote_port: None,
                server_name: None,
                reason_code: None,
                runtime_driver: None,
                runtime_mode: None,
                requested_runtime_mode: None,
                fallback_occurred: None,
                errno: None,
                syscall: None,
                peer_certificate_presented: Some(peer_certificate_presented),
                peer_certificate_chain: peer_certificate_chain
                    .map(|chain| chain.into_iter().map(Into::into).collect()),
            }),
            metrics: None,
        }
    }

    pub fn session_ticket(conn_handle: u32, ticket: Vec<u8>) -> Self {
        Self {
            event_type: EVENT_SESSION_TICKET,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: Some(ticket.into()),
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    pub fn metrics(conn_handle: u32, metrics: &JsSessionMetrics) -> Self {
        Self {
            event_type: EVENT_METRICS,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: Some(metrics.clone()),
        }
    }

    pub fn datagram(conn_handle: u32, data: Vec<u8>, recycler: EventRecycler) -> Self {
        Self {
            event_type: EVENT_DATAGRAM,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: Some(make_data_buf(data, recycler)),
            fin: None,
            meta: None,
            metrics: None,
        }
    }

    /// Sentinel event emitted as the last event before a worker thread exits.
    /// JS awaits this to guarantee all prior events have been delivered.
    pub fn shutdown_complete() -> Self {
        Self {
            event_type: EVENT_SHUTDOWN_COMPLETE,
            conn_handle: 0,
            stream_id: -1,
            headers: None,
            data: None,
            fin: None,
            meta: None,
            metrics: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_session_fields() {
        let ev = JsH3Event::new_session(42, "127.0.0.1".into(), 4433, "example.com".into());
        assert_eq!(ev.event_type, EVENT_NEW_SESSION);
        assert_eq!(ev.conn_handle, 42);
        assert_eq!(ev.stream_id, -1);
        assert!(ev.headers.is_none());
        assert!(ev.data.is_none());
        assert!(ev.fin.is_none());
        assert!(ev.metrics.is_none());
        let meta = ev.meta.expect("meta must be Some");
        assert_eq!(meta.remote_addr.as_deref(), Some("127.0.0.1"));
        assert_eq!(meta.remote_port, Some(4433));
        assert_eq!(meta.server_name.as_deref(), Some("example.com"));
    }

    #[test]
    fn test_new_stream_fields() {
        let ev = JsH3Event::new_stream(7, 12);
        assert_eq!(ev.event_type, EVENT_NEW_STREAM);
        assert_eq!(ev.conn_handle, 7);
        assert_eq!(ev.stream_id, 12_i64);
        assert!(ev.data.is_none());
        assert!(ev.fin.is_none());
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_new_stream_with_data_fields() {
        // Non-empty data is preserved.
        let ev = JsH3Event::new_stream_with_data(1, 4, vec![0xAA, 0xBB], true, NO_RECYCLER);
        assert_eq!(ev.event_type, EVENT_NEW_STREAM);
        assert_eq!(ev.stream_id, 4_i64);
        assert_eq!(ev.data.as_deref(), Some([0xAA, 0xBB].as_slice()));
        assert_eq!(ev.fin, Some(true));

        // Empty data is collapsed to None.
        let ev2 = JsH3Event::new_stream_with_data(1, 4, vec![], false, NO_RECYCLER);
        assert!(ev2.data.is_none());
        assert_eq!(ev2.fin, Some(false));
    }

    #[test]
    fn test_data_event_fields() {
        let ev = JsH3Event::data(3, 8, vec![1, 2, 3], false, NO_RECYCLER);
        assert_eq!(ev.event_type, EVENT_DATA);
        assert_eq!(ev.conn_handle, 3);
        assert_eq!(ev.stream_id, 8_i64);
        assert_eq!(ev.data.as_deref(), Some([1u8, 2, 3].as_slice()));
        assert_eq!(ev.fin, Some(false));

        let ev_fin = JsH3Event::data(3, 8, vec![4], true, NO_RECYCLER);
        assert_eq!(ev_fin.fin, Some(true));
    }

    #[test]
    fn test_finished_fields() {
        let ev = JsH3Event::finished(5, 16);
        assert_eq!(ev.event_type, EVENT_FINISHED);
        assert_eq!(ev.conn_handle, 5);
        assert_eq!(ev.stream_id, 16_i64);
        assert!(ev.data.is_none());
        assert!(ev.fin.is_none());
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_reset_fields() {
        let ev = JsH3Event::reset(9, 20, 256);
        assert_eq!(ev.event_type, EVENT_RESET);
        assert_eq!(ev.conn_handle, 9);
        assert_eq!(ev.stream_id, 20_i64);
        let meta = ev.meta.expect("meta must be Some");
        assert_eq!(meta.error_code, Some(256));
        assert!(meta.error_reason.is_none());
    }

    #[test]
    fn test_session_close_fields() {
        let ev = JsH3Event::session_close(11);
        assert_eq!(ev.event_type, EVENT_SESSION_CLOSE);
        assert_eq!(ev.conn_handle, 11);
        assert_eq!(ev.stream_id, -1);
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_drain_fields() {
        let ev = JsH3Event::drain(2, 32);
        assert_eq!(ev.event_type, EVENT_DRAIN);
        assert_eq!(ev.conn_handle, 2);
        assert_eq!(ev.stream_id, 32_i64);
        assert!(ev.data.is_none());
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_goaway_fields() {
        let ev = JsH3Event::goaway(6, 64);
        assert_eq!(ev.event_type, EVENT_GOAWAY);
        assert_eq!(ev.conn_handle, 6);
        assert_eq!(ev.stream_id, 64_i64);
        assert!(ev.data.is_none());
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_error_fields() {
        let ev = JsH3Event::error(10, 4, 0x0101, "flow control".into());
        assert_eq!(ev.event_type, EVENT_ERROR);
        assert_eq!(ev.conn_handle, 10);
        assert_eq!(ev.stream_id, 4);
        let meta = ev.meta.expect("meta must be Some");
        assert_eq!(meta.error_code, Some(0x0101));
        assert_eq!(meta.error_reason.as_deref(), Some("flow control"));
        assert!(meta.error_category.is_none());
    }

    #[test]
    fn test_runtime_error_fields() {
        let io_err = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        let ev = JsH3Event::runtime_error(13, "poll", "sendmsg", "SEND_FAIL", &io_err);
        assert_eq!(ev.event_type, EVENT_ERROR);
        assert_eq!(ev.conn_handle, 13);
        assert_eq!(ev.stream_id, -1);
        let meta = ev.meta.expect("meta must be Some");
        assert_eq!(meta.error_category.as_deref(), Some("runtime"));
        assert_eq!(meta.runtime_driver.as_deref(), Some("poll"));
        assert_eq!(meta.syscall.as_deref(), Some("sendmsg"));
        assert_eq!(meta.reason_code.as_deref(), Some("SEND_FAIL"));
        assert_eq!(meta.errno, Some(libc::ECONNREFUSED));
        assert!(meta.error_reason.is_some());
    }

    #[test]
    fn test_handshake_complete_fields() {
        let ev = JsH3Event::handshake_complete(15);
        assert_eq!(ev.event_type, EVENT_HANDSHAKE_COMPLETE);
        assert_eq!(ev.conn_handle, 15);
        assert_eq!(ev.stream_id, -1);
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_handshake_complete_with_peer_cert() {
        let chain = vec![vec![0x30, 0x82], vec![0x30, 0x83]];
        let ev = JsH3Event::handshake_complete_with_peer_certificate(20, true, Some(chain));
        assert_eq!(ev.event_type, EVENT_HANDSHAKE_COMPLETE);
        assert_eq!(ev.conn_handle, 20);
        let meta = ev.meta.expect("meta must be Some");
        assert_eq!(meta.peer_certificate_presented, Some(true));
        let certs = meta.peer_certificate_chain.expect("chain must be Some");
        assert_eq!(certs.len(), 2);
        assert_eq!(&certs[0][..], &[0x30, 0x82]);
        assert_eq!(&certs[1][..], &[0x30, 0x83]);

        // Without cert chain.
        let ev2 = JsH3Event::handshake_complete_with_peer_certificate(21, false, None);
        let meta2 = ev2.meta.expect("meta must be Some");
        assert_eq!(meta2.peer_certificate_presented, Some(false));
        assert!(meta2.peer_certificate_chain.is_none());
    }

    #[test]
    fn test_session_ticket_fields() {
        let ticket = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let ev = JsH3Event::session_ticket(25, ticket.clone());
        assert_eq!(ev.event_type, EVENT_SESSION_TICKET);
        assert_eq!(ev.conn_handle, 25);
        assert_eq!(ev.stream_id, -1);
        assert_eq!(ev.data.as_deref(), Some(ticket.as_slice()));
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_datagram_fields() {
        let payload = vec![0x01, 0x02, 0x03, 0x04];
        let ev = JsH3Event::datagram(30, payload.clone(), NO_RECYCLER);
        assert_eq!(ev.event_type, EVENT_DATAGRAM);
        assert_eq!(ev.conn_handle, 30);
        assert_eq!(ev.stream_id, -1);
        assert_eq!(ev.data.as_deref(), Some(payload.as_slice()));
        assert!(ev.meta.is_none());
    }

    #[test]
    fn test_shutdown_complete_fields() {
        let ev = JsH3Event::shutdown_complete();
        assert_eq!(ev.event_type, EVENT_SHUTDOWN_COMPLETE);
        assert_eq!(ev.conn_handle, 0);
        assert_eq!(ev.stream_id, -1);
        assert!(ev.data.is_none());
        assert!(ev.meta.is_none());
        assert!(ev.metrics.is_none());
    }
}
