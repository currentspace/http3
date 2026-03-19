# Release Evidence

This document is the supporting audit ledger for `0.6.0`. It captures the
release story behind the new raw QUIC mutual-TLS surface, the server-side
certificate policy defaults, and the new peer-certificate inspection API used
for exact client pinning.

`CHANGELOG.md` is the public release-note source; this file records the working
evidence behind that release entry.

## Scope

- Base tag: `v0.5.0`
- Release framing: additive raw QUIC security/API release
- Evidence sources:
  - public API/docs: `lib/quic-client.ts`, `lib/quic-server.ts`, `docs/API_CONTRACT.md`, `docs/CONFIGURATION_OPTIONS.md`, `docs/QUIC_GUIDE.md`, `README.md`
  - native config/enforcement: `src/config.rs`, `src/quic_server.rs`, `src/quic_worker.rs`, `src/h3_event.rs`
  - validation lanes: `test/interop/quic-loopback.test.ts`, `test/interop/quic-edge-cases.test.ts`, `test/core/client-connect-lifecycle.test.ts`

## Release framing

This delta is best described as a raw QUIC authentication release:

- downstream clients can now use mTLS without reaching into package internals
- raw QUIC servers now have an explicit and safe client-certificate policy knob
- applications can now inspect the verified peer certificate and pin an exact
  client certificate after CA verification

## Downstream-visible outcomes

### Public raw QUIC client mTLS is now first-class

Evidence:

- `lib/quic-client.ts`
- `docs/API_CONTRACT.md`
- `docs/QUIC_GUIDE.md`
- `test/interop/quic-loopback.test.ts`
- `test/interop/quic-edge-cases.test.ts`

Outcome:

- `connectQuic()` and `connectQuicAsync()` accept public `cert` and `key`
  options
- invalid `cert`/`key` combinations and malformed PEM fail through the public
  API with `ERR_HTTP3_TLS_CONFIG_ERROR`
- downstream users no longer need to construct the native QUIC client directly
  to use mTLS

### Raw QUIC servers now default to the secure client-auth posture

Evidence:

- `lib/quic-server.ts`
- `src/config.rs`
- `src/quic_worker.rs`
- `docs/CONFIGURATION_OPTIONS.md`
- `test/interop/quic-loopback.test.ts`
- `test/interop/quic-edge-cases.test.ts`

Outcome:

- `createQuicServer()` accepts `clientAuth: 'none' | 'request' | 'require'`
- when `ca` is configured and `clientAuth` is omitted, the effective policy is
  `require`
- unauthorized clients are closed before they surface through the raw QUIC
  server `'session'` event

### Exact client-certificate pinning is now possible on accepted raw QUIC sessions

Evidence:

- `lib/quic-server.ts`
- `src/h3_event.rs`
- `src/quic_worker.rs`
- `docs/QUIC_GUIDE.md`
- `test/interop/quic-loopback.test.ts`

Outcome:

- `QuicServerSession#peerCertificatePresented` exposes the handshake-level
  presence signal
- `QuicServerSession#getPeerCertificate()` returns the verified leaf certificate
  as Node's `X509Certificate`
- `QuicServerSession#getPeerCertificateChain()` returns the presented verified
  chain, leaf first
- applications can CA-verify broadly but still close sessions that do not match
  an expected pinned fingerprint

## Caveats To Disclose

- The peer-certificate inspection API is currently on raw QUIC server sessions,
  not on the higher-level `createSecureServer()` HTTP/3 session surface.
- `clientAuth: 'request'` and `clientAuth: 'require'` both require `ca`.
- `clientAuth: 'none'` cannot be combined with `ca`.
- Exact certificate matching remains an application policy layered on top of CA
  verification; the built-in server policy intentionally stops at verified-chain
  enforcement.

## Release-Blocking Checks

- Full local release gate: `npm run release:local-gate`
- Dry-run publish validation: `npm run release:latest -- --validate-only --dist-tag latest`
- Manual Safari validation for `latest`: `docs/SAFARI_VALIDATION_RUNBOOK.md`

Validated in this release pass:

- `npm run lint`
- `npm run typecheck`
- `npm test`
- `npm run test:browser:e2e`
- `npm run perf:concurrency-gate`
- `npm run perf:load-smoke-gate`
- `npm run test:rust`
- `npm run smoke:install`

## 0.6.0 Changelog Entry

- Added first-class raw QUIC client mTLS support through the public `connectQuic()` and `connectQuicAsync()` options, including `cert`/`key` validation and explicit `ERR_HTTP3_TLS_CONFIG_ERROR` failures for invalid TLS input.
- Added raw QUIC server-side client certificate policy control with `clientAuth: 'none' | 'request' | 'require'`, defaulting to `require` whenever a client-verification `ca` is configured.
- Added peer-certificate inspection on `QuicServerSession` with `peerCertificatePresented`, `getPeerCertificate()`, and `getPeerCertificateChain()` so applications can pin exact client certificates after CA verification.
- Updated the raw QUIC API contract, configuration reference, and QUIC guide with explicit mTLS examples, server policy semantics, and a documented certificate-pinning pattern.
