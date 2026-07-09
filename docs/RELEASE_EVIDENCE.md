# Release Evidence

This document is the supporting audit ledger for `0.9.0`. It captures the
release story behind the WASM client/server runtime.

`CHANGELOG.md` is the public release-note source; this file records the working
evidence behind that release entry.

## Scope

- Base tag: `v0.8.6`
- Release framing: WASM runtime release (client and server)
- Evidence sources:
  - design doc: `docs/WASM_CLIENT_PLAN.md`
  - usage guide: `docs/WASM_RUNTIME.md`
  - wasm ABI crate: `crates/http3-wasm/`
  - TS wasm runtime: `lib/wasm/`, `lib/client-event-loop-factory.ts`, `lib/wasm-event-bridge.ts`
  - workerd verification: `examples/workerd-client/`
  - proof-friendly retry-token/pool models: `src/proof_core/`
  - Kani harnesses/fuzz target for the above: `src/proofs/kani_harnesses.rs`, `fuzz/fuzz_targets/retry_token_roundtrip.rs`
  - detached-promise cleanup: `lib/run-detached.ts` and its call sites
  - release metadata: `package.json`, `Cargo.toml`, `npm/*/package.json`, `CHANGELOG.md`

## Release framing

This delta is best described as a WASM runtime release:

- the client (HTTP/3 and raw QUIC) and, Node-only, the server now run on a
  `wasm32-wasip1` build of the same quiche + BoringSSL protocol core used
  natively, reachable via `runtimeMode: 'wasm'`
- the client build is verified running inside real Cloudflare workerd, not
  just Node — the only remaining gap is workerd's own lack of an outbound UDP
  client socket API
- two real bugs (a pool-bucketing inefficiency, a retry-token panic on
  hostile input) surfaced by writing Kani proofs for the server-side code
  this work touched, both fixed and now proof-covered
- a systemic `void`-detached-promise pattern in `lib/` (no rejection
  handling, no way for shutdown to wait for background work) was replaced
  with a drainable task registry

## Downstream-visible outcomes

### WASM runtime is a first-class `runtimeMode`

Evidence:

- `lib/client.ts`, `lib/quic-client.ts` (`connect()`/`connectAsync()`/`connectQuic()`/`connectQuicAsync()`)
- `lib/server.ts`, `lib/quic-server.ts` (`Http3SecureServer.listen()`/`QuicServer.listen()`)
- `lib/client-event-loop-factory.ts` (native/wasm branch, lazy `import()` so native-only consumers never load wasm code)
- `crates/http3-wasm/src/{h3,quic,h3_server,quic_server}.rs` (the `h3c_*`/`qc_*`/`hs_*`/`qs_*` extern-C ABI)

Outcome:

- `runtimeMode: 'wasm'` works end to end for both HTTP/3 and raw QUIC, both
  client and server, verified across the full native x wasm x client x
  server x QUIC x HTTP/3 matrix (8 cells), including a wasm client talking
  to a wasm server over real loopback UDP with zero native code involved
- servers remain Node-only by design (N1) — workerd has no inbound-listening-
  socket model, so a "workerd server" isn't a coherent concept

### Verified inside real Cloudflare workerd, not just asserted

Evidence:

- `examples/workerd-client/worker.ts`, `wrangler.jsonc`, `README.md`
- `lib/wasm/index.workerd.ts`, `lib/wasm/wasi-shim.ts` (host-agnostic — no `node:wasi`, no Buffer, no `node:*`)

Outcome:

- the compiled `http3_client.wasm` artifact instantiates under real
  `wrangler dev`/`workerd`, its full ABI export surface resolves, and
  `WasmH3ClientEventLoop.connect()` generates a real, valid 1200-byte QUIC
  Initial packet inside workerd's own V8 isolate
- `wrangler deploy --dry-run` reproduces cleanly (bundle size confirmed)
- real network handshakes are blocked purely by workerd's own missing
  outbound-UDP API (cloudflare/workerd#4463), not by anything in this package

### Server-side retry-token/connection-routing logic is wasm-compatible and proof-covered

Evidence:

- `src/retry_token.rs` (HMAC-SHA256 via `boring`, replacing `ring` for the server-side token path)
- `src/proof_core/retry_token_model.rs` (payload build/parse, extracted from
  duplicated code in `src/connection_map.rs` and `src/quic_worker.rs`)
- `src/proofs/kani_harnesses.rs` (round-trip correctness, no-panic-on-
  arbitrary-bytes, clock-skew regression — all proven, not just example-tested)
- `fuzz/fuzz_targets/retry_token_roundtrip.rs` (real HMAC-integrated
  `ConnectionMap` path, coverage-guided mutation, 1.69M runs / 46s locally
  with no crashes)

Outcome:

- the retry-token clock-skew check's `i64`-cast-and-`.abs()` panic on
  hostile input (found by writing the no-panic Kani proof) is fixed with
  `u64::abs_diff`, proven correct over the full `u64` domain
- the two server implementations' previously-duplicated token parsing logic
  now has one source of truth

### `buffer_pool.rs`'s checkin bucketing bug is fixed and proof-covered

Evidence:

- `src/proof_core/buffer_pool_model.rs`, `src/proof_core/chunk_pool_model.rs`
- `src/proofs/kani_harnesses.rs` (`*_returns_largest_class_leq_cap`, `*_accepts_every_*_allocation`)

Outcome:

- `class_for_capacity` now returns the largest class `<=` capacity (matching
  `chunk_pool.rs`'s existing fix for the identical bug shape) instead of
  reusing the checkout-side "smallest class `>=`" classification, which
  filed a buffer into a bucket whose declared capacity it didn't meet

### No promise in `lib/` is fire-and-forget without a rejection handler or a shutdown drain

Evidence:

- `lib/run-detached.ts` (`runDetached`, `DetachedTasks`)
- `lib/client.ts`, `lib/quic-client.ts`, `lib/server.ts` (own a `DetachedTasks` registry, drain it in `close()`/`destroy()`)
- `lib/eventsource.ts` (`_startConnection()` gained the try/catch its sibling `_finalizeClose()` already had)

Outcome:

- every `void asyncCall()` site in `lib/` (constructors, event-handler
  callbacks, timers) now either routes its rejection through the object's
  own error-reporting path, or is tracked in a registry the object's own
  `close()`/`destroy()` awaits before completing
- `pnpm run test:core` previously hung 40+ minutes after every test had
  already passed, with no visible cause until the process was killed; after
  this fix it exits in under 10 seconds — the hang was exactly this class of
  bug (a test's own detached background work outliving the test)

## Caveats To Disclose

- Real outbound UDP from Cloudflare Workers/workerd does not exist yet
  (cloudflare/workerd#4463); the workerd verification in this release proves
  module instantiation, ABI resolution, and in-memory protocol-core
  correctness, not a live network handshake from a deployed Worker.
- The wasm ABI crate is client-and-server-capable, but only the client half
  is reachable from workerd; the server half is Node-only by design.
- `crates/http3-wasm/src/abi.rs`'s pointer/length trust boundary
  (`bytes_in`/`write_out_ptr_len`) is enforced by the TypeScript caller, not
  provable in Rust alone — a caller bug there is a real OOB risk in wasm
  linear memory, same as any FFI boundary.
- No `NPM_TOKEN` repository secret is configured; the `latest` release's
  canary-dist-tag mirror step is skipped (logged, non-fatal) rather than
  failing outright. The core publish itself uses npm Trusted Publisher
  (OIDC, `--provenance`), unaffected by this.

## Release-Blocking Checks

- Full local release gate: `npm run release:local-gate`
- Dry-run publish validation: `npm run release:latest -- --validate-only --dist-tag latest`

Validated in this release pass:

- `cargo test --lib --no-default-features` (223/223)
- `pnpm run test:rust:mock:extended` (11/11 integration test binaries)
- `cargo clippy --lib --no-default-features --features os-runtime,node-api`
- `cargo check --no-default-features --features wasm-abi`
- Kani: 19 always-run harnesses + 3 deep harnesses (`HTTP3_KANI_DEEP=1`), all passing
- `cargo +nightly fuzz run retry_token_roundtrip -- -max_total_time=45` (1.69M runs, no crashes)
- `npx napi build --platform --release`
- `pnpm test` (core + runtime + interop + release + ffi, 349 tests)
- `pnpm run lint`, `pnpm run typecheck`
- Full GitHub Actions CI on PR #9: 38/38 checks green, including every
  `verify (macos-15-intel, kqueue)` job (previously intermittently flaky)

## 0.8.4 Changelog Entry

- Refactored unsafe-adjacent Rust logic into proof-friendly pure models for outbound admission, pending writes, connection IDs, recv-buffer accounting, cmsg cursor walking, and io_uring provided-buffer layout.
- Added Kani contracts and bounded harnesses for admission accounting, pending-write release accounting, cmsg cursor bounds, recv-buffer capacity, QUIC-LB CID encoding, and provided-buffer range validation.
- Added standalone Verus sidecar proofs with a bootstrap/smoke path that installs and verifies against the latest Verus binary locally.
- Added structured fuzz targets, Miri smoke coverage, sanitizer scripts, and a Rust safety CI workflow for the new proof/fuzz lanes.
- Fixed deferred-FIN outbound admission accounting so a full payload write whose FIN is not accepted keeps one admission unit held until the FIN is accepted.
- Fixed the Docker curl interop harness so header capture no longer depends on `/dev/stderr` being openable by a spawned curl process.
- Bumped the package line to `0.8.4`, including Cargo metadata, native sidecar package manifests, and root optional sidecar pins.
- Updated the minimum supported Rust version and Clippy MSRV setting to `1.95`.
- Hardened npm release publishing so the latest/canary dist-tag mirror can use either `NPM_TOKEN` or `NODE_AUTH_TOKEN`.

## Historical 0.6.0 Evidence

## 0.6.0 Changelog Entry

- Added first-class raw QUIC client mTLS support through the public `connectQuic()` and `connectQuicAsync()` options, including `cert`/`key` validation and explicit `ERR_HTTP3_TLS_CONFIG_ERROR` failures for invalid TLS input.
- Added raw QUIC server-side client certificate policy control with `clientAuth: 'none' | 'request' | 'require'`, defaulting to `require` whenever a client-verification `ca` is configured.
- Added peer-certificate inspection on `QuicServerSession` with `peerCertificatePresented`, `getPeerCertificate()`, and `getPeerCertificateChain()` so applications can pin exact client certificates after CA verification.
- Updated the raw QUIC API contract, configuration reference, and QUIC guide with explicit mTLS examples, server policy semantics, and a documented certificate-pinning pattern.
