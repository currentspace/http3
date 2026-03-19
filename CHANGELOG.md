# Changelog

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

