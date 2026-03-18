# Test Strategy

## Test lanes

- Unit-ish helper coverage: focused logic tests under `test/`.
- Integration: Node + native addon end-to-end over real UDP/TLS.
- Linux runtime matrix: Docker-based `portable` / `auto` / `fast` validation, including seccomp behavior.
- Interop: external curl HTTP/3/HTTP/2 verification.
- Browser e2e: Chromium + Firefox automated checks.
- Manual release gate: Safari runbook validation.

## Commands

- Full TS/native integration: `npm test`
- Linux Docker runtime matrix: `npm run test:docker:runtime`
- Curl interop lane: `npm run test:interop`
- Browser e2e lane: `npm run test:browser:e2e`
- Performance gates:
  - `npm run perf:concurrency-gate`
  - `npm run perf:load-smoke-gate`

## CI gating policy

- PR/push gates:
  - lint + typecheck
  - `npm test`
  - Linux arm64 Docker runtime matrix
  - browser e2e (Chromium + Firefox)
  - concurrency/load smoke
  - curl interop workflow
- Release gates:
  - `npm run release:check`
  - packed install smoke
  - Safari validation checklist for `rc`/`latest`

