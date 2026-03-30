# Test Strategy

## Repository layout

- `test/core/`: default TypeScript integration coverage for the public API, adapters, SSE/EventSource behavior, shutdown, errors, and helper surfaces.
- `test/runtime/`: runtime-selection and shared-worker topology coverage, including the QUIC driver-pressure lane.
- `test/interop/`: raw QUIC and curl-facing end-to-end lanes that exercise protocol behavior over real transport paths.
- `test/browser/`: browser automation coverage.
- `test/perf-gates/`: budgeted local gates and heavier profiling helpers.
- `test/release/`: packaging and prebuild validation.
- `test/support/`: certificates, helpers, benchmark harnesses, and Docker runtime support code.
- `tests/`: Rust integration-style coverage, now named by intent:
  - `tests/interop_udp_loopback.rs`
  - `tests/interop_quiche_direct.rs`
  - `tests/transport_quiche_pair.rs`
  - `tests/release_curl_standalone.rs`
- `src/`: Rust unit tests live inline with the code they validate.

## Default lanes

- `npm test`: full release-blocking TypeScript bundle across `core`, `runtime`, `interop`, and `release`.
- `npm run test:core`: public API and adapter coverage.
- `npm run test:runtime`: runtime selection, fallback, and shared-worker ownership.
- `npm run test:interop`: TypeScript raw QUIC and curl-facing interop coverage.
- `npm run test:release`: packaging and prebuild guards.
- `npm run test:browser:e2e`: Chromium + Firefox browser compatibility smoke over the HTTPS entrypoint; manual curl/Safari validation still covers explicit protocol confirmation.
- `npm run test:rust`: release-blocking Rust coverage (`cargo test --lib --no-default-features` plus UDP loopback and direct quiche H3 interop).
- `npm run test:rust:diagnostics`: deeper Rust diagnostics that are useful before a publish but are not part of the default CI gate.
- `npm run test:docker:runtime`: Linux Docker runtime matrix for `portable`, `auto`, and `fast` (host architecture locally, `linux/arm64` in CI).

## Performance lanes

- `npm run test:perf`: raw perf-gate and profiling helper tests under `test/perf-gates/`.
- `npm run perf:concurrency-gate`: duration-budget gate around `test/perf-gates/worker-concurrency.test.ts`.
- `npm run perf:load-smoke-gate`: load-smoke gate around `test/perf-gates/load-smoke.test.ts`.
- `npm run bench:quic -- --profile smoke --results-dir perf-results --label quic-smoke`
- `npm run bench:h3 -- --profile smoke --results-dir perf-results --label h3-smoke`
- `npm run bench:quic:docker -- --profile smoke --results-dir perf-results --label quic-docker`
- `npm run bench:h3:docker -- --profile smoke --results-dir perf-results --label h3-docker`
- `npm run perf:linux:quic -- --perf-stat --profile throughput --results-dir perf-results --label quic-linux`
- `npm run perf:linux:h3 -- --perf-record --strace --profile stress --results-dir perf-results --label h3-linux`
- `npm run perf:linux:quic:loopback -- --results-dir perf-results --label quic-loopback`
- `npm run perf:linux:quic:direct -- --results-dir perf-results --label quic-direct`
- `npm run perf:node:quic:mock -- --sink-mode tsfn --callback-mode noop --results-dir perf-results --label quic-mock-tsfn`
- `npm run perf:macos:quic -- --sample --profile throughput --results-dir perf-results --label quic-macos`
- `npm run perf:macos:h3 -- --xctrace --profile stress --results-dir perf-results --label h3-macos`
- `npm run perf:analyze -- --results-dir perf-results`

## Release-blocking vs diagnostic Rust lanes

Release-blocking by default:

- `cargo test --lib --no-default-features`
- `cargo test --test interop_udp_loopback`
- `cargo test --test interop_quiche_direct`

Diagnostic/manual:

- `cargo test --test transport_quiche_pair`
- `cargo test --test release_curl_standalone`

The diagnostic lanes are still important for transport debugging and publish
readiness, but they are intentionally not wired into the default CI bundle until
their environment sensitivity is worth the extra runtime.

## CI gating policy

- PR/push gates:
  - lint + typecheck
  - Rust clippy
  - Rust unit tests
  - `npm test`
  - browser compatibility smoke
  - Linux arm64 Docker runtime matrix
  - concurrency/load smoke
  - packed-install smoke
  - TypeScript interop workflow
  - Rust interop workflow
- Release gates:
  - `npm run release:local-gate`
  - QUIC + H3 smoke benchmark runs when transport topology changes
  - manual artifact review with `npm run perf:analyze`
  - Safari validation checklist for `rc` and `latest`

The perf campaign is intentionally manual and artifact-driven for now. Do not
promote new benchmark gates into CI until the baseline criteria in
[`PERF_PROFILING.md`](./PERF_PROFILING.md) are stable.

## Lifecycle hardening workstream

The active lifecycle instrumentation, integration, and load-test hardening plan
is tracked in [`LIFECYCLE_VALIDATION_CHECKLIST.md`](./LIFECYCLE_VALIDATION_CHECKLIST.md).
Use that checklist as the source of truth for shutdown ordering, GOAWAY/close,
raw QUIC active-stream teardown, durable tracing, and representative
driver/bridge validation work.

