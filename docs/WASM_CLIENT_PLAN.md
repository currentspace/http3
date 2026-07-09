# WASM Client Plan — QUIC + HTTP/3 client core compiled to WebAssembly

**Status:** planned (feasibility PROVEN by spike, 2026-07-08)
**Owner:** implementation by agents; this document is the source of truth
**Companion artifacts:** `spikes/quiche-wasm-wasip1/` (proven build spike + quiche patch)

## 0. Executive summary

Goal: compile the **client half** of this library (raw QUIC client + HTTP/3
client) to WebAssembly as a **sans-IO protocol core with a small host
datagram adapter**, so that:

1. **Today** it runs in Node.js over a `node:dgram` adapter, tested against
   this repo's own native server — proving the core end-to-end.
2. **Later**, when Cloudflare workerd ships outbound UDP client sockets
   (cloudflare/workerd discussion #4463), the identical `.wasm` artifact runs
   in Workers behind a workerd datagram adapter, with zero changes to the
   protocol core.

**Primary approach (proven):** build quiche 0.29.2 + BoringSSL for
**`wasm32-wasip1`** (single-threaded, NOT `-threads`), expose a bare
`extern "C"` surface from a new thin workspace crate, and ship **one
hand-written ~150-line WASI preview1 shim** that runs identically in Node and
workerd. A feasibility spike executed during planning **already compiled and
ran this stack**: real BoringSSL cross-compiled with wasi-sdk 33, linked by
rust-lld, executed under Node — `quiche::connect()` + `Connection::send()`
produced a valid 1200-byte QUIC Initial packet and `Connection::timeout()`
returned a correct ~996 ms PTO. Module size 1.53 MB pre-`wasm-opt`.

**Explicitly rejected bindings** (details §4.2): napi-rs's wasm target
(Node-only: requires `node:wasi` + `worker_threads` + SharedArrayBuffer, and
its default 256 MiB shared memory exceeds workerd's 128 MB isolate limit) and
wasm-bindgen (cannot target wasip1 — since 0.2.123 the macro deliberately
compiles to inert stubs on `target_os = "wasi"`; and `wasm32-unknown-unknown`
is unreachable for quiche because BoringSSL needs a libc and
`Instant::now()` panics there).

**Documented fallback (do not start here):** quinn-proto 0.11.14 + rustls on
`wasm32-unknown-unknown` (upstream CI-tested; iroh ships it in browsers) plus
a minimal in-house H3 layer. ~6–10 engineer-weeks and permanent behavioral
divergence from the native quiche stack. Only revisit if the quiche/boring
build recipe becomes unmaintainable (§10 risk R1).

Why this is worth doing before workerd UDP exists: the Node-hosted WASM
client is a real deliverable on its own (a no-prebuilt-binary fallback for
unsupported platforms, an in-repo conformance harness for the protocol core,
and a deterministic-clock test rig), and the sans-IO seam it forces is the
architecture the repo already almost has.

---

## 1. Goals and non-goals

### Goals

- G1: `wasm32-wasip1` artifact containing the H3 client and raw QUIC client
  protocol state machines (quiche + BoringSSL inside), exercising the **same
  Rust protocol code** the native build uses (`H3ClientHandler`,
  `QuicClientHandler`, `H3Connection`, `QuicConnection`).
- G2: The existing public TS client API (`connect`/`connectAsync`,
  `connectQuic`/`connectQuicAsync`, `Http3ClientSession`, `QuicClientSession`,
  Duplex streams) works unchanged with `runtimeMode: 'wasm'` in Node.
- G3: Event contract parity: the wasm core reproduces the numeric event types
  1–18 and `JsH3Event` field names exactly, so `lib/client.ts` /
  `lib/quic-client.ts` dispatch code is shared between native and wasm
  backends.
- G4: Host adapters are swappable behind one `DatagramTransport` interface:
  Node `dgram` now; workerd datagram API later; test/mock transport for
  deterministic tests.
- G5: Full test lane in CI (build + run) within the repo's test budgets
  (suite < 1 min, full run < 2 min).
- G6: Upstream the 2-line quiche FFI fix so the fork disappears.

### Non-goals

- N1: **No server support in wasm.** Servers stay native-only (io_uring /
  kqueue / poll).
- N2: No wasm support for the existing napi surface (no napi-rs
  `wasm32-wasip1-threads` sidecar). It cannot run in workerd and roughly
  doubles CI build cost. Revisit only if there is demand for a Node
  no-prebuilt fallback of the *full* binding.
- N3: No performance parity promise. `OPENSSL_NO_ASM` pure-C crypto is
  expected 10–50× slower than AES-NI; this core is for
  client/control-plane workloads, not bulk transfer. (Mitigations in §10 R4.)
- N4: No WebTransport, no connection migration, no multipath (single
  connected flow per session; workerd model has no local addr control).
- N5: qlog is excluded from the wasm build (v1). Keylog is delivered as
  events instead of file tailing (§6.4).
- N6: 0-RTT / session-ticket **persistence APIs for Workers KV/DO storage**
  are out of scope; the existing `sessionTicket` option and
  `EVENT_SESSION_TICKET` event flow through as-is, which is sufficient.

---

## 2. Feasibility evidence (what is already proven vs. open)

Proven by the spike (`spikes/quiche-wasm-wasip1/`, executed 2026-07-08 on
this machine, macOS arm64, Rust 1.96, wasi-sdk 33, Node 26.4.0):

| Claim | Evidence |
|---|---|
| BoringSSL (boring-sys 4.22.0 pinned fork) builds for wasm32-wasip1 | cmake + wasi-sdk toolchain; libcrypto.a 1.7 MB, libssl.a 514 KB |
| quiche 0.29.2 compiles + links for wasm32-wasip1 | `cargo build --target wasm32-wasip1 --release` green |
| The module RUNS: Initial packet generation incl. TLS ClientHello, Initial AEAD seal, RNG | printed `initial packet: 1200 bytes` under Node |
| `std::time::Instant` works under WASI preview1 | `conn.timeout()` returned `Some(996ms)` |
| Import surface is tiny and thread-free | exactly 15 `wasi_snapshot_preview1` imports; no sockets/threads/poll |
| A hand-rolled preview1 shim suffices for a Rust wasip1 cdylib (no `node:wasi`) | `mini-shim.mjs` (10 functions) ran a 6-import Rust cdylib incl. panics. NOTE: the quiche module itself ran under `node:wasi`; the mini-shim needs six more stubs (`fd_fdstat_set_flags`, `fd_prestat_get`, `fd_prestat_dir_name`, `fd_read`, `path_filestat_get`, `path_open`) before it can host the quiche module — O5 |
| quiche FFI bug blocking wasm identified + fixed | 2-line patch (`AES_ecb_encrypt`/`CRYPTO_chacha_20` `-> c_void` → no return); trap reproduced, then gone |

Open (Phase 0 closes these):

- O1: **Full handshake** (x25519 completion, cert parse/verify, 1-RTT keys)
  and an H3 GET inside the wasm module — Initial-packet generation exercised
  key derivation/AEAD/RNG; the rest is pure computation in the already-linked
  libcrypto. High confidence, must be verified first.
- O2: In-memory CA loading via `quiche::Config::with_boring_ssl_ctx_builder`
  (spike used `verify_peer(false)`).
- O3: bindgen behavior with Linux CI libclang (spike used macOS libclang).
- O4: Crypto throughput on V8's wasm tier (informational; sets expectations),
  plus post-`wasm-opt`/gzip artifact size (currently unmeasured — only the
  1.53 MB pre-opt figure is real).
- O5: Run the quiche module under the **extended hand shim** (add the six
  missing stubs to `mini-shim.mjs`), then re-enumerate imports with
  `inspect-imports.mjs` and freeze the resulting names as the C2 allowlist.

---

## 3. Architecture

```
                     Node (today)                          workerd (when UDP lands)
        ┌──────────────────────────────┐       ┌───────────────────────────────────┐
        │ lib/client.ts, quic-client.ts│       │  same TS via exports "workerd" —  │
        │ session.ts, stream.ts (Duplex)│      │  nodejs_compat fidelity: §9 C15   │
        └──────────────┬───────────────┘       └───────────────┬───────────────────┘
                       │  ClientEventLoopLike (existing contract, §6.1)
        ┌──────────────┴───────────────┐
        │  WasmClientEventLoop (new)   │   lib/wasm/client-event-loop.ts
        │  pump: recv→transmit→events  │
        │        →re-arm single timer  │
        └───────┬──────────┬───────────┘
                │          │
   DatagramTransport    WasmCore loader + WASI shim (~150 lines, host-agnostic)
   (lib/wasm/transport) │  lib/wasm/core-loader.ts, lib/wasm/wasi-shim.ts
   ┌────────────┐       │
   │ node:dgram │  ┌────┴─────────────────────────────────────────┐
   │ (now)      │  │ http3_client.wasm  (wasm32-wasip1, one file) │
   ├────────────┤  │  crates/http3-wasm: extern "C" ABI (§5.3)    │
   │ workerd    │  │   └─ http3 crate (this repo, no-default-     │
   │ (future)   │  │      features): H3ClientHandler,             │
   ├────────────┤  │      QuicClientHandler, H3Connection,        │
   │ mock/test  │  │      QuicConnection, pending_write, …        │
   └────────────┘  │       └─ quiche 0.29.2 (patched, §5.4)       │
                   │           └─ BoringSSL (wasi-sdk build, §5.5)│
                   └──────────────────────────────────────────────┘
```

Division of responsibility (the sans-IO contract):

- **JS owns**: the UDP socket, DNS resolution, all timers (single re-armed
  `setTimeout` from `timeout_ms()`), randomness for the SCID (20 bytes via
  `crypto.getRandomValues`/`randomFillSync`), and the event dispatch loop.
- **WASM owns**: the entire QUIC + H3 state machine, TLS, packet
  build/parse, loss recovery bookkeeping, flow control, stream state,
  event generation. Its only host needs are the 15 WASI imports (clock,
  entropy, stdio stubs) supplied by our shim.

The pump replaces `run_event_loop` (`src/event_loop.rs:500`): after **any**
input (datagram in, timer fire, command call), JS runs
`poll_transmit → send datagrams; drain_events → dispatch (async);
timeout_ms → re-arm timer`. This mirrors the native tick order
(RX → timers → app events → drain events → pending writes → flush sends).

---

## 4. Key design decisions (with rationale)

### 4.1 Target: `wasm32-wasip1`, single-threaded

- `wasm32-unknown-unknown` is a dead end for quiche: `Instant::now()` panics
  (quiche calls it internally at ~10 sites; `RecvInfo` has no time field, so
  the clock is NOT injectable), and BoringSSL needs a libc.
- `wasm32-wasip1-threads` is unnecessary (BoringSSL supports single-threaded
  builds via `OPENSSL_NO_THREADS_CORRUPT_MEMORY_AND_LEAK_SECRETS_IF_THREADED`)
  and fatal for workerd (imports shared memory; workerd has no
  SharedArrayBuffer/threads).
- workerd runs *any* wasm module whose imports you satisfy in JS. Our module
  needs only 15 preview1 functions; we ship the shim. **Never depend on
  runtime WASI**: workerd's `node:wasi` throws `ERR_METHOD_NOT_IMPLEMENTED`;
  `@cloudflare/workers-wasi` is experimental with zero releases and
  effectively dormant since 2022 (only metadata/config commits since); and
  Node's `node:wasi` is still experimental and has a non-injectable clock.

### 4.2 Bindings: bare `extern "C"` + hand-written loader

- napi-rs wasm: only supports `wasm32-wasip1-threads`; generated loader
  hard-requires `node:wasi`, `node:worker_threads`, shared
  `WebAssembly.Memory` (default initial ≈256 MiB > workerd's 128 MB limit).
  Known to fail on Cloudflare Pages (napi-rs/node-rs#862). Node-only.
- wasm-bindgen: `wasm32-unknown-unknown` only; on wasip1 the macro emits
  inert stubs by design since 0.2.123 (tracking issue rustwasm#3421).
- Bare extern-C prior art: hash-wasm, sqlite-wasm, Frando/quinn-wasm. The
  surface we need is small (~21 exports per protocol prefix, ~40 total,
  §5.3) and the same JSON+ptr/len conventions serve every host.

### 4.3 Reuse the existing crate behind feature gates — no core-crate split

`cargo test --lib --no-default-features` already builds the protocol core
without napi (the `node-api` feature gates all TSFN/napi code, and
`h3_event.rs`/`config.rs`/`error.rs`/`event_loop.rs` swap napi types for
plain Rust ones). The client handlers (`H3ClientHandler`,
`QuicClientHandler`) contain **zero socket/thread code**; the
`ProtocolHandler` trait (`src/event_loop.rs:138-201`) is the exact sans-IO
seam. So: fix Cargo feature hygiene (§5.1), cfg-gate the OS-runtime pieces
(§5.2), and add a thin `crates/http3-wasm` member crate that depends on
`http3 = { path = "../..", default-features = false, features = ["wasm-abi"] }`
(note `"../.."` — the crate sits two levels deep, unlike `fuzz/`) —
following the existing
`fuzz/` member-crate precedent. No file moves, native builds untouched.

### 4.4 Preserve the event contract exactly

Event constants 1–18 and the `JsH3Event` field names are the FFI contract
(`src/h3_event.rs:142-159`). The wasm core emits the same events with the
same semantics so that TS dispatch, `EventCollector`, and the interop suites
are shared verbatim. Notable required behaviors (from the TS-layer contract):

- FIN-only accepted `streamSend` reports `written = 1` (sentinel; returning
  0 deadlocks `_final`).
- `EVENT_STREAM_BLOCKED` (16) on accepted-then-buffered transitions;
  `EVENT_DRAIN` (8) when a blocked stream fully drains.
- `EVENT_WRITE_READY` (18): the wasm core has no cross-thread admission
  queue, so it **never emits 18** — DRAIN alone must keep the stream tests
  green (verified in Phase 3 tests; contingency: emit 18 alongside DRAIN).
- `EVENT_SHUTDOWN_COMPLETE` (15) as the final sentinel after close — the TS
  `close()` awaits it with a 5 s fallback; emit it promptly after
  CONNECTION_CLOSE is flushed (do not wait out the 3×PTO drain period).
- `EVENT_SESSION_CLOSE` (7) exactly once, with
  peer-error/local-error/idle-timeout detail in `meta`.
- Events must NEVER be dispatched synchronously from inside a command call
  (TS registers streams in its map only *after* `sendRequest` returns);
  the WasmClientEventLoop queues dispatch via `queueMicrotask`.
- `sendRequest` blocked-by-flow-control must throw an `Error` whose message
  contains `StreamBlocked` (TS matches on that substring).

### 4.5 One wasm instance per JS realm, handle table inside

BoringSSL is built single-threaded, and JS is single-threaded — instantiate
the module once (module-scope, compile cached) and multiplex sessions through
a slab handle table in the wasm crate. Avoids per-session instantiation cost
and memory duplication.

### 4.6 Data plane: copy at the boundary, fixed staging buffers

Measured boundary costs are noise (~100–200 ns/packet round trip vs Node
dgram's own ceiling of ~93 kpps rx loopback, measured on macOS — Linux
numbers will differ): copy datagrams into a fixed RX
staging region; the core writes outbound packets into a fixed TX region; JS
copies out into pooled Buffers before `socket.send` (the send callback is the
documented reuse barrier). Event `data` payloads are copied out of linear
memory into fresh Buffers (JS owns them indefinitely — same semantics as the
native external buffers, minus the recycling optimization). Cached
`Uint8Array` views must be re-acquired when `view.buffer !== memory.buffer`
(memory.grow detaches views), or link with fixed `--max-memory` (§5.3).

### 4.7 Timers and clocks

- JS keeps exactly one armed `setTimeout` per session, re-armed from
  `timeout_ms()` after every pump; re-arm only when the deadline moved >1 ms
  (Node clamps <1 ms to 1 ms; measured fidelity ~1.3 ms steady-state).
- `enable_pacing(false)` in the wasm config profile — JS cannot honor sub-ms
  SendInfo release times, and workerd's frozen clock makes pacing
  meaningless. Keep CUBIC (default) as congestion controller.
- The shim's `clock_time_get` is **injectable**: tests can install a mock
  clock and deterministically fast-forward idle/PTO timers without
  wall-clock waits (impossible with `node:wasi`). quiche's internal
  `Instant::now()` resolves to this shim import, so no timestamps cross the
  ABI.
- workerd semantics (verified): `Date.now()` freezes during execution but
  advances on I/O, and **inside a `setTimeout` callback returns exactly the
  scheduled fire time** — so the single-armed-timer pattern is correct there
  too. RTT measurement: receive timestamps accurate (datagram arrival = I/O
  tick); send timestamps stale by ≤ CPU time since last I/O (sub-ms).

---

## 5. Workstream A — Rust

### A1. Cargo feature hygiene (prerequisite, pure refactor)

Today `napi`/`napi-derive` are unconditional dependencies (the `node-api`
feature gates code, not deps), and `src/transport/mod.rs:502` has a
`compile_error!` for any OS that isn't macOS/Linux.

Tasks:

1. Make dependencies optional and tie them to features in `Cargo.toml`:
   `node-api = ["dep:napi", "dep:napi-derive"]`. Gate `socket2`,
   `env_logger`, and `ring` behind a new default feature `os-runtime`
   (see below). Change the quiche dependency to
   `quiche = { workspace = true }` (dropping the unconditional `qlog`
   feature) and declare `qlog-files = ["quiche/qlog"]` included in the
   default set — otherwise quiche's qlog code ships in the wasm artifact,
   contradicting N5, and the spike never proved qlog-on-wasm (it built
   quiche with default features only). `libc` usage: `allocator.rs` and
   `transport/` are excluded from wasm anyway; `h3_event.rs`'s errno tests
   are test-only — gate the dependency `#[cfg]`-side rather than removing
   it if simpler. Keep `crossbeam-channel` (pool recyclers use `try_recv`
   only; compiles and works single-threaded on wasm).
2. Add feature `os-runtime` (in `default`): gates `src/transport/` (whole
   module incl. the `compile_error!`), **`src/profile/`** (mock_quic.rs and
   loopback.rs use `std::thread::spawn` and `transport::mock::MockDriver` —
   gate the whole `pub mod profile` in lib.rs), `spawn_*` functions and
   handle/command plumbing in `worker.rs`/`quic_worker.rs` (including their
   top-level `use std::thread;` imports at worker.rs:12 / quic_worker.rs:13),
   `run_event_loop`, `shared_client_reactor.rs`, `client_topology.rs`,
   `timer_heap.rs`, `connection_map.rs` retry-token/server paths, and the
   `TempFileGuard` cert loading path in `config.rs`.
   **Scope bound:** gate whole modules where possible; within-file gating
   only for items the wasm build actually flags; no file restructuring.
   Expect Phase 2 to bounce small residual gating fixups back into this
   feature — the host-target check is only a proxy (task 7).
3. Lift always-compiled value types out of `transport/mod.rs` into a small
   new module (e.g. `src/datagram.rs`): `TxDatagram` **must** move (the
   always-compiled `ProtocolHandler` trait references it in
   `process_packet`/`flush_sends`); move `RxDatagram`/`PollOutcome` with it
   for cohesion; and move **`RuntimeDriverKind`** too —
   `reactor_metrics.rs:12` imports it from `crate::transport` and
   reactor_metrics is unconditionally compiled. Also cfg-gate
   `reactor_metrics`'s `record_ecn_recv` (uses
   `crate::transport::socket::EcnCodePoint`, reactor_metrics.rs:575) and its
   call sites behind `os-runtime`, or lift `EcnCodePoint` as well.
4. `cid.rs`/SCID: add a constructor path that accepts caller-supplied random
   bytes so the wasm build does not need `ring` (JS supplies 20 bytes of
   entropy). Native keeps `ring::rand::SystemRandom`.
5. `reactor_metrics.rs` time: `now_ms()`/`lifecycle_trace_timestamp_ms()`
   call `SystemTime::now()` — fine under wasip1 (clock_time_get); no time
   shim needed. (The transport-type imports in task 3 are the real work.)
6. `connection.rs`/`quic_connection.rs`: cfg-gate `maybe_enable_qlog`
   (std::fs) behind the `qlog-files` feature from task 1.
7. `build.rs`: it unconditionally calls `napi_build::setup()` with
   `napi-build` as an unconditional build-dependency. Make `napi-build` an
   optional build-dependency tied to `node-api`, and call `setup()` only
   when `CARGO_FEATURE_NODE_API` is set (and skip when
   `CARGO_CFG_TARGET_FAMILY == "wasm"`), so the wasm build of the `http3`
   crate doesn't emit N-API link args.
8. CI seam gate: the full wasm build job (D2) is the real gate (bindgen
   needs the wasi sysroot even for `cargo check`). Keep
   `cargo check --no-default-features` (host target) as the cheap PR gate.

Definition of done: `pnpm run test:rust:unit`, `test:rust:mock:extended`,
`pnpm verify` all green; `cargo clippy --no-default-features` clean; no
behavioral change on native.

### A2. Extract direct-call client surfaces (no channels)

The command enums (`ClientWorkerCommand`, `QuicClientCommand`) with their
crossbeam `resp_tx` one-shots exist solely for the thread boundary. The
underlying inherent methods on `H3ClientHandler` (worker.rs:3262-3794) and
`QuicClientHandler` (quic_worker.rs:2930-3470) already exist.

Tasks:

1. Ensure every operation the wasm ABI needs is callable as a plain `&mut
   self` method on the handler (send_request, queue_stream_send,
   stream_close, send_datagram, ping, session_metrics, remote_settings,
   open_bidi_stream, close, take events, flush_sends, process_packet,
   process_timers, next timeout). Where logic lives only in the command
   dispatch match arms, hoist it into methods the dispatcher calls (keeps
   native path identical). **Scope bound:** hoist only what the §5.3 ABI
   table needs; do not refactor the dispatch machinery itself.
2. Make the handlers constructible without a `Driver`/socket: constructor
   takes `(options, scid_bytes, server_name, local_addr, peer_addr)` and the
   internal `EventBatcher`-equivalent is a plain `Vec<JsH3Event>` the caller
   drains (`app_event_budget = MAX`; the ack-gauge RX-pause machinery is
   bypassed — JS is synchronous with the core; keep `ackEventBatch` as a JS
   no-op for API compat).
3. **Export surface (`wasm-abi` feature):** everything A3 needs is
   crate-private today (`H3ClientHandler`, `QuicClientHandler` are non-pub
   in non-pub modules; `ProtocolHandler` is `pub(crate)`; `H3Connection`/
   `QuicConnection`/`JsH3Event` are pub-in-private re-exported only via the
   feature-gated `bench_exports`/`fuzz_exports`). Following that exact
   precedent, add `#[cfg(feature = "wasm-abi")] pub mod wasm_exports`
   re-exporting: `H3ClientHandler`, `QuicClientHandler`, `JsH3Event` +
   `JsEventMeta` + `JsSessionMetrics` + the `EVENT_*` constants, the client
   options/config builders, `Chunk`/`ArcBuf` as needed, and the lifted
   datagram value types — making the required inherent methods `pub` in the
   process. **A3 consumes only this surface.** `wasm-abi` implies nothing
   OS-y (must compile with `--no-default-features --features wasm-abi`).
4. Config for wasm: add an in-memory TLS path to `config.rs` using
   `quiche::Config::with_boring_ssl_ctx_builder` + boring's `X509`/`PKey`
   in-memory APIs for `ca`/`cert`/`key` PEM buffers. Prerequisite: add a
   direct `boring` dependency **version-matched to quiche's pin** (boring is
   only a transitive dep today). This task = host-target implementation +
   native unit test (Phase 1); **O2 is the same code verified under the
   wasm artifact (Phase 2)**. The TempFileGuard path stays for native under
   `os-runtime`. `effective_pmtud_ceiling` (kernel route query) → constant
   fallback 1472. Force `enable_pacing(false)` and no qlog in the wasm
   profile.
5. Keylog plumbing (mechanism for §6.4's keylog events): when
   `opts.keylog` is set, enable `config.log_keys()` and install an
   in-memory `Vec<u8>` writer via `Connection::set_keylog` at connect; the
   accumulated NSS-format lines are drained by the `take_keylog` ABI export
   (§5.3) each pump. No filesystem involved.
6. Unit tests: reuse the existing sans-IO packet pumps
   (`exchange_handshake_packets`/`exchange_h3_packets` in connection.rs
   tests) as `LockstepPair`-style tests exercising the direct-call surface
   on the host target (fast, no wasm needed).

Definition of done: native suites green; new direct-call unit tests green
under `cargo test --lib --no-default-features`; `cargo check
--no-default-features --features wasm-abi` green.

### A3. `crates/http3-wasm` — the extern-C ABI crate

New workspace member (add to `members` in root `Cargo.toml`), modeled on
`fuzz/`'s dependency pattern:

```toml
[package]
name = "http3-wasm"
publish = false
[lib]
crate-type = ["cdylib", "rlib"]        # rlib so unit/integration tests can link
[dependencies]
http3 = { path = "../..", default-features = false, features = ["wasm-abi"] }
serde_json = "1"
[dev-dependencies]
quiche = { workspace = true }          # hand-rolled server peer for lockstep tests
```

**ABI conventions:** all pointers are `u32` offsets into the module's own
exported linear memory. Strings/JSON cross as `(ptr, len)` UTF-8. Handles
are `u32` slab indices (0 is never a valid handle).

**Error contract (applies to every export):** negative return = failure with
a shared code enum: `-1` = again/blocked (retryable; for `stream_send` this
means backpressure, for `send_request` the fetched message contains
`StreamBlocked`), `-2` = protocol/config error (fetch message), `-3` =
invalid handle, `-4` = bad arguments. Messages are fetched via
`last_error(handle, buf, cap)`; **`handle = 0` reads a global last-error
slot**, which is where construction failures land (`h3c_new` returns 0 on
failure). Messages preserve the `[h3:CATEGORY|k=v]` structured prefix so
`lib/error-map.ts` keeps working.

**Buffer lifetime (normative):** the events/scratch buffers returned by the
core are valid **only until the next call on the same handle**. Therefore JS
must fully decode the event JSON and copy every `dataOff`/`dataLen` payload
into fresh Buffers **synchronously during the pump, before calling
`timeout_ms`/`next_send`/anything else on that handle**; only dispatch of
the already-decoded batch may be deferred (§6.4).

**`h3c_new` options (normative):** one JSON object, **camelCase field names
exactly matching the TS native-options object** built in `lib/client.ts` /
`lib/quic-client.ts` (e.g. `maxIdleTimeoutMs`, `initialMaxData`,
`rejectUnauthorized`, `enableDatagrams`, `alpn` for QUIC), plus:
`serverAddr` (`"ip:port"` / `"[v6]:port"`), `serverName`, `localAddr`,
`scidHex` (40 lowercase hex chars = 20 bytes from host RNG). Binary-valued
options cross as strings: `ca`/`cert`/`key` as UTF-8 PEM strings,
`sessionTicket` as base64. Fields **ignored on wasm** (documented, not
errors): `runtimeMode`, `fallbackPolicy`, `qlogDir`, `qlogLevel`, `keylog`
as a string path (boolean form supported via `take_keylog`).

**Exports (H3 client, prefix `h3c_`; raw QUIC client mirrors as `qc_` minus
`send_request`/`remote_settings`, plus `qc_open_stream`):**

| Export | Signature (conceptual) | Notes |
|---|---|---|
| `wasm_alloc` / `wasm_free` | `(size) -> ptr` / `(ptr, size)` | staging allocations by JS |
| `h3c_new` | `(opts_json_ptr, opts_len) -> handle \| 0` | opts = existing `JsClientOptions` fields + `serverAddr`, `serverName`, `scidHex` (20 bytes from JS), `localAddr` |
| `h3c_last_error` | `(handle, buf_ptr, cap) -> len` | UTF-8 message; preserves the `[h3:CATEGORY\|k=v]` structured prefix so `lib/error-map.ts` keeps working |
| `h3c_rx_buffer` | `(handle) -> ptr` | fixed 64 KiB RX staging region |
| `h3c_recv` | `(handle, len) -> status` | JS copied a datagram into rx_buffer; wraps `process_packet` (from/to are fixed = peer/local) |
| `h3c_tx_buffer` | `(handle) -> ptr` | fixed 64 KiB TX staging region |
| `h3c_next_send` | `(handle) -> len \| 0` | writes next outbound datagram into tx_buffer; loop until 0 (wraps `flush_sends`/`try_send_next`) |
| `h3c_timeout_ms` | `(handle) -> i64` | `-1` = no timer; wraps `conn.timeout()` |
| `h3c_on_timeout` | `(handle)` | wraps `process_timers(now)` |
| `h3c_drain_events` | `(handle, out_ptr_ptr) -> len` | serialized batch (see below) |
| `h3c_send_request` | `(handle, headers_json_ptr, len, fin) -> i64` | stream id, or negative (message contains `StreamBlocked` for flow-control block) |
| `h3c_stream_send` | `(handle, stream_id, ptr, len, fin) -> i64` | ≥0 = admitted bytes (FIN-only accept = 1); `-1` = backpressure; `-2` = error |
| `h3c_stream_close` | `(handle, stream_id, code) -> status` | |
| `h3c_send_datagram` | `(handle, ptr, len) -> status` | |
| `h3c_ping` | `(handle) -> status` | later emits `EVENT_PING_ACK` with `durationMs` |
| `h3c_session_metrics` | `(handle, out_ptr_ptr) -> len` | JSON, 8-field TS shape (`packetsIn…datagramQueueDepth`) |
| `h3c_remote_settings` | `(handle, out_ptr_ptr) -> len` | JSON `[{id,value}]` |
| `h3c_close` | `(handle, code, reason_ptr, len) -> status` | `quiche_conn.close`; subsequent pumps emit SESSION_CLOSE → SHUTDOWN_COMPLETE |
| `h3c_take_keylog` | `(handle, out_ptr_ptr) -> len \| 0` | drains accumulated NSS-format keylog lines (A2 task 5); 0 = none |
| `h3c_is_done` | `(handle) -> bool` | wraps `is_done()` + pending-TX-drained |
| `h3c_free` | `(handle)` | drop the session |

**Event serialization (v1 — pragmatic hybrid):** `drain_events` returns one
JSON array of event objects using the exact TS field names (`eventType`,
`streamId`, `headers`, `fin`, `meta`, `metrics`), except binary payloads:
`data` is replaced by `dataOff`/`dataLen` (offsets into linear memory, valid
until the next call on the handle; JS copies immediately into fresh
Buffers). Rationale: metadata volume is trivial, DATA bytes dominate and
never pass through JSON. If profiling later shows JSON parse cost matters,
swap in a packed binary record format behind the same TS decoder — an
encapsulated decision in `lib/wasm/events.ts`.

**Memory sizing:** link with `-C link-arg=--max-memory=67108864` (64 MiB) so
views can be safely cached against a bounded memory and the workerd 128 MB
isolate budget always has headroom. Buffer-pool size classes stay default;
they exist for native fragmentation and are harmless here (or bypass with
`Chunk::unpooled` if profiling says so).

Definition of done: `WebAssembly.Module.imports()` of the built artifact ⊆
the C2 allowlist; `WebAssembly.Module.exports()` matches the table; Rust
smoke tests (`cargo test -p http3-wasm`, host target) exercise the
slab/handle plumbing. Note the lockstep pumps named in A2 task 6 are
`#[cfg(test)]`-internal to the `http3` crate and unreachable here — the
http3-wasm tests hand-roll the server side of the pair directly with the
`quiche` dev-dependency (~30 lines: accept, recv/send loop), or the
`wasm_exports` module exposes a small test-helper pump.

### A4. quiche patch + upstream PR

The fork: quiche 0.29.2 declares `AES_ecb_encrypt` and `CRYPTO_chacha_20` as
`-> c_void` (src/crypto/boringssl.rs:364-372); wasm's typed linking rejects
the signature mismatch and traps at the first `encrypt_pkt`. Patch file:
`spikes/quiche-wasm-wasip1/quiche-0.29.2-wasm-ffi.patch`.

1. Create `currentspace/quiche` fork (branch `wasm-ffi-fix` off the 0.29.2
   tag) carrying the 2-line fix; reference via `[patch.crates-io]` in the
   **workspace** `Cargo.toml` (patch applies to all members; behavior on
   native is identical — the declarations were UB-adjacent everywhere).
2. Open the upstream PR to cloudflare/quiche immediately: "fix FFI decls
   returning c_void (1-byte enum) where C returns void; enables wasm targets,
   removes UB-adjacent declarations on all targets." Track in the repo;
   drop the patch when a release contains it.

### A5. BoringSSL-for-wasi build script

New script `scripts/build-bssl-wasi.sh` (invoked by `pnpm run build:wasm`
and CI), fully derived from the proven spike recipe:

```bash
# Inputs: WASI_SDK_PATH (wasi-sdk 33), cargo registry path for boring-sys
# Output: target/bssl-wasi/<boring-sys-version>/{lib/libcrypto.a,lib/libssl.a}
BSSL_SRC="$(dirname "$(cargo metadata ... boring-sys)")/deps/boringssl"   # ABI match with bindings; ships pre-generated err_data.c (no Go)
cp -R "$BSSL_SRC" "$WORK/src"
# Drop socket BIOs: no netdb.h in the wasip1 sysroot; nothing quiche needs references them
sed -i '' -e '/crypto\/bio\/connect\.c/d' -e '/crypto\/bio\/socket\.c/d' \
          -e '/crypto\/bio\/socket_helper\.c/d' "$WORK/src/CMakeLists.txt"
W='-DOPENSSL_NO_THREADS_CORRUPT_MEMORY_AND_LEAK_SECRETS_IF_THREADED
   -DFREEBSD_GETRANDOM -DGRND_NONBLOCK=0 -DSO_KEEPALIVE=0 -DSO_ERROR=0
   -include <repo>/scripts/bssl-wasi-shim.h'   # getrandom→getentropy; socket/setsockopt/connect → -1
cmake -G Ninja -B "$WORK/build" -S "$WORK/src" \
  -DCMAKE_TOOLCHAIN_FILE="$WASI_SDK_PATH/share/cmake/wasi-sdk-p1.cmake" \
  -DCMAKE_BUILD_TYPE=Release -DOPENSSL_NO_ASM=1 \
  -DCMAKE_C_FLAGS="$W" -DCMAKE_CXX_FLAGS="$W"
ninja -C "$WORK/build" crypto ssl && stage libs
```

Cargo/bindgen environment for the wasm build (napi convention says env vars
live in `.cargo/config.toml [env]`, but `WASI_SDK_PATH` is machine-specific —
set the derived, path-dependent vars in the build script/CI step and document
in CLAUDE.md):

```
BORING_BSSL_PATH_wasm32_wasip1=<staged prebuilt dir>
BORING_BSSL_INCLUDE_PATH_wasm32_wasip1=<boring-sys crate>/deps/boringssl/src/include
BORING_BSSL_SYSROOT_wasm32_wasip1=$WASI_SDK_PATH/share/wasi-sysroot
BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1="--target=wasm32-wasip1 --sysroot=$SYSROOT \
    -isystem $SYSROOT/include/wasm32-wasip1 -fvisibility=default"
```

`-fvisibility=default` is **MANDATORY** — without it bindgen silently emits
zero functions for wasm32 targets (rust-bindgen #1681) and the build fails
with `E0425 cannot find function CRYPTO_library_init` much later.

Linker flags: **cargo does not interpolate env vars in `config.toml`
values**, and the sysroot path is machine-specific — so the build script
sets them itself, paths pre-expanded, via
`CARGO_TARGET_WASM32_WASIP1_RUSTFLAGS` (or `cargo --config` overrides):

```
-L native=$WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip1/noeh   # wasi-sdk 33 keeps libc++ here
-L native=$WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip1
-C link-arg=-lc++ -C link-arg=-lc++abi        # boring-sys omits the C++ runtime link line on non-unix targets
-C link-arg=--max-memory=67108864
```

(Shell portability: the script must run on macOS and Linux CI — use
`sed -i.bak … && rm` or a small node one-liner for in-place edits, never
BSD-only `sed -i ''`.)

Caching/rebuild policy: stage libs under a directory keyed by the boring-sys
version from `Cargo.lock`; CI caches on that key; a lockfile bump of
boring-sys invalidates and rebuilds (~minutes with ninja). When quiche bumps
boring, re-run and fix define drift (jedisct1/boringssl-wasm is the
maintained reference for the define set).

Pin `wasi-sdk 33` (download URL per-platform in the script; macOS arm64 for
dev, x86_64-linux in CI).

---

## 6. Workstream B — TypeScript

### 6.1 Mandatory refactors (small, non-breaking; land first)

1. **Extract `ClientEventLoopLike`** (H3) and widen the existing
   `QuicClientEventLoopLike`: `Http3ClientSessionBase._eventLoop`
   (session.ts:472), `ClientHttp3Stream._eventLoop` (stream.ts:490), and
   `QuicClientSession._eventLoop` (quic-client.ts:174) are typed as the
   concrete classes, which have private fields — TS will not admit a
   structurally identical substitute. Retype to interfaces.
2. **Lazy native binding load**: `lib/event-loop.ts:486` does
   `const binding = require(findBinding())` at module import time (node:fs +
   __dirname). Wrap in a memoized `getBinding()`; callers import the
   function. Without this, `import '@currentspace/http3'` crashes in any
   non-Node runtime even if only the wasm path is used.
3. **Split `ServerHttp2StreamAdapter` out of `lib/stream.ts`** (it pulls a
   runtime `node:http2` import into the client bundle) into e.g.
   `lib/stream-h2-adapter.ts`.
4. **Injectable DNS**: `endpoint.ts` uses `node:dns/promises`. Add an
   optional resolver hook; IP-literal endpoints already bypass DNS. (The
   workerd adapter will pass hostnames through to the platform.)
5. **Keylog isolation** (the Phase-1 part): ensure the wasm runtime path
   never imports `lib/keylog.ts` (fs/os). The event-based delivery itself —
   `take_keylog` drained each pump and emitted as session `'keylog'` events
   (option stays `boolean`; a string path remains Node-native-only) — is
   Phase 3 work (§6.4) since it depends on the A2 task 5 / §5.3 ABI.

Definition of done: `pnpm run lint && pnpm run typecheck && pnpm test` green
with zero behavior change; native path untouched.

### 6.2 WASI shim — `lib/wasm/wasi-shim.ts`

One host-agnostic module (works in Node, workerd, browsers):

```ts
export interface ShimOptions {
  nowNs?: () => bigint;              // injectable mock clock for tests
  random?: (buf: Uint8Array) => void;
  onStderr?: (text: string) => void; // panic messages / debug logs
}
export function makePreview1(opts, getMemory: () => WebAssembly.Memory): Record<string, Function>
```

Implements: `random_get` (crypto.getRandomValues / randomFillSync),
`clock_time_get` (MONOTONIC → `process.hrtime.bigint()` in Node /
`performance.now()*1e6` elsewhere; REALTIME → Date.now-anchored),
`environ_sizes_get`/`environ_get` (empty), `fd_write` (fd 1/2 → onStderr),
`proc_exit` (throw), plus defensive ENOSYS/no-op stubs for
`fd_close`, `fd_fdstat_get`, `fd_fdstat_set_flags`, `fd_prestat_get`,
`fd_prestat_dir_name`, `fd_read`, `fd_seek`, `path_filestat_get`,
`path_open`, `sched_yield`. This list is a **superset** of the module's
actual 15-import surface (C2 is the normative allowlist; `sched_yield` came
from the threads experiment and should not appear in the artifact's
imports). Base it on `spikes/quiche-wasm-wasip1/mini-shim.mjs`, which
implements only 10 of these — Phase 0 / O5 extends it before the packet
pump can run.

### 6.3 Loader — `lib/wasm/core-loader.ts`

```ts
export interface WasmCoreSource { module?: WebAssembly.Module; bytes?: Uint8Array; }
export function loadHttp3WasmCore(src: WasmCoreSource, opts?: ShimOptions): Http3WasmCore
```

- Node path: `fs.readFileSync` + `new WebAssembly.Module` **cached at module
  scope** (sync compile measured 0.17 ms for 66 KB, 25 ms for a 30 MB
  module — keeps the sync `connectQuic()` shape viable). Do NOT rely on
  instance-phase `.wasm` ESM imports (unflagged only ≥ Node 24.5; engines
  say ≥ 24.0; they can't satisfy preview1 imports anyway).
- workerd path: caller passes a precompiled `WebAssembly.Module` (wrangler
  bundles `.wasm` imports as modules; `instantiateStreaming`/compile-from-
  bytes are unsupported there).
- Asserts the export table and wraps it in a typed façade `Http3WasmCore`
  (thin, mechanical: memory views with the detach guard, JSON encode/decode,
  `dataOff/dataLen` copy-out in `lib/wasm/events.ts`).

### 6.4 `WasmClientEventLoop` — `lib/wasm/client-event-loop.ts`

Implements `ClientEventLoopLike` (H3) and the QUIC variant, over
`Http3WasmCore` + a `DatagramTransport`. Behavioral spec (all verified
against the current TS contract):

- `connect(serverAddr, serverName)`: resolve transport, generate 20-byte
  SCID via host RNG, `h3c_new`, initial pump (emits the ClientHello
  datagrams). Async failures surface as events, not throws.
- Pump after **every** input: `while ((n = next_send()) > 0) transport.send`
  → `drain_events` + **synchronously decode the JSON and copy every
  `dataOff`/`dataLen` payload into fresh Buffers** (the core's buffers are
  invalidated by the next call on the handle — §5.3) → `take_keylog` if
  enabled → `queueMicrotask(dispatch of the already-decoded batch)` →
  `timeout_ms` → re-arm the single timer (>1 ms delta only). Never dispatch
  events synchronously inside a command call.
- `streamSend(...)`: map core result to `{status, written}` /
  admitted-bytes per the existing `streamSendOutcomeBytes` convention;
  preserve the FIN-only `written=1` sentinel.
- `close(code, reason)` — normative teardown order (an open dgram socket or
  live timer keeps the Node event loop alive; this is the likeliest way
  Phase 3 tests wedge): idempotent; `h3c_close` → pump until `h3c_is_done`
  or a bounded deadline → dispatch `EVENT_SHUTDOWN_COMPLETE` →
  `clearTimeout` the armed timer → `h3c_free(handle)` → `await
  transport.close()` → resolve. Subsequent calls resolve immediately. Must
  beat the TS 5 s `SHUTDOWN_TIMEOUT_MS` fallback.
- Binding-compat surface: `ackEventBatch(count)` no-op, `requestShutdown()`,
  `joinWorker()` no-op, `localAddress()` from the transport,
  `getQlogPath() → null`.
- Metrics: 8-field TS shape (omit native's extra `pmtu` field).
- Errors: preserve `[h3:CATEGORY|k=v]` prefixes; `StreamBlocked` substring
  on blocked `sendRequest`.

### 6.5 `DatagramTransport` + Node adapter

```ts
export interface DatagramTransport {
  send(datagram: Uint8Array): void;          // connected flow — one fixed peer
  onMessage(cb: (datagram: Uint8Array) => void): void;
  localAddress(): { address: string; family: string; port: number };
  close(): Promise<void>;
}
export function connectNodeUdp(host: string, port: number, opts?): Promise<DatagramTransport>
```

Node adapter (`lib/wasm/node-udp-adapter.ts`): `dgram.createSocket({ type,
recvBufferSize: 4–8 MB })` + `socket.connect(port, host)`; **mandatory
`'error'` handler** that swallows `ECONNREFUSED` (ICMP port-unreachable
surfaces async on connected sockets; unhandled it crashes the process —
quiche's idle timeout is the real failure signal). Assert
`getRecvBufferSize()` after bind in harness setup (Linux `rmem_max` may
silently clamp it). No GSO/GRO/recvmmsg exists in Node's dgram — one
syscall per datagram is the accepted ceiling.

The future workerd adapter implements the same interface over whatever ships
from workerd#4463 (`connectDatagram({hostname, port})`-shaped, per the
discussion), **with a capability probe that round-trips a real packet** —
today's workerd `node:dgram` stub silently drops sends, so module presence
must never be trusted.

### 6.6 Runtime-mode integration

- Add `'wasm'` to `RuntimeMode`/`RuntimeDriver` unions in `lib/runtime.ts`;
  early branch in `runWithRuntimeSelection` (mirror `'portable'`), reason
  code `'requested-wasm'`. **Never pass `'wasm'` to native** —
  `TransportRuntimeMode::parse` rejects it.
- In `connect()` (client.ts) and `connectQuic()` (quic-client.ts): factory
  `createClientEventLoop(mode, options, onEvents)` branching before `new
  binding.NativeWorkerClient(...)`. The factory lives in a named file
  (`lib/client-event-loop-factory.ts`) that **lazily dynamic-imports**
  `lib/wasm/` so native-only consumers never load the wasm loader.
- The wasm runtime path lives under `lib/wasm/` and must not import
  `lib/event-loop.ts`'s binding loader, `lib/keylog.ts`, or anything pulling
  `node:fs`/`node:http2`/`node:dns`. **Explicit B6 task:** add an ESLint
  `no-restricted-imports` block for `lib/wasm/**` forbidding
  `./event-loop.js`, `./keylog.js`, `node:fs`, `node:http2`, `node:dns`.
  Also in the wasm path: feature-detect `unref()` on timers and avoid bare
  `setImmediate` (use `setTimeout(0)` fallback) — workerd's nodejs_compat
  coverage for these is unverified (§9 C15).
- workerd entry (Phase 5): `lib/wasm/index.workerd.ts` compiled by a second
  tsconfig **without** `types: ["node"]` (TS 6 needs explicit types; use
  `@cloudflare/workers-types` or WebWorker lib). Register every new tsconfig
  in `eslint.config.mjs parserOptions.project` (typed linting hard-fails
  otherwise).

---

## 7. Workstream C — Testing

Budgets (project law): ≤15 s per test, <1 min per suite, full `pnpm test`
<2 min. The mock clock (§4.7) keeps timeout tests wall-clock-free.

1. **C1 Rust lockstep tests** (host target, no wasm): drive the direct-call
   handler surface with the existing sans-IO packet pumps. Lane:
   `cargo test --lib --no-default-features`.
2. **C2 Import/export allowlist** (`test/wasm/artifact-shape.test.ts`):
   `WebAssembly.Module.imports(mod)` ⊆ the allowlist; exports match §5.3.
   Fails on any dependency bump that grows the syscall surface. The
   normative allowlist is the 15 names the spike enumerated — re-verified
   and frozen during Phase 0 (O5) via
   `spikes/quiche-wasm-wasip1/inspect-imports.mjs`:
   `random_get, clock_time_get, environ_get, environ_sizes_get, proc_exit,
   fd_close, fd_fdstat_get, fd_fdstat_set_flags, fd_prestat_get,
   fd_prestat_dir_name, fd_read, fd_seek, fd_write, path_filestat_get,
   path_open` (no `sched_yield` — that came from the threads experiment).
3. **C3 Node loopback, wasm client ↔ native server**: extend
   `test/support/native-test-helpers.ts` with `createWasmH3Pair()` /
   `createWasmQuicPair()` — native server on `127.0.0.1` ephemeral
   (`runtimeMode: 'portable'`, `disableRetry: true`, self-signed certs with
   `ca` passed to the client once O2 lands; `rejectUnauthorized: false`
   until then), wasm client over the Node UDP adapter. `EventCollector` is
   transport-agnostic and reused verbatim.
   H3 coverage: handshake, GET w/ response body, request body upload w/
   backpressure (STREAM_BLOCKED → DRAIN), trailers, GOAWAY, RESET,
   datagrams, ping, session ticket, close sentinel timing, FIN-only write.
   **Raw-QUIC coverage (same Phase 3 task, mirroring
   `test/interop/quic-loopback.test.ts`):** `connectQuic`/`connectQuicAsync`
   handshake, `openStream()` bidi echo, server-initiated stream surfacing
   as `EVENT_NEW_STREAM` → `'stream'` event, QuicStream blocked → drain,
   FIN handling, client mTLS (`cert`/`key`), datagrams, close semantics.
4. **C4 Public-API interop reuse**: run `test/interop/h3-loopback.test.ts`
   and the QUIC loopback suite with the **client** on `runtimeMode: 'wasm'`.
   Mechanism (explicit Phase 4 task — the suites are not parameterized
   today, and server options must stay native): add
   `clientRuntimeMode(): RuntimeMode | undefined` to `test/support` —
   returns `'wasm'` when `HTTP3_WASM=1`, else `undefined` — and thread it
   into client-side `connect()`/`connectQuic()` options only; server
   options keep `runtimeMode: 'portable'`. Gate: `HTTP3_WASM=1` env
   (pattern copied from `HTTP3_LONGHAUL`) until CI always builds the
   artifact, then drop the gate.
5. **C5 Deterministic timer tests**: mock `nowNs` in the shim; fast-forward
   idle timeout and PTO; assert `EVENT_SESSION_CLOSE` w/ idle-timeout detail
   and retransmission behavior without waiting wall-clock.
6. **C6 Frozen-clock simulation**: a test driver whose clock advances only
   on I/O events (workerd semantics) — handshake + GET must still complete.
   This validates workerd viability years before workerd UDP exists.
7. **C7 workerd smoke (Phase 5)**: `workerd --experimental` / miniflare
   loads the module + shim + a mock in-memory transport; asserts
   instantiation, exports resolution, and a lockstep handshake against
   pre-recorded server flights (or a loopback relay). Not in the default
   lane; runs in CI weekly + on wasm-touching PRs.
8. **New lane wiring** (lands in Phase 2 with D1a — the Phase 2/3 gates
   depend on it): `test/wasm/**` compiled by `tsconfig.test.json` as usual;
   script `pnpm run test:wasm` following the existing
   `--test-isolation=none --test-timeout=15000` conventions; added to
   `pnpm test` once ungated. **Artifact provenance:** tests load
   `dist/wasm/http3_client.wasm` exclusively (the copy `build:wasm`
   produces — never `target/wasm32-wasip1/release/*.wasm` directly). When
   `HTTP3_WASM` is unset or the artifact file is missing, suites
   **self-skip** with a message (node:test `skip`), never fail.

---

## 8. Workstream D — Build, CI, packaging

### D1a Scripts — lands in **Phase 2** (the Phase 2/3 gates invoke these)

- `build:bssl-wasi` → `scripts/build-bssl-wasi.sh` (§A5; cached).
- `build:wasm` → bssl step, then
  `cargo build -p http3-wasm --target wasm32-wasip1 --release`, then
  `wasm-opt -Oz` (binaryen), copy to `dist/wasm/http3_client.wasm`.
  Post-opt/gzip size is **unmeasured** — the only real figure is 1.53 MB
  pre-opt; conservative planning bound is 1–2 MB gzipped (still far under
  the 10 MB paid Workers limit). Measure in Phase 2 and record in the doc.
- `test:wasm` per §C8 (lane wiring is also Phase 2).

### D1b Verify/release integration — lands in **Phase 4**

- `verify.sh`: add a wasm build+test step between the napi build and
  `pnpm test`, gated on **both** `VERIFY_SKIP_WASM` (existing
  `VERIFY_SKIP_*` convention) and `WASI_SDK_PATH` being set — the step
  self-skips with a notice when the toolchain is absent. The verify.yml
  lanes (ubuntu × 3 Node, macOS, Dockerfile.verify) do NOT install
  wasi-sdk; **the dedicated D2 wasm job is the CI enforcement point.**
  (Installing wasi-sdk into every verify lane is possible later but is not
  part of this plan.)

### D2 CI (`.github/workflows/ci.yml`)

New `wasm` job: dtolnay/rust-toolchain@stable with
`targets: wasm32-wasip1`; `Swatinem/rust-cache@v2` (ci.yml currently has no
cargo cache — this job must have it; the bssl+quiche cold build is the long
pole); a pinned wasi-sdk-33 download step exporting `WASI_SDK_PATH`; binaryen
install; `pnpm run build:wasm`; `HTTP3_WASM=1 pnpm run test:wasm`. Bssl
staging cached keyed on the boring-sys version from `Cargo.lock` + wasi-sdk
version. Budget: well under the existing 45–60 min timeouts once cached.

Also: extend `scripts/check-node-api-boundary.mjs` `scanEntries` to include
`crates/http3-wasm/src` (it currently scans only `src/` + `build.rs`; the
check is release-blocking and the new crate must stay NAPI-free by
definition).

### D3 Packaging (v1 decision: ship in the main package)

- `dist/wasm/` (loader, shim, event decoder, adapters, wasm runtime TS) +
  `dist/wasm/http3_client.wasm` (~1–1.5 MB raw after wasm-opt) ship in the
  **main** `@currentspace/http3` tarball. Rationale: platform-independent,
  avoids a 4th optionalDependencies sidecar (each release already requires
  hand-bumping pnpm-lock importer pins per sidecar — see
  `docs/RELEASE_RUNBOOK.md` and the release-prep lockfile gotcha), and the
  napi-rs-style `-wasm32-wasi` sidecar layout is exactly what fails to
  resolve on Cloudflare Pages (napi-rs/node-rs#862). Revisit a sidecar only
  if tarball size becomes a complaint.
- `package.json`: add `"./wasm"` subpath export:
  `{ "types": …, "workerd": "./dist/wasm/index.workerd.js", "worker": …,
  "default": "./dist/wasm/index.js" }` (condition order
  `workerd`→`worker`→`browser` per esbuild docs — **verified against real
  wrangler in Phase 5**; keep the map clean — top-level `main`/`browser`
  shadowing bugs are a known wrangler/esbuild class, workers-sdk#2805).
  Root `.` export unchanged (Node native path).
- `files` whitelist: add `dist/wasm` (covered by `dist`? — `files` already
  lists `dist`; verify the wasm binary isn't excluded by `.npmignore`-like
  rules and that `scripts/publish-npm-release.mjs assertRootPackageLayout`
  gains the new required entries so packaging regressions fail the publish).
- `release.yml`: wasm build job uploads the artifact for the pack step; a
  validate lane runs `test:wasm` against the packed tarball.
- Do NOT touch the napi loader (`index.js`) or its WASI fallback branch —
  the wasm runtime is reached via `runtimeMode`/`./wasm`, not the napi
  loader chain. (The existing generated `forceWasi` branch in index.js
  refers to napi-rs's own wasm sidecar mechanism, which we are not using.)

### D4 Docs

- `docs/WASM_RUNTIME.md`: usage (Node + future workerd), options matrix,
  limitations (N3–N6), workerd constraints table (§9).
- CLAUDE.md: add `build:wasm`/`test:wasm` commands, `WASI_SDK_PATH` note,
  and "rebuild wasm after Rust changes" rule.
- `docs/SUPPORT_MATRIX.md`: add the wasm runtime row.

---

## 9. workerd readiness (Workstream E — design now, deploy later)

Verified constraints the design already satisfies (E-tasks only document and
smoke-test them):

| # | Constraint (verified 2026-07) | Design consequence |
|---|---|---|
| C1 | No outbound UDP today; `node:dgram` is a silent no-op stub; no CF commitment on #4463 | Node adapter ships first; workerd adapter behind `DatagramTransport` + packet round-trip capability probe |
| C2 | Likely future shape: proxied, hostname-based, `cloudflare:sockets`-like (per jasnell on #6451: runtime itself won't speak QUIC) | Adapter never assumes local addr control, raw IPs, ECN/TOS, GSO, or PMTUD |
| C3 | Clock frozen during execution; advances per I/O; `Date.now()` inside `setTimeout` cb == exact scheduled time | Single-armed-timer pump is correct; pacing disabled; RTT tolerances ≥ a few ms |
| C4 | Timers only in request/DO context; ms granularity | One `setTimeout` per session, re-armed per pump |
| C5 | No threads / SharedArrayBuffer / wasm atomics | wasip1 single-threaded build (done) |
| C6 | 128 MB isolate incl. wasm memory | `--max-memory=64MiB` link cap; bounded pools |
| C7 | CPU 10 ms/req free, 30 s paid | Target paid plan; handshake crypto measured in Phase 0 (O4) |
| C8 | Bundle 3 MB free / 10 MB paid (gzip); 1 s startup CPU | conservatively ≤ 1–2 MB gzip (unmeasured; Phase 2 records it) — fits paid comfortably; measure `startup_time_ms`; lazy-instantiate if needed |
| C9 | No sockets in global scope; no cross-request sharing; waitUntil +30 s | Sessions created per-request/per-DO; document |
| C10 | DO: outbound conn pins ≤15 min; eviction after 70–140 s idle; hibernation discards wasm memory | Connections are ephemeral; keep `maxIdleTimeoutMs` ≤ 60 s in Workers profile; persist session tickets for 1-RTT/0-RTT reconnect |
| C11 | Max 6 simultaneous outbound connections in the establishment phase per invocation (UDP accounting undefined — no headers phase) | Fine for a client; don't dial many QUIC connections in parallel |
| C12 | Cloudflare IP ranges blocked for outbound (TCP today; assume UDP same) | Document loudly: a Workers-hosted client likely cannot reach Cloudflare-fronted H3 origins |
| C13 | No functional WASI in workerd | Self-supplied shim (done by design) |
| C14 | No filesystem / trust store | CA PEM via options (in-memory boring loading); optionally embed a root bundle later |
| C15 | nodejs_compat runtime fidelity for `node:stream` Duplex, `node:events`, `Buffer`, `setImmediate`, `process.nextTick`/`emitWarning`, timer `unref()` — **UNVERIFIED** (no dossier confirms it; the client TS path needs exactly these) | E-task/C7 extension must drive the public TS API (connect → request → Duplex write/drain → close) under workerd, not just instantiate the module; wasm path code guards: feature-detect `unref`, avoid bare `setImmediate` (§6.6) |

E-tasks: `index.workerd.ts` entry + tsconfig (§6.6), exports condition
(§D3), C7 smoke harness (§C7) **extended to drive the public TS API under
workerd per C15**, a sample worker fixture (`examples/workerd-client/` with
`wrangler.toml`; add `workerd`/`miniflare` and `wrangler` as
devDependencies — the Phase 5 gate references this fixture),
`docs/WASM_RUNTIME.md` workerd section, and a tracking issue that watches
workerd#4463 / a `connectDatagram` API to slot in the real adapter.

---

## 10. Risks and mitigations

| # | Risk | Likelihood / impact | Mitigation |
|---|---|---|---|
| R1 | boring-sys/BoringSSL bump breaks the wasi recipe (defines drift, C++-ification of libcrypto) | Med / Med | Staged-lib build keyed on boring-sys version; jedisct1/boringssl-wasm tracked as reference; recipe fully scripted + CI'd so breakage is visible at bump time, not release time. Strategic escape hatch: quinn-proto fallback (§0) |
| R2 | Upstream quiche rejects the FFI patch | Low / Low | Patch is trivially correct on all targets; carrying the 2-line fork indefinitely is cheap (`[patch.crates-io]` on a tagged fork) |
| R3 | bindgen/libclang variance on Linux CI (spike verified macOS only) | Med / Low | Phase 2 CI job proves it; `-fvisibility=default` is the known fix; pin libclang via `LIBCLANG_PATH` if needed |
| R4 | `OPENSSL_NO_ASM` crypto too slow for real use | High / Low (client control-plane) | Set expectations (N3); prefer ChaCha20-Poly1305 cipher-suite ordering for 1-RTT; try `-msimd128` in CFLAGS as a cheap win; benchmark in Phase 0 (O4) |
| R5 | `EVENT_WRITE_READY` never emitted breaks some backpressure path | Low / Med | Explicit C3 test coverage of `_final`/drain flows; contingency: emit 18 alongside 8 |
| R6 | Event JSON serialization too slow at high event rates | Low / Low | `dataOff/dataLen` keeps payload bytes out of JSON; decoder is encapsulated — swap to packed binary if profiling demands |
| R7 | workerd UDP never ships | Med / — | The Node deliverables (conformance harness, no-prebuilt fallback, deterministic test rig) justify the work standalone; core is runtime-portable by construction |
| R8 | Frozen-clock semantics degrade loss recovery in workerd | Med / Med | C6 frozen-clock simulation in CI now; pacing disabled; PTO/idle behavior asserted under event-advanced clocks |
| R9 | wasm artifact bloats the npm tarball | Low / Low | wasm-opt -Oz + gzip, conservatively ≤ 1–2 MB (unmeasured — Phase 2 records the real number); sidecar package remains a documented fallback plan (§D3) |
| R10 | Second protocol front-end drifts from native | Med / Med | Same Rust handlers compiled to both targets (that is the point); shared TS dispatch; C4 runs the same public-API suites against both `runtimeMode`s |

---

## 11. Phases, sequencing, acceptance gates

Dependencies: P0 → P1 → P2 → P3 → P4; P5 after P4. A4 (upstream PR) can
start immediately. Within phases, tasks are parallelizable by different
agents where files don't overlap.

### Phase 0 — Close the spike (go/no-go, ~days)

Rebuild the spike from `spikes/quiche-wasm-wasip1/` with a local wasi-sdk 33
and the patch; extend `mini-shim.mjs` with the six missing preview1 stubs
and run the module under the hand shim, not `node:wasi` (O5); freeze the
import allowlist for C2; then drive a **full handshake + one H3 GET**
through the wasm module against this repo's **native H3 server** over a
minimal JS packet pump (no TS-layer integration; raw `node:dgram` + the
extended shim). Measure handshake CPU + module size (O1, O4).
**Gate:** H3 response body received through the wasm module via the hand
shim; import allowlist recorded in §C2. If cert verification via
`with_boring_ssl_ctx_builder` (O2) resists quickly, defer it to Phase 2 and
gate with `verify_peer(false)` — but it must land before any release (its
verification is in the Phase 4 gate).

### Phase 1 — Seams (pure refactors, native-neutral)

A1 (Cargo hygiene, os-runtime feature, datagram value-type lift) + A2
(direct-call surfaces) + B1 (TS refactors §6.1).
**Gate:** `pnpm verify` green; `cargo test --lib --no-default-features`
green; zero native behavior change (existing suites are the proof).

### Phase 2 — The wasm artifact

A5 (bssl script) + A3 (`crates/http3-wasm` ABI) + A4 fork wiring + O2/O3 +
**D1a (build:bssl-wasi / build:wasm / test:wasm scripts) + C8 lane wiring**
(the gates below invoke them). D2's CI job lands here (build + C2 allowlist
test only). Record the measured post-wasm-opt/gzip size (O4) in this doc.
**Gate:** CI (Linux) builds `dist/wasm/http3_client.wasm` via
`pnpm run build:wasm`; C2 passes; Rust host-target tests of the handle
plumbing pass.

**Status: DONE (2026-07-08, macOS arm64, local — Linux CI verification per
D2/O3 remains open).** All A3/A4/A5/D1a/C2 deliverables landed and green:

- `crates/http3-wasm` implements the full `h3c_*`/`qc_*` extern-C table
  from §5.3 (39 exports + `wasm_alloc`/`wasm_free` = 42 total incl.
  `memory`); `cargo test -p http3-wasm` is 17/17 green on the host target,
  including a real lockstep integration test
  (`tests/h3_lockstep.rs`) that drives `H3ClientHandler::new_direct`
  through a full QUIC handshake + HTTP/3 GET against a hand-rolled quiche
  server over real loopback UDP — no wasm target needed for that proof.
- **O1 (full handshake) closed for real**, beyond the host-target proof
  above: `spikes/quiche-wasm-wasip1/validate-handshake.mjs` drives the
  actual compiled `dist/wasm/http3_client.wasm` artifact's `h3c_*` ABI
  (via the extended `mini-shim.mjs`, no `node:wasi`) over a raw
  `node:dgram` socket against this repo's real native H3 server
  in-process, and completes a full QUIC+TLS handshake and an HTTP/3
  GET/response entirely inside the wasm module. Repeatable, exit 0.
- **O4 (size), measured for real** (this crate compiled with default
  workspace profile settings, i.e. `debug = 1` line tables still on):
  pre-opt 10.0 MiB (vs. the tiny spike's 1.53 MiB — expected, this build
  includes the full `H3ClientHandler`/`QuicClientHandler`/config/event
  machinery, not just a `quiche::connect()` smoke test); `wasm-opt -Oz
  --strip-debug` → **1.47 MiB** (stripping debug info alone accounts for
  most of the reduction: 10.0 MiB → 1.7 MiB from `--strip-debug`, → 1.47
  MiB adding `-Oz`); **gzip -9 → ~618 KiB**. Comfortably inside the "1–2
  MiB gzipped" planning bound and the 10 MB paid-Workers bundle limit
  (§9 C8).
- **O5 (import allowlist), re-derived empirically and frozen** in
  `test/wasm/artifact-shape.test.ts` — **12 names**, not the spike's
  15-name candidate list: `random_get`, `environ_get`,
  `environ_sizes_get`, `clock_time_get`, `fd_close`, `fd_prestat_get`,
  `fd_prestat_dir_name`, `fd_read`, `fd_seek`, `fd_write`, `proc_exit`,
  `sched_yield`. Two real, explainable deltas from the candidate list:
  `sched_yield` **is** needed (`crossbeam-channel`'s spin-wait backoff
  calls `thread::yield_now()` even single-threaded — a real, permanent
  dependency, not a threads-experiment leftover); `fd_fdstat_get` /
  `fd_fdstat_set_flags` / `path_filestat_get` / `path_open` are **not**
  needed (this build excludes `os-runtime` and `qlog-files` entirely, so
  no filesystem-touching code path exists to reference them — a good
  sign the A1 feature-gating actually worked). `mini-shim.mjs` extended
  accordingly (only 3 new stubs needed in practice: `fd_prestat_get`,
  `fd_prestat_dir_name`, `fd_read` — not 6).
- **A4 landed as scoped, not static, patching** — see the decision log
  (§12) for why: `[patch.crates-io]` in the root `Cargo.toml` would break
  `cargo build --release` on a fresh clone before anyone has run the wasm
  scripts. Verified explicitly both orders (patch-dir absent vs. present)
  against a real `cargo build --release`; `git diff Cargo.lock` shows only
  the expected, permanent `http3-wasm` package addition in both cases.
- Not yet done (later phases per the plan): Linux/CI bindgen verification
  (O3, D2), packaging (D3), `verify.sh` wiring (D1b), upstreaming the
  quiche patch as a real PR (A4 task 2 — needs the repo owner's own GitHub
  account, out of scope for automation).

### Phase 3 — TS wasm runtime

B2–B6 (shim, loader, WasmClientEventLoop ×2 incl. keylog-event delivery,
Node UDP adapter, runtimeMode 'wasm', factory file, ESLint zone).
**Gate:** C3 loopback suites (H3 + raw QUIC lists) green under
`HTTP3_WASM=1`; C5 deterministic timer tests green; lint/typecheck clean
with the new eslint project entries.

**Status: DONE (2026-07-08, macOS arm64, local).** All B2–B6 deliverables
landed and green:

- `lib/wasm/wasi-shim.ts` (`makePreview1Imports`), `lib/wasm/core-loader.ts`
  (`loadHttp3WasmCore` / `loadHttp3WasmCoreFromFile`, `Http3WasmCore` typed
  facade + memory-view helpers with the grow-detach guard),
  `lib/wasm/events.ts` (`decodeEventBatch`/`drainKeylog`),
  `lib/wasm/wasm-options.ts` (shared options/SCID/addr helpers — centralizes
  the `allow0rtt` vs native `allow0Rtt` naming gotcha),
  `lib/wasm/datagram-transport.ts` + `lib/wasm/node-udp-adapter.ts`,
  `lib/wasm/h3-client-event-loop.ts` + `lib/wasm/quic-client-event-loop.ts`
  (`WasmH3ClientEventLoop` / `WasmQuicClientEventLoop`, structurally —not
  nominally— implementing `ClientEventLoopLike`/`QuicClientEventLoopLike` so
  neither file needs to import `lib/event-loop.ts`), `lib/client-event-loop-factory.ts`
  (`createClientEventLoop` mode-branching + `resolveWasmArtifactPath`), the
  `lib/wasm/**` ESLint `no-restricted-imports` zone (`eslint.config.mjs`),
  and `'wasm'` added to `RuntimeMode`/`RuntimeDriver`/`RuntimeReasonCode`
  plus an early branch in `runWithRuntimeSelection` (`lib/runtime.ts`).
- C3 (`test/support/wasm-test-helpers.ts` + `test/wasm/h3-loopback.test.ts` +
  `test/wasm/quic-loopback.test.ts`) and C5
  (`test/wasm/deterministic-clock.test.ts`) all green: 20/20 new tests,
  `HTTP3_WASM=1 pnpm run test:wasm` (24 incl. C2) completes in well under
  1 s; full `pnpm test` (native suite) unaffected (362/362, one pre-existing
  unrelated skip).
- **Two genuine Phase 2 ABI gaps found and fixed** (smallest-possible Rust
  change, per this phase's deviation allowance — full rationale inline as
  code comments at each site): `H3ClientHandler::try_send_next` /
  `QuicClientHandler::try_send_next` (`src/worker.rs`, `src/quic_worker.rs`)
  now call the already-existing (but previously close()-path-unreachable)
  `refresh_timeout_deadline()` when a flush cycle has nothing left to send —
  mirroring what `ProtocolHandler::flush_sends` already does for the native
  reactor loop. Without this, the direct-call surface's cached
  `timer_deadline` stayed whatever it was *before* a `h3c_close`/`qc_close`
  request (typically the idle timeout, tens of seconds out) until it
  happened to naturally elapse, since `process_timers_for_handle` only
  refreshes it *after* confirming the stale value is already due — a
  structural gap invisible to native (whose reactor loop refreshes every
  session unconditionally after every command) but fatal to the direct-call
  `close()` sequence's "prompt, sentinel-driven" contract. Caught by this
  phase's own `close()`-timing test going from ~2000 ms (hitting
  `CLOSE_DRAIN_DEADLINE_MS`) to ~40–90 ms once fixed; confirmed via a
  standalone timing diagnostic and a full Rust suite re-run
  (`cargo test --lib`, `test:rust:mock:extended`, `cargo test -p http3-wasm`
  — all green before and after).
- Two deliberate, documented test-design lessons worth keeping in mind for
  Phase 4+: (1) raw QUIC signals "stream fully done" via a distinct
  `EVENT_FINISHED` (5), not necessarily a `fin: true` flag on the last DATA
  event — a test helper that gates completion detection on `evt.data` being
  present will hang on a FIN-only final frame. (2) C5-style clock-jump tests
  must trigger the post-jump re-check via a command that does **not** send
  fresh data (e.g. not `ping()`) — sending anything lets quiche observe
  post-jump activity and "revive" its own idle-time bookkeeping, undoing the
  simulated fast-forward; `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop`
  expose a test-only `_forceTimeoutCheck()` for exactly this.

### Phase 4 — Full test + release integration

C4 (client-side runtimeMode threading + interop suites), C6 (frozen clock),
D1b (verify.sh step) + D3 (packaging, release.yml, boundary-check scan),
D4 docs.
**Gate:** `pnpm verify` runs the wasm step where `WASI_SDK_PATH` is set and
self-skips cleanly elsewhere (D2's CI job remains the enforcement point);
C3 handshake passes **with `ca` verification enabled (O2)**; packed-tarball
smoke test proves `require('@currentspace/http3')` and the `./wasm` entry
resolve; test lane ungated and inside the 2-min budget.

**Status: PARTIAL (2026-07-08, macOS arm64, local) — D1b and the
locally-verifiable slice of D3 are DONE; C4/C6 (owned by
`test/interop/**`/`test/wasm/**`/`test/support/**`, a parallel workstream)
and the two CI-workflow-file changes below are tracked separately and are
NOT part of this update — see the TODO at the end of this block.**

D1b (`scripts/verify.sh`, §8):
- New wasm build+test step added between the napi-release-build/`build:dist`
  block and `build:test`/the TS test suite, gated on **both**
  `VERIFY_SKIP_WASM` (new — same convention as `VERIFY_SKIP_BROWSER_E2E` /
  `_PERF_GATES` / `_SMOKE_INSTALL`) and `WASI_SDK_PATH` being non-empty;
  prints a clear, distinct notice and exits 0 on either skip condition
  rather than doing nothing silently. Verified both ways:
  `WASI_SDK_PATH` unset → clean skip, full `bash scripts/verify.sh --fast`
  (13 steps, native build assumed already done) green in 73 s;
  `WASI_SDK_PATH` set → runs `pnpm run build:wasm` for real, then
  `HTTP3_WASM=1 pnpm run test:wasm` for real (not a stub) — 20/21 wasm
  tests passed on this run, the one failure being in the just-created,
  still-in-progress `test/wasm/frozen-clock.test.ts` (C6, a parallel
  agent's untracked WIP file per `git status` at the time — outside this
  slice's ownership); isolating that one file out, the wasm suite is
  20/20. The build+test wiring itself is proven correct: it builds the
  real artifact and runs the real suite, and correctly propagates a real
  failure rather than swallowing it.

D3 (packaging, §8, non-CI slice only):
- **Fixed the `dist/wasm`-wiped-by-`build:dist` bug**, the core packaging
  defect this pass was scoped to fix. Root cause: `build:dist` did an
  unconditional `rmSync('dist', { recursive: true })` before `tsc`; `tsc`
  has no way to regenerate `dist/wasm/http3_client.wasm` (a separate
  cargo+wasm-opt pipeline's output — D1a), so `pnpm run build:wasm`
  followed by `pnpm run build` (or bare `build:dist`) silently deleted it.
  Fix: new `scripts/clean-dist.mjs` stashes `dist/wasm/http3_client.wasm`
  (via `copyFileSync`, not `renameSync`, so it survives the stash dir
  living on a different filesystem than the repo) before the `rmSync`,
  restores it after, and is a byte-for-byte no-op when the artifact
  doesn't exist yet (the common case — no `WASI_SDK_PATH`). `package.json`'s
  `build:dist` now runs it instead of the old inline `node -e` one-liner.
  **Chosen over the alternative** of folding `build:wasm` into the `build`
  script chain, because `build:wasm` hard-requires a wasi-sdk toolchain
  almost nobody has installed by default — wiring it into the default
  `build` script would break plain `pnpm run build` for every native-only
  contributor. Verified: ran `pnpm run build:wasm` then `pnpm run build`
  (that order); the resulting `dist/wasm/http3_client.wasm` is
  byte-identical (sha256 `4d7c4e85…`, 1,539,140 bytes) to what `build:wasm`
  alone produced, while every other `dist/*` file was freshly and fully
  rebuilt by `tsc` in the same run; `pnpm test` afterward: 363 tests, 362
  pass, 1 pre-existing unrelated skip, 0 fail — unaffected.
- **`files` whitelist**: no change needed. A real `pnpm pack --dry-run`
  confirms the existing `"dist"` entry already recursively covers
  `dist/wasm/*` — both `http3_client.wasm` and every compiled
  `lib/wasm/*.ts` → `dist/wasm/*.js`/`.d.ts` output — with no
  `.npmignore`/negation pattern excluding it. `tsconfig.json`'s
  `include: ["lib"]` / `rootDir: "lib"` already covers `lib/wasm/**`
  automatically (no separate include entry was ever needed); `dist/wasm`
  simply looked incomplete earlier in this phase because `build:dist` was
  stale relative to `lib/wasm/*.ts`, not because of a tsconfig gap.
- **`scripts/publish-npm-release.mjs`**: `assertRootPackageLayout`'s
  `requiredFiles` now also lists `dist/wasm/http3_client.wasm`,
  `dist/wasm/core-loader.js`, and `dist/wasm/wasi-shim.js` — the artifact
  plus its two foundational compiled entry points (no single
  `lib/wasm/index.ts` exists yet to name one canonical entry, so this is a
  representative sample, matching the existing check's own granularity for
  the native side, which also samples rather than enumerates every
  `dist/*.js`). **Flagging an immediate, real consequence for whoever wires
  `release.yml` (TODO below): a real `scripts/publish-npm-release.mjs` run
  will now hard-fail unless `pnpm run build:wasm` ran first** — intended
  (this is the regression guard the task asked for), but `release.yml`
  must account for it.
- **`scripts/verify-prebuilds.mjs`**: left untouched, deliberately. It only
  checks presence + literal napi-export byte-markers in the three OS/arch
  `.node` prebuild binaries under `optionalDependencies`
  (`http3.linux-x64-gnu.node` etc.) before they're copied into `npm/<pkg>/`;
  it has no concept of the root tarball's `files` list or `dist/wasm` at
  all. That surface is exactly what the `assertRootPackageLayout` change
  above covers — nothing wasm-shaped belongs in this script.
- **`"./wasm"` subpath export: intentionally NOT added.** The plan's D3
  text above specifies `workerd`/`worker` conditions pointing at
  `dist/wasm/index.workerd.js`, which does not exist — that file is Phase
  5's `lib/wasm/index.workerd.ts` deliverable (§6.6/§9 E-tasks), not yet
  written. Adding an export that resolves to a nonexistent file would make
  any attempt to `require`/`import` `@currentspace/http3/wasm` today fail
  outright (or silently pick the wrong condition), which is strictly worse
  than not exporting the subpath at all. **Deferred to Phase 5**: add the
  export once `lib/wasm/index.workerd.ts` and its dedicated
  (non-`types:["node"]`) tsconfig actually exist, and verify the condition
  ordering (`workerd`→`worker`→`browser`→`default`) against real wrangler
  resolution, per this plan's own note on workers-sdk#2805-class shadowing
  bugs.
- `.github/workflows/ci.yml` (D2) and `.github/workflows/release.yml` (D3)
  intentionally **not touched** — modifying CI workflow YAML that cannot be
  executed/verified in this environment is out of scope here (mirrors how
  Phase 2 handled "cannot create a real GitHub fork": document precisely,
  don't commit unverifiable infrastructure changes). Full, concrete,
  ready-to-implement spec follows.

**TODO for a human or a future agent with CI-execution access** (nothing
below is implemented yet in `.github/workflows/*`; this is meant to be
directly actionable without re-deriving anything):

*`ci.yml` — new `wasm` job (D2):*
1. New job, same triggers as the other CI jobs (PR + push).
2. `dtolnay/rust-toolchain@stable` with `targets: wasm32-wasip1`.
3. `Swatinem/rust-cache@v2` — `ci.yml` has no cargo cache today; this job
   needs one (the bssl+quiche cold build is the long pole). Fold the
   boring-sys version from `Cargo.lock` **and** the pinned wasi-sdk version
   into the cache key so either bump invalidates it.
4. Download+stage wasi-sdk 33 (Linux x86_64 tarball) and
   `export WASI_SDK_PATH=...` via `$GITHUB_ENV` (not a plain shell export)
   so it survives into later steps.
5. Install binaryen (`wasm-opt`) from apt or a pinned release tarball.
   `scripts/build-wasm.mjs` already degrades gracefully (copies the
   unoptimized ~10 MiB build with a log line) if it's missing, but CI
   should install it so the size-checked, real artifact is what ships.
6. `pnpm run build:wasm`.
7. `HTTP3_WASM=1 pnpm run test:wasm`.
8. This job is also the real O3 gate (Linux/bindgen libclang variance,
   Risk R3) — it has never run on Linux CI as of this writing (the spike
   and Phase 2 build were both macOS-only); expect to need
   `-fvisibility=default` (already in `scripts/build-wasm.mjs`) and
   possibly a pinned `LIBCLANG_PATH`.
9. Budget: comfortably inside the existing 45–60 min timeouts once the
   cache in step 3 is warm.
10. Separately (a plain Node script change, not workflow YAML — in scope
    for a future non-CI pass, just not part of this one): extend
    `scripts/check-node-api-boundary.mjs`'s `scanEntries` to include
    `crates/http3-wasm/src` (it currently scans only `src/` + `build.rs`);
    this check is release-blocking and the new crate must stay NAPI-free by
    construction.

*`release.yml` — wasm artifact wiring (D3):*
1. **This pass's change makes this non-optional, not aspirational:**
   `scripts/publish-npm-release.mjs`'s `assertRootPackageLayout` now
   requires `dist/wasm/http3_client.wasm` (+ two compiled TS entries) to be
   in the packed tarball or the publish step throws. `release.yml` MUST run
   a wasm build before invoking `scripts/publish-npm-release.mjs`
   (`npm run release:publish:npm`), or every future real release will
   hard-fail at that check.
2. Add a `build-wasm` job (or step in the existing release job): install
   wasi-sdk 33 + binaryen (same recipe as `ci.yml`'s `wasm` job — consider
   factoring "install wasi-sdk + binaryen" into one composite action shared
   by both workflows so the two copies can't drift), run
   `pnpm run build:wasm`, then `actions/upload-artifact` the resulting
   `dist/wasm/http3_client.wasm`.
3. The job that runs `scripts/publish-npm-release.mjs` should
   `actions/download-artifact` it into `dist/wasm/http3_client.wasm` any
   time before `buildRootPackage()`'s `assertRootPackageLayout` call —
   before **or** after `build:dist` both work now, since this pass's
   `scripts/clean-dist.mjs` fix means `build:dist` no longer wipes a
   pre-staged artifact.
4. **Validate lane**: run `HTTP3_WASM=1 pnpm run test:wasm` against the
   *packed-and-installed* tarball (not the repo's own `dist/`), the same
   shape as the existing `scripts/smoke-install.mjs` precedent
   (pack → install into a temp dir → exercise it), so the lane proves the
   published layout actually works, not just the local build tree.
5. Do not add the `"./wasm"` export as part of this — see the decision-log
   entry (§12) and the paragraph above: Phase 5's job, once
   `lib/wasm/index.workerd.ts` exists.

### Phase 5 — workerd readiness

E-tasks (§9): workerd entry + tsconfig, exports conditions verified against
wrangler resolution, C7 smoke harness, docs, tracking issue for #4463.
**Gate:** C7 passes under `workerd --experimental` with the mock transport;
`wrangler deploy --dry-run` on a sample worker resolves and stays under
size/startup budgets.

**Status: DONE (2026-07-08, macOS arm64, local) — real `workerd`/`wrangler`
signal obtained, not simulated; gate met exactly as written above.**

- **`lib/wasm/index.workerd.ts`** (new): thin re-export entry point —
  `loadHttp3WasmCore`, `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop`,
  `DatagramTransport`, option/event types — plus
  `createUnavailableDatagramTransport()`, a `DatagramTransport` that
  throws a descriptive, unmissable error from every method rather than
  silently no-op-ing, for wiring before a real transport exists. No fake
  workerd UDP transport was written, per this phase's explicit
  instruction — there is nothing to point it at yet (§9 C1).
- **A real, necessary Phase 3 gap found and fixed, not just a Phase 5
  addition**: `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` originally
  (Phase 3) took a `wasmPath: string` and hard-imported
  `loadHttp3WasmCoreFromFile`/`connectNodeUdp` — both `node:fs`/
  `node:dgram`-dependent — directly into files otherwise designed to be
  host-agnostic. That was invisible on Node (the only host ever tried
  before this phase) but made the classes **impossible to compile, let
  alone construct, under a tsconfig without `@types/node`** — verified
  empirically: a dynamic `import()` of a file that itself imports
  `node:fs` still surfaces that file's type errors under such a tsconfig
  (TypeScript does not defer module resolution for dynamic imports the
  way a same-file `if` branch might suggest). Fixed by having the
  constructors take an already-instantiated `core: Http3WasmCore` and a
  required `transportFactory`; the Node-only pieces moved to
  `lib/wasm/node-core-loader.ts` (new, holds `loadHttp3WasmCoreFromFile`,
  split out of `core-loader.ts`) and are now built by the *caller*
  (`lib/client.ts`/`lib/quic-client.ts`, and 2 direct-construction tests)
  rather than the event loop classes themselves.
- **The event/data contract went `Uint8Array`-only inside `lib/wasm/**`**
  (`WasmEvent.data`, keylog lines, `Http3WasmCore.copyOut`/
  `readOutPtrResult`, `CommonWasmClientOptions.ca`/`sessionTicket`,
  `WasmQuicClientEventLoopOptions.cert`/`key`) — `Buffer` is a Node
  ambient global `@cloudflare/workers-types` deliberately does not
  declare (verified by installing it and grepping — no `Buffer`
  anywhere), and the whole point of `tsconfig.workerd.json` is to catch
  exactly this kind of accidental dependency rather than paper over it
  with a fake ambient shim. `.toString('utf8')`/`.toString('base64')`
  (Buffer-specific) became `TextDecoder`/`btoa(String.fromCharCode(...))`
  (Web-standard, present in both Node and workers-types). Every place the
  *native-parity contract* genuinely requires a real `Buffer` instance
  (the public, documented `'sessionTicket'`/`'datagram'` events, stream
  `'data'` chunks, `'keylog'`) still gets one — `lib/wasm-event-bridge.ts`
  (new, deliberately outside `lib/wasm/**` since it imports
  `lib/event-loop.ts`'s `NativeEvent`, forbidden there) wraps every
  `Uint8Array` payload in a real `Buffer.from(...)` at the
  `lib/client.ts`/`lib/quic-client.ts` integration boundary — zero
  behavior change for any existing consumer, confirmed by the full
  existing native suite (362/362, 1 pre-existing skip) and wasm suite
  (21/21) staying green through this refactor.
- **`tsconfig.workerd.json`** (new): compiles `lib/wasm/**/*.ts` with
  `@cloudflare/workers-types` (new devDependency) instead of `@types/node`,
  excluding the two files that are Node-only by design
  (`node-udp-adapter.ts`, `node-core-loader.ts`) plus the new Node-facing
  `index.ts` aggregate. `tsc -p tsconfig.workerd.json --noEmit` passes
  clean. Registered in `eslint.config.mjs`'s `parserOptions.project`;
  the existing `lib/wasm/**` ESLint zone's node:fs exception moved from
  `core-loader.ts` (now fully host-agnostic) to the new
  `node-core-loader.ts`. Beyond `Buffer`, this surfaced two more real
  friction points, both fixed with narrow, documented, verified-correct
  workarounds rather than papered over: `NodeJS.Timeout` (host-dependent —
  `setTimeout`'s return type differs between `@types/node` and
  workers-types; fixed via `ReturnType<typeof setTimeout>`, which
  correctly resolves to whichever ambient declaration is in scope) and
  `WebAssembly.Module`'s constructor (workers-types correctly declares
  `Module` as `abstract` with no public constructor, accurately modeling
  that workerd cannot compile-from-bytes — worked around with a narrow,
  documented local constructor type for the one branch Node's real
  implementation supports; a workerd caller must always pass `{ module }`,
  never `{ bytes }`, which was already true by design).
- **`"./wasm"` package export** (deferred from Phase 4, §12): added with
  **per-condition nested `types`** — `workerd`/`worker` conditions each
  carry their own `types: "./dist/wasm/index.workerd.d.ts"` — not a flat
  top-level `types` key. Verified this distinction matters, not just in
  theory: with a flat key (the plan's originally-sketched shape), a
  `customConditions: ["workerd"]` tsc consumer resolved
  `connectNodeUdp`/`loadHttp3WasmCoreFromFile` successfully even though
  the JS runtime resolved `index.workerd.js`, which does *not* export
  them — exactly the workers-sdk#2805-class shadowing bug the plan
  warned about, reproduced and then fixed with the nested shape (same
  `customConditions: ["workerd"]` consumer now correctly fails to resolve
  those two names — `error TS2305`). `dist/wasm/index.js`'s `default`
  condition target is a new small Node-facing aggregate,
  `lib/wasm/index.ts` (re-exports the same host-agnostic surface plus the
  two Node-only conveniences). Verified against real `pnpm run build` +
  `pnpm pack --dry-run`: every referenced file (`index.d.ts`, `index.js`,
  `index.workerd.d.ts`, `index.workerd.js`, `http3_client.wasm`) exists in
  the packed tarball. The root `.` export is unchanged.
- **C7 smoke harness + sample worker fixture, combined**
  (`examples/workerd-client/`): real `wrangler.jsonc` +
  `worker.ts`, importing `@currentspace/http3/wasm` through a genuine
  `workspace:*` dependency (not a relative path) — resolved through a
  real pnpm workspace symlink exactly like an npm-installed consumer.
  `workerd`/`wrangler` added as devDependencies (the gate needed them, and
  they produced real, working signal — not added speculatively). Real,
  repeated, honest results (full transcripts in the example's own
  README.md and the phase-6/status report):
  - `wrangler dev`: the compiled `dist/wasm/http3_client.wasm` instantiates
    under a real local `workerd` V8 isolate; the full `h3c_*`/`qc_*`
    export surface resolves; `WasmH3ClientEventLoop` (the identical class
    the Node runtime path uses) constructs and drives `connect()` against
    an in-memory mock `DatagramTransport`, producing a real, well-formed
    1200-byte QUIC v1 Initial packet — proving the WASI shim's
    `crypto.getRandomValues`/`performance.now()` bindings, `TextEncoder`/
    `TextDecoder`, `queueMicrotask`-deferred dispatch, and the
    single-`setTimeout` pump/close discipline all run correctly inside
    workerd's own engine. Achieved with `compatibility_flags: []` — no
    `nodejs_compat` — real (if narrow) positive signal that this code path
    specifically does not depend on nodejs_compat fidelity (C15 stays open
    for anything that does need it).
  - `wrangler deploy --dry-run`: succeeds; 1542.56 KiB / 630.20 KiB gzip
    upload size — comfortably under the free-tier 3 MB gzip limit,
    consistent with Phase 2's measurement of the artifact alone.
  - **Not proven, honestly**: any real network I/O (no outbound UDP exists
    to test against — this is the whole point of C1); nodejs_compat
    fidelity for `node:stream`/`node:events`/`Buffer` (C15) — this code
    path is built specifically to avoid needing any of that. No real
    `wrangler deploy` to a Cloudflare account was attempted (out of scope;
    `--dry-run` already covers everything short of the missing UDP piece).
  - One real, unrelated environment finding recorded (not a blocker, not
    part of this fix's scope): pnpm's generated `node_modules/.bin/workerd`
    shim on this platform wraps the `workerd` binary (a raw Mach-O
    executable, not a JS file) with `exec node <path>`, which fails —
    `npx workerd`/`./node_modules/.bin/workerd` both error trying to parse
    the binary as JS. Invoking the real binary path directly works fine,
    and `wrangler` itself is unaffected (it resolves the platform-specific
    `@cloudflare/workerd-darwin-arm64` binary directly, not through this
    shim) — this only matters if you want bare `workerd --experimental`
    rather than `wrangler dev`.
- **Docs**: `docs/WASM_RUNTIME.md`'s existing "Workers / workerd status"
  section (added in Phase 4's docs pass) extended in place with what this
  phase built, the full honest C7 results, what remains, and a
  ready-to-file draft tracking-issue template — no real GitHub artifact
  created (repo owner's call, matching how the Phase 2 quiche-patch
  upstreaming and Phase 4 CI-workflow TODOs were handled).
- **Verification**: `pnpm run lint`, `pnpm run typecheck` (both existing
  tsconfigs), and the new `tsc -p tsconfig.workerd.json --noEmit` all
  clean; full native `pnpm test` unaffected (362/362, 1 pre-existing
  skip); `HTTP3_WASM=1 pnpm run test:wasm` unaffected (21/21, including
  the 2 direct-construction tests updated for the `core`/
  `transportFactory` constructor shape change); `pnpm run build` +
  `pnpm pack --dry-run` confirm every file the new export references is
  actually in the packed tarball.

---

## 12. Decision log / deliberately deferred

| Decision | Status |
|---|---|
| quiche-on-wasip1 vs quinn-proto swap | **quiche** (proven; protocol parity). quinn-proto documented as fallback with effort estimate 6–10 wk |
| Bindings | bare extern-C + hand shim (napi-rs wasm and wasm-bindgen rejected with evidence) |
| Crate layout | feature-gated root crate + thin `crates/http3-wasm` member; no core-crate file moves |
| Event transport v1 | JSON metadata + `dataOff/dataLen` payloads; packed binary deferred behind the decoder interface |
| Packaging v1 | wasm in main package + `./wasm` export; sidecar deferred |
| napi-rs `wasm32-wasip1-threads` Node fallback of the full binding | rejected (N2) |
| H3 client mTLS (cert/key) parity | deferred — preserve today's asymmetry (raw QUIC client has mTLS, H3 client doesn't) |
| 0-RTT / session tickets in wasm | carried through (options + event flow exist); Workers-side persistence helpers deferred (N6) |
| Embedded Mozilla root bundle | deferred; `ca` option (in-memory) is the v1 trust path |
| `peer_certificate_chain` on client HANDSHAKE_COMPLETE | deferred (native client doesn't emit it either; parity preserved) |
| Browser adapter (WebSocket relay à la quinn-wasm/iroh) | out of scope; falls out of the architecture nearly free — revisit after Phase 5 |
| A4 quiche-patch wiring: static `[patch.crates-io]` in root `Cargo.toml` vs. scoped `--config` override | **scoped `--config`, not static.** `scripts/prepare-quiche-wasm-patch.sh` vendors + patches quiche into the git-ignored `target/quiche-wasm-patched/<version>/`; `scripts/build-wasm.mjs` passes `--config patch.crates-io.quiche.path="<that dir>"` to *only* its own `cargo build -p http3-wasm --target wasm32-wasip1` invocation. A static `[patch.crates-io]` in the root manifest would make `cargo build --release` (and every other cargo invocation, on every machine, forever) fail hard on a fresh clone before the vendoring script has ever run — Cargo resolves `[patch]` for the whole workspace regardless of target/features. Verified empirically both orders: `cargo build --release` succeeds identically whether or not `target/quiche-wasm-patched/` exists yet, and `git diff Cargo.lock` shows zero churn beyond the permanent `http3-wasm` package entry (a `--config`-patched build transiently drops the `quiche` entry's `source`/`checksum` fields, but a subsequent unpatched build cleanly restores them) |
| Upstreaming the quiche FFI fix (A4 task 2) | **manual follow-up for the repo owner.** Creating a real `currentspace/quiche` (or similar) GitHub fork and opening a PR against `cloudflare/quiche` requires the repo owner's own GitHub account/credentials — out of scope for automation. The patch itself is ready to submit as-is: `spikes/quiche-wasm-wasip1/quiche-0.29.2-wasm-ffi.patch` (2 lines, `src/crypto/boringssl.rs`, drops an incorrect `-> c_void` on two FFI decls that are `void` in C). Until upstreamed, `scripts/prepare-quiche-wasm-patch.sh` is the local, reproducible, git-ignored vendoring path the wasm build depends on. |
| `package.json` `"./wasm"` subpath export (D3) | ~~**deferred to Phase 5, not added in the Phase 4 D1b/D3 pass.**~~ **LANDED in Phase 5** (2026-07-08): added with **per-condition nested `types`** (`workerd`/`worker` each carry their own `types: "./dist/wasm/index.workerd.d.ts"`), not the flat single top-level `types` key originally sketched in §8 D3 above — a flat key turned out to be a real, reproduced instance of the workers-sdk#2805-class shadowing bug this plan already worried about (verified: under `customConditions: ["workerd"]`, a flat `types` key let `connectNodeUdp` — a Node-only export — resolve successfully even though the actual JS runtime target, `index.workerd.js`, does not export it; the nested shape fixes this, confirmed by the same check now correctly failing with `TS2305`). See the Phase 5 status block for the full verification (real `wrangler`, real `node --conditions=workerd`, real `customConditions` tsc checks). |
| CI wiring for the wasm artifact: D2's new `ci.yml` `wasm` job, D3's `release.yml` upload + validate lane | **not implemented in this pass — `.github/workflows/*.yml` is out of scope for an agent that cannot execute a real GitHub Actions run** (same reasoning as the "cannot create a real GitHub fork" row above: document precisely, don't commit unverifiable infrastructure changes). A complete, directly-actionable spec — including a real consequence this pass's `publish-npm-release.mjs` change introduces (release now hard-fails without a prior wasm build) — is written out in the Phase 4 status block's TODO above rather than duplicated here. **Still true after Phase 5** — nothing in Phase 5 touched CI workflow files either, for the same reason. |
| Phase 5 `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` constructor shape: `wasmPath`+implicit `node:dgram` default (Phase 3) vs. `core: Http3WasmCore`+required `transportFactory` (Phase 5) | **changed in Phase 5, not additive.** The Phase 3 shape hard-imported `node:fs`/`node:dgram` from inside files otherwise meant to be host-agnostic, which is invisible on Node (the only host tried before Phase 5) but makes the classes impossible to compile under a tsconfig without `@types/node` — a real, necessary fix rather than a style preference. `lib/client.ts`/`lib/quic-client.ts` and 2 direct-construction tests (`test/wasm/deterministic-clock.test.ts`, `test/wasm/frozen-clock.test.ts`) were updated accordingly; all pre-existing wasm tests (21/21) and the native suite (362/362) stayed green through the change. |
| `lib/wasm/**` event/data payload type: `Buffer` (Phase 3) vs. `Uint8Array` (Phase 5) | **changed in Phase 5, not additive — but the public API contract is unchanged.** `Buffer` is a Node ambient global `@cloudflare/workers-types` deliberately does not declare. `WasmEvent.data`, keylog lines, `Http3WasmCore.copyOut`/`readOutPtrResult`, and the `ca`/`sessionTicket`/`cert`/`key` option fields are `Uint8Array` throughout `lib/wasm/**` now; the native-parity contract (real `Buffer` instances for the documented public `'sessionTicket'`/`'datagram'` events, stream `'data'` chunks, `'keylog'`) is preserved by a new, deliberately Node-only bridge file, `lib/wasm-event-bridge.ts` (outside `lib/wasm/**`, since it imports `lib/event-loop.ts`'s `NativeEvent`, forbidden there), which wraps every payload in a real `Buffer.from(...)` at the `lib/client.ts`/`lib/quic-client.ts` integration boundary. Zero behavior change for any existing consumer — confirmed by the full native and wasm suites staying green. |

## 13. References

- Research dossiers (session artifacts, summarized throughout):
  quiche-wasm feasibility + proven recipe; binding strategy; workerd
  constraints; Node harness measurements; quinn-proto fallback; Rust seam
  map; TS contract map; build/CI/packaging map.
- `spikes/quiche-wasm-wasip1/` — proven spike + patch.
- cloudflare/workerd#4463 (UDP request; author's comment =
  `cloudflare-udp-client-discussion-comment.md`), workerd#6451 (jasnell on
  QUIC posture), napi-rs/node-rs#862, rust-bindgen#1681,
  cloudflare/boring#288, jedisct1/boringssl-wasm, Frando/quinn-wasm, iroh
  v0.33 browser support.
