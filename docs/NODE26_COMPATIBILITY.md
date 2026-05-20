# Node 26 Compatibility And HTTP/3 Alignment

**Last updated:** 2026-05-20

## Runtime Facts

- Node.js 26.0.0 was first released on 2026-05-05 and is in Current status.
- The latest Node 26 line observed for this update is Node.js 26.2.0.
- Node 26 reports `NODE_MODULE_VERSION` 147, but this package uses `napi-rs`
  with stable Node-API feature levels (`napi4`, `napi6`, `napi8`), so release
  prebuilds are shared across supported Node majors.
- CI must test Node 24, 25, and 26 before claiming Node 26 support. Node 24
  remains the LTS baseline; Node 26 is the forward-compatibility target.

Official references:

- Node releases: https://nodejs.org/en/about/previous-releases
- Node 26.0.0 release notes: https://nodejs.org/en/blog/release/v26.0.0
- Node release schedule update: https://nodejs.org/uk/blog/announcements/evolving-the-nodejs-release-schedule

## CI Coverage

The Node 26 support check is intentionally the same workload as Node 24 and
Node 25:

- native addon build plus TypeScript build
- lint and typecheck
- default TypeScript test suite
- Rust unit, clippy, and verify lanes
- Docker runtime matrix, including io_uring lanes where currently run
- TypeScript interop lane
- distribution packaging and packed-install smoke

The release prebuild workflow builds one N-API binary per supported
OS/arch/libc target; Node 24/25/26 compatibility is proven by loading and smoke
testing the same package under each Node major in CI.

## Node Core QUIC/HTTP3 Alignment

Node.js 26 includes QUIC commits from nodejs/node#62387, but the public Node
26.2.0 API docs do not expose a stable QUIC or HTTP/3 module. Treat Node core
QUIC as a research input, not as a runtime dependency or direct interop target
for this package.

Alignment themes from nodejs/node#62387:

| Node core theme | Local alignment target |
| --- | --- |
| Streaming outbound writes and JS-side backpressure | Keep `write()` callbacks tied to local/native admission, not peer ACK, and preserve `Writable` high-water semantics. |
| FIN tracking through stream commit | Keep FIN-only sends explicit and covered by `_final`/pending-write tests. |
| Connection-level flow-control checks before pulling stream data | Keep flow-control tests for large bodies and blocked/resumed streams in both H3 and raw QUIC paths. |
| HTTP/3 body pull, trailers, GOAWAY, and shutdown callbacks | Maintain protocol-correctness coverage for request bodies, response trailers, GOAWAY-before-close, and graceful close. |
| Server-side priority callback | Document current behavior before adding a public priority API; do not expose quiche-specific details. |
| TLS tickets, early data, and client-cert verification | Keep session-ticket, 0-RTT gating, `rejectUnauthorized`, and raw QUIC client-auth behavior tested separately. |
| NEW_TOKEN, retry token hardening, and constant-time token compare | Keep retry/token tests and review token comparison/hashing when retry behavior changes. |
| Preferred address and active migration fixes | Keep active migration documented as deferred unless an implementation and tests are added. |

## Follow-Up Review Checklist

- Compare Node core's stream/FIN changes against `lib/stream.ts`,
  `lib/quic-stream.ts`, `src/pending_write.rs`, and `src/write_outcome.rs`.
- Compare Node core TLS/session-ticket changes against `src/config.rs`,
  `src/connection.rs`, and the session ticket docs/tests.
- Compare Node core GOAWAY and HTTP/3 callback behavior against
  `test/core/protocol-correctness.test.ts`, `test/core/header-preservation.test.ts`,
  and `test/e2e/h3-scenarios.test.ts`.
- File focused follow-up issues for confirmed gaps; do not start direct Node
  core QUIC interop until Node exposes a documented stable or supported API.
