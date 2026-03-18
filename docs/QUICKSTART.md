# Quickstart

## Prerequisites

- Node.js 24+
- Rust toolchain (for local native builds)
- OpenSSL

## Local install and build

```bash
npm ci
npm run build
```

## Run test suite

```bash
npm test
npm run test:interop
```

## Run browser compatibility smoke

```bash
npx playwright install --with-deps chromium firefox
npm run test:browser:e2e
```

Use the explicit `curl --http3-only` and `curl --http2` checks below when you
need protocol-specific confirmation in addition to browser smoke coverage.

## Local Docker demo

```bash
npm run docker:build
npm run docker:up
```

Then test:

```bash
curl --http3-only -k https://127.0.0.1:8443/
curl --http2 -k https://127.0.0.1:8443/
```

