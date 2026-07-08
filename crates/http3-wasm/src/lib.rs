//! `http3-wasm` — bare `extern "C"` ABI exposing this repo's HTTP/3 and raw
//! QUIC **client** protocol cores (the same `H3ClientHandler` /
//! `QuicClientHandler` the native N-API binding uses) to a host-agnostic
//! `wasm32-wasip1` artifact. See `docs/WASM_CLIENT_PLAN.md` §5.3 for the
//! full normative ABI spec this crate implements.
//!
//! # ABI conventions (read this before calling anything)
//!
//! - **Pointers** are `u32` offsets into *this module's own* exported
//!   linear memory. Because wasm gives host and guest the same flat
//!   address space, a pointer this crate hands back to the host (e.g.
//!   from `h3c_rx_buffer`) is directly usable by the host as an offset
//!   into `instance.exports.memory.buffer` — no separate translation
//!   step.
//! - **Strings/JSON** cross as `(ptr, len)` UTF-8 byte ranges.
//! - **Handles** are `u32` slab indices; `0` is never a valid handle.
//! - **Errors**: a negative return is a shared code —
//!   `-1` again/blocked, `-2` protocol/config error, `-3` invalid handle,
//!   `-4` bad arguments. Fetch the message with `{h3c,qc}_last_error`;
//!   `handle = 0` reads a global last-error slot (where construction
//!   failures from `{h3c,qc}_new` land, since a handle doesn't exist yet
//!   to hang the error off of).
//!
//! ## Buffer lifetime — footgun warning
//!
//! **Every buffer this crate hands back a pointer to (RX/TX staging
//! regions, `drain_events`/`session_metrics`/`remote_settings`/
//! `take_keylog` output, event `dataOff`/`dataLen` payload ranges) is
//! valid *only until the next call on the same handle*.** These are
//! reused scratch buffers, not fresh allocations per call — a second call
//! on the same handle may resize, clear, or otherwise invalidate the
//! previous pointer's contents (or, if a `Vec` backing it reallocates,
//! its address entirely). Callers **must** fully decode/copy everything
//! they need out of a returned buffer synchronously, before making any
//! other call on that handle. Only *dispatch* of an already-decoded batch
//! may be deferred — never the decode/copy itself.
//!
//! This mirrors the native binding's own external-buffer lifetime rules
//! (`RecyclableBuffer`) closely enough to reuse the same TS-side
//! discipline, but is stricter: there is no per-buffer refcounting here,
//! just "you have until the next call."

// Every `extern "C"` export (the actual ABI surface) lives in `h3.rs` /
// `quic.rs` / `abi.rs` (`wasm_alloc` / `wasm_free`). `#[unsafe(no_mangle)]`
// exports at the symbol level, independent of Rust-level `pub`/visibility,
// so these modules stay private.
mod abi;
mod encoding;
mod events;
mod h3;
mod handle;
mod json_opts;
mod quic;
