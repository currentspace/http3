//! Event types bridged from the Rust worker thread to the JS main thread
//! via a napi `ThreadsafeFunction`.

#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg(feature = "node-api")]
type ByteBuf = napi::bindgen_prelude::Buffer;
#[cfg(not(feature = "node-api"))]
type ByteBuf = Vec<u8>;

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
    ) -> Self {
        Self {
            event_type: EVENT_NEW_STREAM,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: if data.is_empty() { None } else { Some(data.into()) },
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

    pub fn data(conn_handle: u32, stream_id: u64, data: Vec<u8>, fin: bool) -> Self {
        Self {
            event_type: EVENT_DATA,
            conn_handle,
            stream_id: stream_id as i64,
            headers: None,
            data: Some(data.into()),
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

    pub fn datagram(conn_handle: u32, data: Vec<u8>) -> Self {
        Self {
            event_type: EVENT_DATAGRAM,
            conn_handle,
            stream_id: -1,
            headers: None,
            data: Some(data.into()),
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
