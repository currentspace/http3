# Contributing to @currentspace/http3

## Prerequisites

- Node.js 24+
- Rust 1.85+ (edition 2024)
- OpenSSL (for test certificate generation)
- Docker (optional, for the Linux stress/bench image)
- curl with HTTP/3 support (optional, for interop checks)

## Setup

```bash
git clone https://github.com/currentspace/http3.git
cd http3
npm install
npm run build
```

## Day-to-Day Commands

- **Build:** `npm run build` (Rust native addon + TypeScript)
- **Build test artifacts:** `npm run build:test`
- **Typecheck:** `npm run typecheck`
- **Lint:** `npm run lint`
- **TypeScript + native test suite:** `npm test`
- **Rust tests:** `cargo test`
- **Rust lints:** `cargo clippy --all-targets`

## Test Matrix

- **Core unit/integration lane:** `npm test`
- **curl interop lane:** `npm run test:interop`
- **Browser interop lane:** `npx playwright install --with-deps chromium firefox && npm run test:browser:e2e`
- **Performance gates:** `npm run perf:concurrency-gate` and `npm run perf:load-smoke-gate`
- **Packed-install smoke test:** `npm run smoke:install`
- The full lane breakdown and CI expectations live in `docs/TEST_STRATEGY.md`.

## Docker Testing

`Dockerfile.test` builds a Linux container with Node + Rust and runs the long
stress/bench-oriented test set:

```bash
docker build -f Dockerfile.test -t http3-test .
docker run --rm http3-test
```

For the local demo stack used by the deployment/load-test docs, use the
existing compose helpers instead:

```bash
npm run docker:build
npm run docker:up
npm run docker:down
```

## Release Validation

- Run `npm run release:check` before a release cut.
- Run `npm run smoke:install` against the packed artifact.
- Follow `docs/RELEASE_RUNBOOK.md` for canary/rc/latest flow.
- For `rc` and `latest`, also complete `docs/SAFARI_VALIDATION_RUNBOOK.md`.

## Pull Requests

- Run `npm run typecheck && npm test && cargo test` before submitting.
- If you changed protocol, browser, packaging, or performance-sensitive code, run the relevant extra lanes from `docs/TEST_STRATEGY.md`.
- Keep commits focused — one logical change per PR
- Clippy must pass: `cargo clippy --all-targets`
