# Test Strategy

## Test lanes

- Unit-ish helper coverage: focused logic tests under `test/`.
- Integration: Node + native addon end-to-end over real UDP/TLS.
- Shared-reactor topology: focused client-port/worker reuse tests for raw QUIC and H3.
- Linux runtime matrix: Docker-based `portable` / `auto` / `fast` validation, including seccomp behavior.
- Benchmark telemetry: host-side QUIC/H3 harnesses that expose driver/worker/session counters.
- Interop: external curl HTTP/3/HTTP/2 verification.
- Browser e2e: Chromium + Firefox automated checks.
- Manual release gate: Safari runbook validation.

## Commands

- Full TS/native integration: `npm test`
- Shared-reactor focused lanes:
  - `node --test dist-test/test/quic-fast-shared-worker.test.js`
  - `node --test dist-test/test/h3-fast-shared-worker.test.js`
- Linux Docker runtime matrix: `npm run test:docker:runtime`
- Host benchmark lanes:
  - `npm run bench:quic -- --profile smoke`
  - `npm run bench:h3 -- --profile smoke`
- Curl interop lane: `npm run test:interop`
- Browser e2e lane: `npm run test:browser:e2e`
- Performance gates:
  - `npm run perf:concurrency-gate`
  - `npm run perf:load-smoke-gate`

## CI gating policy

- PR/push gates:
  - lint + typecheck
  - `npm test`
  - focused shared-reactor topology coverage on supported hosts
  - Linux arm64 Docker runtime matrix
  - browser e2e (Chromium + Firefox)
  - concurrency/load smoke
  - curl interop workflow
- Release gates:
  - `npm run release:check`
  - QUIC + H3 smoke benchmark runs when transport topology changes
  - packed install smoke
  - Safari validation checklist for `rc`/`latest`

