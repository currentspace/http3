# Release Evidence

This document is the supporting audit ledger for `0.8.4`. It captures the
release story behind the Rust safety, fuzzing, and formal-verification work.

`CHANGELOG.md` is the public release-note source; this file records the working
evidence behind that release entry.

## Scope

- Base tag: `v0.8.3`
- Release framing: Rust safety hardening and proof coverage release
- Evidence sources:
  - proof-friendly models: `src/proof_core/`
  - Kani harnesses/contracts: `src/proofs/`, `scripts/rust-kani-smoke.sh`
  - Verus sidecar proofs: `proofs/verus/`, `scripts/rust-verus-smoke.sh`, `scripts/rust-verus-bootstrap.sh`
  - fuzz/Miri/sanitizer lanes: `fuzz/fuzz_targets/`, `scripts/rust-safety-smoke.sh`, `scripts/rust-sanitizer-asan.sh`
  - release metadata: `package.json`, `Cargo.toml`, `npm/*/package.json`, `CHANGELOG.md`

## Release framing

This delta is best described as a native Rust safety release:

- unsafe-adjacent native logic is split into pure models that are easier to
  fuzz, model-check, and prove
- Kani and Verus are now first-class local/CI verification lanes
- the deferred-FIN outbound admission edge case is fixed and covered

## Downstream-visible outcomes

### Native safety tooling is part of the release process

Evidence:

- `package.json`
- `.github/workflows/rust-safety.yml`
- `scripts/rust-safety-smoke.sh`
- `scripts/rust-kani-smoke.sh`
- `scripts/rust-verus-smoke.sh`
- `scripts/rust-verus-bootstrap.sh`

Outcome:

- release candidates can run Miri, structure-aware fuzz smoke targets, Kani
  bounded proofs, and standalone Verus proofs from npm scripts
- local Verus bootstrap installs the latest binary release and the matching
  Rust toolchain path is validated by the smoke script
- Kani smoke handles the current verifier/MSRV gap by proving against a
  temporary manifest shadow without lowering the real workspace MSRV

### Proof-friendly Rust models now back production behavior

Evidence:

- `src/proof_core/admission.rs`
- `src/proof_core/pending_write_model.rs`
- `src/proof_core/cid_model.rs`
- `src/proof_core/cmsg_cursor.rs`
- `src/proof_core/recv_buf_model.rs`
- `src/proof_core/ring_layout.rs`
- production call sites in `src/outbound_admission.rs`, `src/pending_write.rs`,
  `src/cid.rs`, `src/transport/socket.rs`, `src/transport/io_uring.rs`, and
  `src/unsafe_boundary.rs`

Outcome:

- the proof modules are small, deterministic, allocation-free models of the
  arithmetic and boundary conditions that matter for native safety
- production code now uses those models instead of duplicating ad hoc logic
- Kani/Verus proofs and unit tests exercise the same semantics production uses

### Deferred-FIN outbound accounting is fixed

Evidence:

- `src/proof_core/admission.rs`
- `src/pending_write.rs`
- `src/outbound_admission.rs`
- `src/proofs/kani_harnesses.rs`
- `proofs/verus/admission.rs`
- `proofs/verus/pending_write.rs`

Outcome:

- when a payload write succeeds but the associated FIN is deferred, admission
  accounting keeps one unit outstanding until the FIN is accepted
- partial write release accounting remains bounded by the originally admitted
  units

## Caveats To Disclose

- The current released `cargo-kani` verifier bundles a Rust compiler older than
  the workspace MSRV, so the Kani script uses a temporary manifest shadow for
  proof execution while keeping the real crate at Rust `1.95`.
- Verus proofs are sidecar specifications of the model behavior, not full
  end-to-end proofs of quiche, napi-rs, or the operating-system syscalls.
- The local macOS browser E2E gate requires cached sudo credentials so the test
  harness can trust the temporary localhost certificate in the System keychain.
- npm publish validation still requires the final prebuild artifact set produced
  by the release workflow before the actual `0.8.4` tag/publish step.

## Release-Blocking Checks

- Full local release gate: `npm run release:local-gate`
- Dry-run publish validation: `npm run release:latest -- --validate-only --dist-tag latest`
- Rust safety smoke: `npm run test:rust:safety:smoke`
- Formal deep lane: `npm run test:rust:formal:deep`

Validated in this release pass:

- `npm run verify:docker`
- `cargo clippy --lib`
- `cargo clippy --tests --features bench-internals --no-default-features`
- `npm run test:rust:full`
- `npm run test:rust:loom`
- `npm run test:rust:diagnostics`
- `npm run test:rust:safety:smoke`
- `npm run test:rust:formal:deep`
- `npm run test:rust:sanitizer:asan`
- `npm run test:e2e`
- `npm run test:ffi:stress`
- `npm run test:perf`
- `npm run test:longhaul`
- `npm run test:rust:stress:all`
- `npm run test:docker:runtime`
- `HTTP3_RUNTIME_SKIP_BUILD=1 npm run test:docker:runtime:privileged`
- `npm run docker:interop:build`
- `npm run test:interop:cross-platform`
- `npm run test:interop:cross-platform:io-uring`
- `HTTP3_CONCURRENCY_MAX_MS=12000 pnpm run perf:concurrency-gate`
- `HTTP3_LOAD_SMOKE_TOTAL=150 HTTP3_LOAD_SMOKE_CONCURRENCY=25 HTTP3_LOAD_SMOKE_MAX_MS=10000 pnpm run perf:load-smoke-gate`
- `pnpm run smoke:install`
- `HTTP3_VERUS_REQUIRED=1 bash scripts/rust-verus-smoke.sh`
- `cargo fmt --check`
- `rustfmt --check proofs/verus/*.rs`
- `git diff --check`

Blocked locally:

- `npm run test:browser:e2e` on macOS stopped at the expected keychain
  preflight because cached sudo credentials were unavailable:
  `Run sudo -v before test:browser:e2e`. The same browser E2E lane passed in
  `npm run verify:docker`, where Linux Firefox performs the HTTP/3 validation.
- `HTTP3_BROWSER_SECURITY_SUDO=0 npm run test:browser:e2e` also could not
  complete in this non-interactive shell because macOS `security
  add-trusted-cert` timed out while adding the temporary user-keychain trust
  entry.
- `node scripts/verify-prebuilds.mjs` correctly reports that the release
  workspace still needs the Linux prebuild artifacts from CI:
  `http3.linux-x64-gnu.node` and `http3.linux-arm64-gnu.node`.
- The first CI dry-run release workflow built and validated all prebuilds, then
  failed in the publish dry-run because the runner exposed npm auth through
  `NODE_AUTH_TOKEN` while the canary dist-tag mirror guard only accepted
  `NPM_TOKEN`. The publisher now accepts either environment variable.

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
