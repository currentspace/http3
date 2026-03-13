# Changelog

## 0.1.1

- Documented how to share `sessionTicketKeys` across instances for resumption and 0-RTT continuity.
- Added the shared `sessionTicketKeys` pattern to the Hono production example.
- Included the docs and Hono example assets in the published npm package so downstream consumers can inspect them directly.

## 0.1.0

- Initial production-oriented HTTP/3 package for Node.js 24+.
- Unified H3/H2 server surface, client session/stream APIs, fetch/SSE/EventSource adapters.
- Rust worker-thread transport powered by quiche with observability and operational helpers.

