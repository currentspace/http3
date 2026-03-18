# Test Strategy

## Test lanes

- Unit-ish helper coverage: focused logic tests under `test/`.
- Integration: Node + native addon end-to-end over real UDP/TLS.
- Shared-reactor topology: focused client-port/worker reuse tests for raw QUIC and H3.
- Linux runtime matrix: Docker-based `portable` / `auto` / `fast` validation, including seccomp behavior.
- Benchmark telemetry: host-side QUIC/H3 harnesses that expose driver/worker/session counters.
- Cross-platform perf campaign: persisted host/Docker/macOS artifacts plus analyzer-driven comparison before new gates.
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
  - `npm run bench:quic -- --profile smoke --results-dir perf-results --label quic-smoke`
  - `npm run bench:h3 -- --profile smoke --results-dir perf-results --label h3-smoke`
- Docker benchmark lanes:
  - `npm run bench:quic:docker -- --results-dir perf-results --label quic-docker`
  - `npm run bench:h3:docker -- --results-dir perf-results --label h3-docker`
- Profiler wrappers:
  - `npm run perf:linux:quic -- --perf-stat --profile throughput --results-dir perf-results --label quic-linux`
  - `npm run perf:linux:h3 -- --perf-record --strace --profile stress --results-dir perf-results --label h3-linux`
  - `npm run perf:macos:quic -- --sample --profile throughput --results-dir perf-results --label quic-macos`
  - `npm run perf:macos:h3 -- --xctrace --profile stress --results-dir perf-results --label h3-macos`
- Artifact comparison: `npm run perf:analyze -- --results-dir perf-results`
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
  - manual perf campaign artifact review using `npm run perf:analyze`
  - packed install smoke
  - Safari validation checklist for `rc`/`latest`

The perf campaign is intentionally manual and artifact-driven for now. Do not
promote new performance gates into CI until the baseline criteria in
[`PERF_PROFILING.md`](./PERF_PROFILING.md) are stable.

