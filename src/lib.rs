//! Native N-API addon providing QUIC and HTTP/3 for Node.js, powered by
//! cloudflare/quiche. Exposes worker-thread servers/clients for both HTTP/3
//! and raw QUIC to the TypeScript layer via napi-rs.

mod allocator;
mod arc_buf;
mod buffer_pool;
pub mod chunk_pool;
mod cid;
#[cfg(feature = "node-api")]
mod client;
#[cfg(feature = "os-runtime")]
mod client_topology;
mod config;
mod connection;
mod connection_map;
mod datagram;
mod error;
mod event_loop;
mod h3_event;
mod keylog_sink;
#[cfg(all(feature = "node-api", feature = "os-runtime"))]
mod mock_quic_profile;
mod outbound_admission;
mod pending_write;
mod ping_state;
#[cfg(feature = "os-runtime")]
pub mod profile;
mod proof_core;
#[cfg(feature = "node-api")]
mod quic_client;
mod quic_connection;
#[cfg(feature = "node-api")]
mod quic_server;
mod quic_worker;
mod reactor_metrics;
mod retry_token;
#[cfg(feature = "node-api")]
mod server;
mod server_sharding;
#[cfg(feature = "os-runtime")]
mod shared_client_reactor;
mod timer_heap;
#[cfg(feature = "os-runtime")]
mod transport;
pub mod unsafe_boundary;
mod worker;
mod write_outcome;

#[cfg(kani)]
mod proofs;

#[cfg(feature = "bench-internals")]
pub mod bench_exports {
    // ── Transport layer ─────────────────────────────────────────────
    pub use crate::timer_heap::TimerHeap;
    pub use crate::transport::mock::{MockDriver, MockWaker, PacketLossConfig};
    pub use crate::transport::{
        Driver, DriverWaker, ErasedWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
    };

    #[cfg(target_os = "linux")]
    pub use crate::transport::io_uring::{IoUringDriver, IoUringWaker};
    #[cfg(target_os = "linux")]
    pub use crate::transport::poll::{PollDriver, PollWaker};

    // ── Event system ────────────────────────────────────────────────
    pub use crate::event_loop::{EventBatcher, EventBatcherStatsHandle, EventSink, MAX_BATCH_SIZE};
    pub use crate::h3_event::{
        EVENT_DATA, EVENT_DATAGRAM, EVENT_DRAIN, EVENT_ERROR, EVENT_FINISHED, EVENT_GOAWAY,
        EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_METRICS, EVENT_NEW_SESSION,
        EVENT_NEW_STREAM, EVENT_PING_ACK, EVENT_RESET, EVENT_SESSION_CLOSE, EVENT_SESSION_TICKET,
        EVENT_SHUTDOWN_COMPLETE, EVENT_STREAM_BLOCKED, EVENT_WRITE_READY, JsEventMeta, JsH3Event,
        JsHeader, JsSessionMetrics,
    };
    pub use crate::profile::event_sink::{
        TaggedEventBatch, channel_batcher, counting_batcher, noop_batcher,
    };

    // ── Buffer pools ────────────────────────────────────────────────
    pub use crate::buffer_pool::{AdaptiveBufferPool, BufferPool};
    pub use crate::chunk_pool::Chunk;

    // ── Configuration ───────────────────────────────────────────────
    pub use crate::cid::CidEncoding;
    pub use crate::config::{
        ClientAuthMode, Http3Config, JsQuicClientOptions, JsQuicServerOptions,
        TransportRuntimeMode, new_quic_client_config, new_quic_server_config,
    };
    pub use crate::error::Http3NativeError;

    // ── H3 connection + worker ──────────────────────────────────────
    pub use crate::connection::{H3Connection, H3ConnectionInit};
    pub use crate::connection_map::ConnectionMap;
    pub use crate::worker::{
        ClientWorkerCommand, ClientWorkerHandle, H3ServerWorker, WorkerCommand, WorkerHandle,
        spawn_h3_client_on_driver, spawn_h3_server_worker_on_driver,
    };

    // ── QUIC connection + worker ────────────────────────────────────
    pub use crate::quic_connection::QuicConnection;
    pub use crate::quic_worker::{
        QuicClientCommand, QuicClientHandle, QuicServerCommand, QuicServerConfig, QuicServerHandle,
        QuicServerWorker, spawn_dedicated_quic_client_on_driver, spawn_server_worker_on_driver,
    };
}

#[cfg(feature = "fuzzing")]
pub mod fuzz_exports {
    /// Exercise recvmsg control-buffer parsing without exposing transport
    /// internals in normal builds.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn parse_recv_cmsgs(control: &[u8]) {
        let _ = crate::transport::socket::parse_recv_cmsgs(control);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn parse_recv_cmsgs(_control: &[u8]) {}

    pub fn pending_write_state(data: &[u8]) {
        crate::pending_write::fuzz_pending_write_state(data);
    }

    pub fn pending_write_state_ops(ops: &[(u8, u8, u8)]) {
        crate::pending_write::fuzz_pending_write_state_ops(ops.iter().copied());
    }

    /// Mint a retry token via the real `ConnectionMap` path (HMAC included,
    /// not just `proof_core::retry_token_model`'s pure parsing logic that
    /// Kani already covers) — the fuzz target's incremental value over the
    /// Kani proofs is exercising this integration and the real `boring`
    /// HMAC FFI call under adversarial/coverage-guided byte patterns.
    pub fn retry_token_mint(
        key: [u8; 32],
        peer: std::net::SocketAddr,
        odcid: &[u8],
    ) -> Vec<u8> {
        let map = crate::connection_map::ConnectionMap::with_key_bytes(
            1,
            crate::cid::CidEncoding::random(),
            key,
        );
        map.mint_token(&peer, odcid)
    }

    /// Validate a (possibly mutated/adversarial) retry token via the real
    /// `ConnectionMap` path.
    pub fn retry_token_validate(
        key: [u8; 32],
        token: &[u8],
        peer: std::net::SocketAddr,
    ) -> Option<Vec<u8>> {
        let map = crate::connection_map::ConnectionMap::with_key_bytes(
            1,
            crate::cid::CidEncoding::random(),
            key,
        );
        map.validate_token(token, &peer)
    }
}

/// Export surface for a future `crates/http3-wasm` extern-C ABI crate (see
/// `docs/WASM_CLIENT_PLAN.md` A2 task 3 / A3). Mirrors the `bench_exports`
/// / `fuzz_exports` pattern above exactly: `H3ClientHandler` and
/// `QuicClientHandler` are non-`pub` structs in non-`pub` modules
/// (`worker`/`quic_worker`), so this `pub mod` under a `pub use` is what
/// actually makes them reachable outside the crate. Must not pull in
/// `os-runtime` or `node-api` — every item re-exported here is either
/// always-compiled or itself feature-independent; `cargo check
/// --no-default-features --features wasm-abi` is the proof.
#[cfg(feature = "wasm-abi")]
pub mod wasm_exports {
    // ── Client protocol handlers (the sans-IO direct-call surface) ───
    pub use crate::quic_worker::QuicClientHandler;
    pub use crate::worker::H3ClientHandler;

    // ── Server protocol handlers (added alongside server-side wasm ABI
    // support; mirror the client handlers above exactly — `new_direct`
    // + plain callable methods, no thread/channel/command-enum
    // plumbing) ───────────────────────────────────────────────────────
    pub use crate::quic_worker::{QuicServerConfig, QuicServerHandler};
    pub use crate::worker::H3ServerHandler;

    // ── Connection routing / retry-token maps (a `new_direct` server
    // constructor builds one of these internally from a caller-supplied
    // 32-byte key; exported in case a caller needs to construct one
    // directly, e.g. for tests) ────────────────────────────────────────
    pub use crate::connection_map::ConnectionMap;

    // ── Event system ──────────────────────────────────────────────
    pub use crate::h3_event::{
        EVENT_DATA, EVENT_DATAGRAM, EVENT_DRAIN, EVENT_ERROR, EVENT_FINISHED, EVENT_GOAWAY,
        EVENT_HANDSHAKE_COMPLETE, EVENT_HEADERS, EVENT_METRICS, EVENT_NEW_SESSION,
        EVENT_NEW_STREAM, EVENT_PING_ACK, EVENT_RESET, EVENT_SESSION_CLOSE, EVENT_SESSION_TICKET,
        EVENT_SHUTDOWN_COMPLETE, EVENT_STREAM_BLOCKED, EVENT_WRITE_READY, JsEventMeta, JsH3Event,
        JsHeader, JsSessionMetrics,
    };

    // ── Buffers (handler method signatures use these) ────────────────
    pub use crate::arc_buf::{ArcBuf, ArcBufFactory};
    pub use crate::chunk_pool::Chunk;

    // ── Outbound write-admission window (a `new_direct` constructor
    // parameter: `Arc<OutboundAdmission>`, native call sites all use
    // `OutboundAdmission::default()`) ─────────────────────────────────
    pub use crate::outbound_admission::OutboundAdmission;

    // ── Config: option structs + builders (native file-based paths are
    // still `os-runtime`-gated inside `config.rs`; only the always-
    // compiled in-memory builders are usable here) ───────────────────
    pub use crate::config::{
        ClientAuthMode, Http3Config, JsClientOptions, JsQuicClientOptions, JsQuicServerOptions,
        JsServerOptions, TransportRuntimeMode, effective_pmtud_ceiling,
        new_quic_client_config_in_memory, new_quic_server_config_in_memory,
    };
    pub use crate::error::Http3NativeError;

    // ── CID encoding strategy (a `QuicServerConfig`/`Http3Config` field a
    // direct-call server caller must be able to construct) ────────────
    pub use crate::cid::CidEncoding;

    // ── Lifted datagram value types (A1 task 3) ──────────────────────
    pub use crate::datagram::{PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram};
}

#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg_attr(feature = "node-api", napi)]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg_attr(feature = "node-api", napi)]
pub fn runtime_telemetry() -> reactor_metrics::JsReactorTelemetrySnapshot {
    reactor_metrics::snapshot()
}

#[cfg_attr(feature = "node-api", napi)]
pub fn reset_runtime_telemetry() {
    reactor_metrics::reset();
}

#[cfg_attr(feature = "node-api", napi)]
pub fn set_lifecycle_trace_enabled(enabled: bool) {
    reactor_metrics::set_lifecycle_trace_enabled(enabled);
}

#[cfg_attr(feature = "node-api", napi)]
pub fn reset_lifecycle_trace() {
    reactor_metrics::reset_lifecycle_trace();
}

#[cfg_attr(feature = "node-api", napi)]
pub fn lifecycle_trace_snapshot() -> reactor_metrics::JsLifecycleTraceSnapshot {
    reactor_metrics::lifecycle_trace_snapshot()
}
