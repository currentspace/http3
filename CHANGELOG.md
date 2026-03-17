# Changelog

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

