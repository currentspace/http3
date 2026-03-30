# Changelog

## 0.7.1

- Fixed `optionalDependencies` in the root package to reference the matching `0.7.1` native sidecar versions instead of the stale `0.6.0` references shipped in 0.7.0.

## 0.7.0

### Memory leak and correctness fixes

- Fixed a heap leak where `ServerHttp3Stream` and `QuicStream` readable sides were never consumed on GET requests, preventing the `'close'` event from firing and streams from being GC'd. Heap dropped from 358MB to 5.4MB after 1M requests.
- Fixed silent data loss at 8 call sites where `.unwrap_or(0)` swallowed fatal send errors (`InvalidStreamState`, `StreamLimit`, `FinalSize`); these now emit `EVENT_ERROR` to JS instead of retrying forever in `pending_writes`.
- Fixed partial-write data loss where `into_vec()` consumed the send buffer before confirming the write succeeded; now uses borrow-based `stream_send`.
- Fixed io_uring RX buffer ring exhaustion and implemented a stop-and-wait cross-thread echo protocol for reliable buffer recycling.

### Performance (+21.8% H3 sustained throughput)

- Added `RecyclableBuffer` for inbound data using `napi_create_external_buffer` with finalize callbacks that return buffers to the worker pool via crossbeam, eliminating malloc fragmentation.
- Added `ChunkPool` with 1KB/2KB/4KB size classes for outbound `WorkerCommand::StreamSend` and `PendingWrite`, with auto-recycle on drop.
- Added zero-copy H3 send via `send_body_owned()` wrapping `Vec` in `ArcBuf` without copying, matching the QUIC zero-copy path.
- Normalized `JsH3Event` object shapes so all events present the same set of JS properties (`headers`, `fin`, `meta`, `metrics`), reducing V8 hidden classes from 6-8 to 2 and eliminating megamorphic inline-cache misses in the hot event dispatch path.
- Added lazy `BackpressureState` allocation: the per-stream backpressure state and its two internal arrays are no longer created eagerly, saving ~69K object allocations per second under sustained load.
- Added combined `sendResponse` NAPI method (`respondWithBody` on `ServerHttp3Stream`) that sends response headers, body, and FIN in a single FFI boundary crossing instead of three separate NAPI calls.

### Shutdown lifecycle fix

- Fixed a deadlock where `close()` on all four event loop types (H3 server/client, QUIC server/client) would block the JS event loop by synchronously joining the native worker thread, preventing the TSFN shutdown sentinel callback from ever firing.
- Split the native `shutdown()` into `requestShutdown()` (non-blocking command send) and `joinWorker()` (thread join), exposed via NAPI on all native binding structs.
- Switched QUIC client/server TSFN ownership to `Arc<EventTsfn>` to keep the threadsafe function alive during the shutdown-to-join window, matching the H3 server's existing pattern.

### Transport drivers

- Improved io_uring driver reliability: RX buffer ring exhaustion fix, waker resubmit, `sendmsg` EAGAIN handling, `recv_from` fallback.
- Improved kqueue driver with pending-write drain fixes and waker reliability.
- Added driver-tracing feature gate for optional per-event diagnostic logging.

### Test infrastructure

- Added 545 tests (161 Rust unit, 53 integration, 8 packet loss, 325 TypeScript), all passing.
- Added comprehensive e2e scenario suites for both H3 and raw QUIC covering echo, streaming, datagrams, server push, concurrent sessions, connection churn, and mixed workloads (34 tests).
- Added 5-minute longhaul tests: sustained H3 at 24.6K req/s (0 errors), sustained QUIC at 45.6K streams/s (0 errors), H3 mix at 197 req/s, QUIC mix at 2.4K ops/s, idle connection stability, and connection churn.
- Added FFI boundary stress tests validating rapid lifecycle, large buffer transfers (256KB), concurrent sessions, and telemetry integrity.
- Added packet loss simulation tests at 5%, 10%, and 20% loss rates.
- Added heap profiling scripts (`heap-profile-h3-sustained.mjs`, `analyze-heap-snapshot.mjs`).

### Dependencies

- Rust 1.94, napi-rs 3.8, quiche 0.26.1 (BoringSSL).
- Migrated to pnpm for workspace management.

## 0.6.0

- Added first-class raw QUIC client mTLS support through the public `connectQuic()` and `connectQuicAsync()` options, including `cert`/`key` validation and explicit `ERR_HTTP3_TLS_CONFIG_ERROR` failures for invalid TLS input.
- Added raw QUIC server-side client certificate policy control with `clientAuth: 'none' | 'request' | 'require'`, defaulting to `require` whenever a client-verification `ca` is configured.
- Added peer-certificate inspection on `QuicServerSession` with `peerCertificatePresented`, `getPeerCertificate()`, and `getPeerCertificateChain()` so applications can pin exact client certificates after CA verification.
- Updated the raw QUIC API contract, configuration reference, and QUIC guide with explicit mTLS examples, server policy semantics, and a documented certificate-pinning pattern.

## 0.5.0

- Aligned fast raw-QUIC and HTTP/3 client ownership so high-connection client lanes now reuse one worker and one local UDP port per bind family instead of scaling setup work with session count.
- Reduced `runtimeMode: 'auto'` fallback churn by caching fast-path unavailability for the life of a process, so restricted Linux environments stop re-probing `io_uring` on every new connection attempt.
- Added a repeatable cross-platform performance workflow with persisted host and Docker artifacts, Linux/macOS profiler wrappers, and attribution harnesses for quiche-direct, loopback, and Node mock-transport analysis.
- Fixed the raw-QUIC timeout/reap regression captured in the committed `quic-bottlenecks` artifact set, returning the affected 1000-stream lane from `950/1000` completions with timeouts to `1000/1000` with lower tail latency.
- Kept one important limit explicit: like-for-like HTTP/3 lanes still trail raw QUIC controls, so this release should be read as transport hardening and observability work rather than as proof that higher-layer overhead is gone.

## 0.4.0

- Added explicit QUIC runtime selection with `runtimeMode: 'auto' | 'fast' | 'portable'`, structured fallback reporting, and runtime metadata on client/server objects.
- Fixed hostname/service-name endpoint support for HTTP/3 and raw QUIC clients, including clean `connect*Async()` rejection without early unhandled session errors.
- Added a portable Linux QUIC driver for ordinary containers while preserving the `io_uring` fast path, plus Linux Docker runtime-matrix coverage and documentation for capability/seccomp requirements.

## 0.3.1

- Fixed the root npm package layout so published tarballs always include the built `dist/` JS/types surface referenced by `main`, `types`, and the export map.
- Added the dedicated `@currentspace/http3-linux-x64-gnu` native sidecar package and wired it into automated release publishing for Linux x64 glibc installs.

## 0.3.0

- Refreshed the published documentation set with configuration and error-handling guides, example READMEs, and contributor testing/release notes.
- Expanded automated coverage for adapters, EventSource/SSE behavior, keylog/error mapping, graceful shutdown, and raw QUIC prebuild validation.
- Fixed fallback adapter regressions around synchronous handler throws and HTTP/1.1 POST response abortion.
- Added release-time validation for native prebuild exports and clearer runtime errors when raw QUIC bindings are missing from a published artifact.

## 0.2.1

- Added comprehensive raw QUIC guide (`docs/QUIC_GUIDE.md`).
- Added QUIC API surface to `docs/API_CONTRACT.md` (server, client, stream, options, error constants).
- Added raw QUIC and transport layer entries to `docs/SUPPORT_MATRIX.md`.
- Added QUIC quick-start examples and features section to root `README.md`.
- Added CHANGELOG entries for 0.1.3 and 0.2.0.

## 0.2.0

- Replaced mio with platform-native I/O drivers: kqueue on macOS, io_uring on Linux.
- Upgraded quiche from 0.24 to 0.26.1 (fixes MAX_DATA retransmission under sustained throughput).
- Added adaptive optimizations: blocked-stream queue, batch size 2048, loopback MTU 8192, TX buffer pool, event coalescing, adaptive high-water mark, proactive stream shutdown.
- Fixed io_uring: VecDeque for unsent packets, waker resubmit, sendmsg EAGAIN, recv_from fallback.

## 0.1.3

- Added raw QUIC server and client APIs (`createQuicServer`, `connectQuic`, `connectQuicAsync`, `QuicStream`).
- Added QUIC error constants (`QUIC_NO_ERROR` through `QUIC_CRYPTO_ERROR`).
- Added datagram support (`sendDatagram`, `'datagram'` event) and session resumption (`'sessionTicket'` event).
- Added QUIC protocol verification and stress test suites.

## 0.1.2

- Republish the `0.1.1` release contents under a new npm version after `0.1.1` became unavailable for reuse on npm.

## 0.1.1

- Documented how to share `sessionTicketKeys` across instances for resumption and 0-RTT continuity.
- Added the shared `sessionTicketKeys` pattern to the Hono production example.
- Included the docs and Hono example assets in the published npm package so downstream consumers can inspect them directly.

## 0.1.0

- Initial production-oriented HTTP/3 package for Node.js 24+.
- Unified H3/H2 server surface, client session/stream APIs, fetch/SSE/EventSource adapters.
- Rust worker-thread transport powered by quiche with observability and operational helpers.

