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

- Baseline test suite: pending installation/build.
- Storage guard: three tests passed (APFS/reserve calculation, preflight refusal,
  and cancellation of a running process on threshold crossing); live preflight
  measured 55.50% project / 91.61% system usage.
- Registry target inventory: in progress.
- JavaScript/Rust/WASM/container upgrades: pending.
- Cross-platform, browser, safety, packaging, and performance qualification: pending.

Known integration risks: quiche 0.29.2 WASM FFI patch; exact boring 4.22.0 pin;
BoringSSL source-layout-dependent WASI build; spike manifest with absolute temp
patch path; Node 26 types with Node >=24 support; floating CI Rust versus Docker
1.96.0; runtime acme.sh installer; existing security workflow lacks Rust audit.
