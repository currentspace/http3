# Dependency refresh, October 2026

Execution branch: `chore/dependency-refresh-2026-10`.
Baseline: `7fcbda5` (0.9.2). Implementation and local qualification are complete.
This ledger preserves the execution history; earlier pending notes describe the
state at that stage and are superseded by the later results.

## Final outcome

- Updated JavaScript/Rust dependency graphs, Node/pnpm/Rust toolchains,
  QUIC/TLS bindings, browsers/Workers tooling, safety/audit tools, containers,
  certificate tooling, Actions, and update automation in separate commits.
- Native macOS arm64 and Linux arm64 qualification passed. Linux x64 binaries
  were cross-built with the pinned compiler and passed unit, Node/FFI, runtime,
  and package checks under emulation. Hosted CI has not run; its native x64 and
  Intel macOS lanes remain a release prerequisite.
- Browser, WASM/workerd, sanitizer/proof/fuzz-smoke, sustained traffic, package,
  and audit results are detailed below. npm: zero findings; Rust: zero
  vulnerabilities plus the bounded paste maintenance exception.
- Retained compatibility choices are explicit: WASI SDK 33, TypeScript 6 API
  alongside the TypeScript 7 compiler, Node 24 types, and the one quiche FFI
  correction. See Release follow-up and rollback below.
- All four packages passed dry-run and are archived locally with checksums.
  Nothing was pushed, published, or deployed; repository version stays 0.9.2.
- Highest observed system volume usage: **93.97%**, below the 95% ceiling.
  Task Docker resources were removed. Original Docker state is restored:
  10 images (14.99 GB), 63 default cache entries (10.92 GB), no containers or
  volumes. No unrelated image/cache cleanup was performed.

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

## QUIC/TLS and WASM stage

- Updated quiche to 0.30.0, boring/boring-sys to 5.3.0, and the remaining
  compatible Rust transitive graph. Rust audit now reports zero vulnerabilities;
  `paste` 1.0.15 remains unmaintained through latest tikv-jemalloc-ctl 0.7.0.
- Removed the old AES/ChaCha FFI patch and BoringSSL source surgery. boring 5
  supports socket exclusion and WASI entropy directly. The BSSL cache now keys
  on the SDK/compiler and complete build recipe, including no-exception flags.
- Found a different upstream FFI mismatch: quiche's `ERR_error_string_n`
  declaration returns void, while BoringSSL 5 returns a pointer. Vendored the
  checksum-verified quiche release with only that call changed to use generated
  boring-sys bindings; see `vendor/quiche/PATCHES.md`. All builds share the
  same source and lockfile. WASM linker warnings are now fatal.
- SDK 34 failed the standalone spike link with Rust 1.99.1's startup objects
  (`__wasi_init_tp`). Retain checksum-verified SDK 33 pending stable Rust >=1.100
  and rerun spike/artifact/runtime checks at that point. No stub masks the skew.
- The standalone spike is now a workspace member with a working shared build
  command. It emitted a 1200-byte Initial and approximately 995 ms PTO.
- Fixed the direct-quiche test harness to preserve a connection across split
  Initial packets; BoringSSL 5's larger ClientHello exposed its prior reset bug.
- Final native fast verification: 14 passed, 0 failed, 4 explicitly skipped
  stages (99 s); the corrected direct handshake test passed. WASM suite: all
  39 tests passed on Node 26, both WASM links passed with fatal warnings, and
  Cargo.lock remained unchanged. Local workerd fixture passed all three checks.
- Earlier updated-stack run: performance concurrency gate 969 ms (10 s budget),
  event-loop p95 8.4 ms over 3800 requests; load smoke passed. Host WASM Rust
  tests (18 unit + 3 integration) and UDP interoperability passed. An initial
  Node run had one EADDRINUSE port collision; the full rerun and final native
  verification passed. These do not replace pending Linux/browser qualification.
- Storage after this stage: 56.40% project / 91.93% system, approximately
  72.4 GiB system free. No Docker builds or pruning performed yet.

Remaining integration work: safety-tool pins and proof/sanitizer runs,
containers and OS/tool dependencies, CI Actions, runtime certificate installer,
Rust license/audit integration, full platform/browser/package qualification,
SBOMs and automated update configuration.

## Browser qualification

- Playwright 1.64.1 on macOS passed Chromium, Firefox, and WebKit page/fetch/SSE
  HTTP/3 checks (3/3, no skips) with lint and typecheck passing.
- Firefox 157 needed isolated app data (`CFFIXED_USER_HOME`) and its own NSS
  certificate database to avoid reading the user's Firefox directory; upstream
  Playwright #42768 is fixed in Firefox 158. Revisit on the next browser update.
- Temporary user-keychain trust was sufficient; no sudo was required. Cleanup
  explicitly removes trust, restores the search list, deletes the keychain, and
  removes macOS deny-delete ACLs only inside the test-created Firefox home.
  Final test exit was successful and the two earlier residual fixtures were
  removed.

## Safety tools and CI actions

- Pinned nightly 2026-10-28, cargo-fuzz 0.13.2, Kani 0.68.0, cargo-llvm-cov
  0.9.1, and Verus release/0.2026.10.26.d17fbd0. Verus downloads are checked
  against the release asset SHA-256 before extraction. Its launcher requires
  Rust 1.95.0, which bootstrap now installs separately from the project compiler.
- Safety bootstrap no longer updates a floating stable/nightly channel. Kani
  incompatibility/proof failures are fatal by default. Its fallback workspace
  copy excludes artifacts and package caches to avoid recursive disk growth.
- Miri: 6/6 boundary tests; AddressSanitizer: 6/6; seven fuzz smoke targets ran;
  Kani: 19/19 harnesses verified; Verus: 11 proofs verified, zero errors.
  Smoke fuzz runs are setup checks, not sustained fuzz qualification.
- cargo-llvm-cov ran all 223 Rust unit tests: 72.69% line coverage, 73.31% region
  coverage, 72.26% function coverage. Coverage artifacts remain under target.
- Updated all external GitHub Actions to verified release commits, including
  checkout 7.0.1, setup-node 7.1.0, pnpm setup 6.1.0, dependency-review 5.0.0,
  download-artifact 8.0.2, upload-artifact 7.0.2, rust-cache 2.9.3, Docker build
  7.4.0/buildx setup 4.4.1, CodeQL action 4.39.1. The CodeQL bundle release
  tag is deliberately not used as an action version. actionlint passed.
- Storage: approximately 56.72% project / 92.31% system (69 GiB free).

## Dependency audit and SBOM automation

- Updated cargo-deny 0.20.2 configuration without relaxing the default license
  policy; build/dev dependencies are included. First-party Cargo manifests now
  declare the repository's existing MIT license. Added a foldhash 0.2.0-only
  Zlib allowance after inspecting its permissive license.
- Full cargo-deny advisory/license/source checks pass with one explicitly
  documented exception: RUSTSEC-2024-0436 (`paste` 1.0.15). The latest
  tikv-jemalloc-ctl 0.7.0 still depends on this compile-time macro. A separate
  guard rejects any new parent/version or use after 2026-11-28; both rejection
  paths were exercised. No vulnerability advisory is ignored. Maintainers must
  remove the exception when the allocator drops paste, or review it by expiry.
- Security CI now runs pinned Rust audit/deny tools and the exception guard,
  and publishes JavaScript and source CycloneDX SBOMs. Syft 1.55.0 installation
  verifies release archive checksums; pnpm 12 generates the JavaScript SBOM.
- Generated source SBOM includes every one of 228 resolved Cargo packages.
  JavaScript SBOM contains 437 components. Added BoringSSL's actual bundled
  source commit (`427ec40cc8edc545253289232e885708c409e5ea`) and license text,
  resolved from boring-sys 5.3.0's repository submodule at its published commit.
  Patched quiche provenance remains explicit. Container OS inventories follow
  after image qualification.

## Linux arm64 and minimum compiler qualification

- Node 24.22.1 on Debian Trixie: full verification 19 stages passed, no failures
  (192 s). WASM was explicitly skipped in that container and passed separately
  on macOS. Linux browser execution passed Firefox; Chromium and WebKit were
  explicit platform skips, with all three browsers passing on macOS.
- Linux Rust unit tests: 223 passed; direct/UDP interoperability: four passed;
  Loom passed. Node core/runtime/interop/release: 335 passed; FFI: 67 passed.
- All six Linux runtime modes passed, including portable, automatic fallback,
  expected fast-mode rejection under seccomp, and actual io_uring with relaxed
  seccomp (including the privileged lane). Concurrency/load gates passed.
- Node 26.12.1 on Linux arm64 passed the same 335 Node and 67 FFI tests and
  packed-install smoke using the updated native binary. Final macOS Node 26
  tests and packed-install smoke also passed.
- macOS-to-Debian interoperability: 15/15 passed in portable mode and 15/15
  with io_uring enabled. The updated Dockerfile.interop built successfully.
- Actual Rust 1.96.0 `cargo check --locked --all-targets --features
  bench-internals` passed, so the declared minimum stays 1.96. Removed an
  obsolete AWS-LC environment override; no resolved dependency uses AWS-LC.
- Highest observed system use so far: 93.97%, below the 94% guard and 95% user
  ceiling. Removed only completed task containers/images and the 30 exact cache
  IDs from the interop build; preserved all older default-builder cache.

Production/x64 image qualification, final packaged artifacts, sustained tests,
and the closing storage inventory remain in progress.

## Containers and update automation

- All five Dockerfiles use Debian Trixie and immutable Node 24.22.1 base
  digests; Node 26.12.1 is also pinned. CI and local runtime scripts resolve
  exact requested versions through `containers/node-images.json`, rejecting
  unsupported/stale versions instead of silently testing a different Node.
- Production image built and served a successful Hono HTTP/3 response from a
  macOS client. Its actual tools report Node 24.22.1, AWS CLI 2.39.0, and
  acme.sh 3.1.6. AWS CLI comes from its pinned official image; operational
  helper scripts use the same image digest. Those cloud operations were not run.
- acme.sh is downloaded at build time from a verified commit archive, with
  its source and GPLv3 license retained in `/opt/acme.sh`. Startup requires the
  bundled client. Three offline entrypoint tests cover disabled mode, missing
  client failure, and hydration from a valid mocked secret. No real issuance,
  DNS mutation, or AWS writes were performed.
- Updated Dockerfile frontend to stable 1.28.0 with an immutable digest.
  Runtime and stress images now include the previously missing Rust integration
  fixture directory. The verified production image is archived locally.
- Generated a production OS/package CycloneDX SBOM with Syft 1.55.0, then
  annotated the bundled AWS CLI and acme.sh versions/source pins. Reproduce
  using `bash scripts/generate-container-sbom.sh IMAGE OUTPUT.json`; retain
  the source SBOM alongside it for the Rust/BoringSSL dependency graph.
- Renovate configuration validates against the official schema: weekly Monday
  PRs, immutable Actions, grouped QUIC/TLS, napi, and TypeScript updates, Node
  24 type compatibility, and no automerge. Image pins in JSON/Compose and
  operational scripts have custom managers. Repository-side configuration is
  ready; no Renovate service/app activation or remote settings change was made.
- Final npm audit: zero findings. Rust audit: zero vulnerabilities, the one
  documented `paste` maintenance warning. cargo-deny advisory, ban, license,
  and source checks passed. An unrelated incomplete Cargo-home advisory cache
  was left untouched; the successful audit uses this task's local database.

- Dockerfile.test built successfully and all four stress/benchmark suites passed
  on Linux arm64 with io_uring enabled. Its task container was removed.

- Final macOS sustained qualification: all six tests across four suites passed
  in 600 s, including H3 mixed/sustained traffic, idle retention, QUIC mixed
  traffic, sustained transfers, and connection churn; no skips. These ran
  alongside container builds, so their throughput is not an isolated benchmark.
- Updated verification runners to `macos-26-intel` and `macos-26`; GitHub's
  [current runner catalog](https://github.com/actions/runner-images/blob/main/README.md)
  lists both. The prior macOS 14 runner
  [retires November 2](https://github.com/actions/runner-images/issues/13518).
  Workflow syntax validation is local; hosted workflow execution remains a
  separate check after pushing the branch.

- Added Ubuntu 26.04 to native verification alongside Ubuntu 24.04, using
  labels from the same official runner catalog. Release builds retain the
  Ubuntu 24.04 baseline to avoid raising the published Linux glibc requirement.
- Direct x64 Docker build is not qualified: QEMU SIGSEGVs on Rust 1.99.1
  `rustc -vV` before project compilation. A native arm64-hosted x64 cross-build
  and separate x64 Node runtime checks are in progress. This is an emulation
  limitation, not evidence of a successful x64 compiler/runtime lane.

- actionlint 1.7.12 is the latest release but its bundled label list predates
  Ubuntu 26.04. Added only that official hosted label to its configurable
  allowlist, with a source link and removal condition. Full actionlint passes.

## Linux x64 and release artifact qualification

- QEMU cannot run the pinned x64 Rust compiler (`rustc -vV` SIGSEGV). Native
  arm64 Rust 1.99.1 plus Debian's x64 GCC/G++ cross-toolchain successfully
  built the locked x64 addon and Rust test executable. ELF inspection confirms
  x86-64. The x64 Rust executable passed all 223 unit tests under emulation.
- x64 Node 24.22.1 passed 326 Node tests, 67 FFI tests, and packed-install
  smoke. After supplying curl in the temporary runtime image, all nine curl
  interoperability tests also passed with no skips (335 Node tests in total).
- All six x64 runtime-selection lanes passed, including portable/fallback,
  expected seccomp rejection, and io_uring in unconfined/privileged modes.
- The temporary cross-build runtime initially lacked OpenSSL and curl; those
  fixture gaps were fixed and affected tests rerun. The committed runtime
  Dockerfile already installs both. Node 26 additionally needs libatomic,
  copied from the same pinned official Node 26 x64 image as its executable.
- Release script dry-run passed for all three native packages and the root
  package, including native export and WASM layout checks. Packed all four
  locally with pnpm. Used a temporary `0.9.3-dependency-qualification.20261028`
  version only in an isolated staging copy; repository versions remain 0.9.2.
  No package, tag, branch, or image was published or pushed.
- The final packed root package loaded all public entry points and instantiated
  WASM on macOS Node 24/26, Linux arm64 Node 26, and Linux x64 Node 24.
  These checks load from a separate consumer's node_modules, not source paths.

## Release follow-up and rollback

- Keep WASI SDK 33 until stable Rust >=1.100 resolves the demonstrated SDK 34
  startup incompatibility, then rerun both WASM build variants and runtimes.
- Keep TypeScript 6's API alias until ESLint/napi accept the stable TypeScript 7
  API; the compiler itself is already TypeScript 7. Node type declarations stay
  on latest 24.x to enforce the package's Node 24 support contract.
- Revisit the single quiche FFI correction when upstream fixes it; retain
  checksum/provenance and fatal WASM linker warnings until then.
- Remove or explicitly review the scoped paste maintenance exception before
  2026-11-28. The guard rejects version/parent drift and expiration.
- Hosted CI (including native x64 and Intel macOS) must pass after the branch
  is pushed. Local x64 results use cross-built binaries and emulation; they do
  not prove a native x64 compiler build or hosted runner behavior.
- Before an actual release, choose an unused version, synchronize root/native
  package versions, rebuild the complete artifact set on the release runners,
  and run the existing release dry-run workflow. Activate Renovate separately
  if the repository does not already have its service installed.
- No live deployment changed. For rollback, use baseline commit `7fcbda5` in
  an isolated checkout and rebuild from its own lockfiles/toolchains. Keep
  native, JavaScript, and WASM artifacts from the same revision; do not mix an
  old binary with this update's generated loader or dependency graph. The
  updated production image is archived under `artifacts/dependency-update/`.

- Final Linux x64 Node 26.12.1: 335 Node tests, 67 FFI tests, packed-install
  smoke, all public package entry points, and packed WASM instantiation passed.
  The runtime image was archived before removal. Native unit and Node runtime
  tests passed under emulation; direct emulated x64 compiler remains unsupported.
- `artifacts/dependency-update/artifact-manifest.json` records SHA-256 hashes
  for the final lockfiles, three native binaries, WASM, and four package tarballs.

## Closing storage inventory

After task-only Docker cleanup: project volume 57.12% used (919.3 GiB free),
system volume 92.50% used (67.1 GiB free). Peak observed system use was 93.97%.
Docker is back to its starting inventory: 10 original images totaling 14.99 GB,
63 original default-builder cache entries totaling 10.92 GB, zero containers,
and zero volumes. The temporary builder and all task images were removed.
Logs, SBOMs, test toolchains, package tarballs, image archives, and checksums are
retained on the project volume under `artifacts/dependency-update/`.
