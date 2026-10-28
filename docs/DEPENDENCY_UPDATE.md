# Dependency refresh, October 2026

Execution branch: `chore/dependency-refresh-2026-10`.
Baseline: `7fcbda5` (0.9.2). This is an execution ledger, not a completion claim.

## Scope and sequence

1. Inventory exact current/target versions; capture baseline tests and storage.
2. Align and pin compiler, Node, pnpm, and verification toolchains.
3. Upgrade JavaScript tooling, browsers, Workers tooling, and examples.
4. Upgrade Rust dependencies, napi, QUIC/TLS, WASI SDK, and custom patches.
5. Refresh containers, OS dependencies, Actions, certificate tooling, and audits.
6. Qualify native/WASM/platform matrices, artifacts, performance, and rollback.

Each stage gets tested and committed separately. Major-version updates remain in
scope. Every retained pin needs a concrete blocker and follow-up; a passing
subset or skipped platform is not full qualification. Publishing/deployment is
separate from preparing and testing release artifacts.

## Storage policy

User ceiling: less than 95% used on local drives. Stop disk-heavy work at 94%,
with a preflight allocation reserve. Use `scripts/storage-guard.py` around builds
and installs, including `--docker` for Docker inventory before/after. It watches
the workspace, home, temporary directory, and explicit extra paths every five
seconds. APFS usage is calculated from used plus available space, matching df.
The guard cannot prevent unrelated processes from filling a drive; recheck host
and Docker state after cancellation. Do not prune unrelated images or caches.

Keep build output, downloads, and temporary files under this workspace's ignored
`artifacts/dependency-update/` directory and `target/` on `/Volumes/home`.

Initial observations:

- Project volume: 56% used, approximately 958 GiB available.
- System/Data volume: 92% used, approximately 76 GiB available.
- Docker Desktop: 14.99 GB images, 10.92 GB build cache, no containers/volumes.
- Docker.raw: 23.55 GiB allocated; backing file is on the system volume.
- No pre-existing target, node_modules, dist, or dist-test in this checkout.

## Evidence and outstanding work

Raw command logs and registry snapshots live in ignored
`artifacts/dependency-update/`. Summaries and selected version decisions are
recorded here before each commit.

- Baseline `verify.sh --fast`: 14 stages passed, 0 failed, 4 skipped (233 s).
  Rust unit: 223 passed; extended protocol tests and Loom passed; Node core/
  runtime/interop/release: 335 passed; FFI: passed.
- Baseline WASM SDK 33: 39 tests passed, no skips.
- Baseline performance and packed install: passed; concurrency 1079 ms, event
  loop p95 8.3 ms over 3750 requests; load smoke passed.
- Baseline browser E2E: not yet run.
- Baseline npm audit: 51 findings (1 critical, 18 high, 27 moderate, 5 low).
- Baseline WASM build changed Cargo.lock source metadata (registry to patch
  path). Restored the original lockfile after the run; fix reproducibility in
  the WASM upgrade stage.
- Storage guard: three tests passed (APFS/reserve calculation, preflight refusal,
  and cancellation of a running process on threshold crossing); live preflight
  measured 55.50% project / 91.61% system usage.
- Registry target inventory: [recorded](DEPENDENCY_INVENTORY.md).
- Toolchain update: pnpm 12.16.1, Rust 1.99.1, Node 24.22.1 / 26.12.1.
  Keep the existing Rust 1.96 minimum until dependency resolution proves a
  higher minimum necessary. Node 25 is EOL and removed from CI.
- Docker internal filesystem: 3% used (23.3 GiB); system volume remains 92%.
- Toolchain validation: `verify.sh --fast` on Node 24.22.1 passed all 14
  enabled stages (94 s; same four explicitly skipped stages as baseline).
  Public API tests on Node 26.12.1 passed 3/3. All seven workflow YAML files
  parsed. Frozen pnpm 12 install passed after migration: the old lockfile had
  missing optional-prebuild records; explicit first-party exclusions and
  `install --fix-lockfile` repaired it. pnpm 12 also records its own binary
  dependencies in a separate lockfile document.
- Latest storage check: 55.83% project, 91.73% system; approximately 74.3 GiB
  system space available. No unrelated Docker resources removed.
- JavaScript update: TypeScript compiler 7.0.2 with the official TypeScript 6
  compatibility API alias for ESLint/napi; Node 24 types; latest stable direct
  dependencies and refreshed transitives across all four workspace packages.
  Source installs now keep optional compiler/runtime binaries while excluding
  only first-party native prebuilds. pnpm recorded version-specific release-age
  exceptions for the explicitly selected current Workers releases; Wrangler's
  own dependency is miniflare 5.20261023.1-beta (not an independently selected
  prerelease). Revisit the compatibility alias with each linter/napi update.
- Updated ESLint required retaining HTTP/2 method receivers and explicit
  single-argument assertion callbacks. Existing lifecycle and protocol tests
  cover these changes. Regenerated napi loader/types with CLI 3.10.9.
- JavaScript validation: Node 24 fast verification 14/14 enabled stages passed
  (85 s); Node 26 core/runtime/interop/release 335 passed, FFI 67 passed, WASM
  39 passed; c8 12 FFI coverage command passed. No npm audit findings remain
  across the updated workspace graph (451 reported dependencies).
- Packed install passed on Node 24.22.1 and 26.12.1 with the regenerated loader.
- Updated Wrangler/workerd local smoke: passed all three fixture checks with
  both the old and 2026-10-28 compatibility dates. Adopted the latter. Dry-run
  bundling passed. These checks cover instantiation, exports, and generated
  Initial packets through a mock transport, not a live Workers UDP handshake.
  Tested both Wrangler's bundled workerd 1.20261023.1 and the directly declared
  1.20261028.1 binary via Miniflare's runtime-path override.
- Playwright 1.64.1 browsers installed on the project volume. macOS browser
  trust needs further validation: `sudo -n true` reports a password is required.
- Rust/WASM/container upgrades: pending.
- Ordinary Rust updates: refreshed arbitrary/derive_arbitrary, bytes,
  crossbeam-channel, libc, log, rcgen, serde/core/derive, serde_json and their
  compatible transitives. `test:rust:full`, UDP/direct-quiche interoperability,
  and `cargo test -p http3-wasm --locked` passed. This stage retains the prior
  napi and QUIC/TLS versions; native addon rebuild follows in the bridge stage.
- Native bridge/platform updates: napi 3.15.0, napi-build 2.6.0,
  napi-derive 3.6.13, io-uring 0.7.15, mio 1.2.4, socket2 0.6.5; allocator
  crates and nix were already current. Node 24 fast verification passed all
  14 enabled stages (141 s), including the rebuilt addon; FFI stress passed
  8/8. Linux-specific I/O qualification remains pending the Docker stage.
- Rust audit tools installed locally: cargo-audit 0.22.2, cargo-deny 0.20.2.
  Pre-TLS audit found crossbeam-epoch 0.9.18 (RUSTSEC-2026-0204), plus
  unmaintained paste and proc-macro-error2 warnings. Full transitive refresh and
  post-update audit remain required.
- Cross-platform, browser, safety, packaging, and performance qualification: pending.

Known integration risks: quiche 0.29.2 WASM FFI patch; exact boring 4.22.0 pin;
BoringSSL source-layout-dependent WASI build; spike manifest with absolute temp
patch path; Node 26 types with Node >=24 support; floating CI Rust versus Docker
1.96.0; runtime acme.sh installer; existing security workflow lacks Rust audit.
