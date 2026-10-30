# Release Runbook

## Dist Tags

- `canary`: fast iterative validation
- `rc`: pre-production candidate
- `latest`: stable

## Standard Release Flow

1. Prepare the release commit on `main` with the target version and a versioned `CHANGELOG.md` entry, using `docs/RELEASE_EVIDENCE.md` as the working audit ledger behind the notes.
2. Run `pnpm run release:latest -- --validate-only --dist-tag <canary|rc|latest>` to execute the full local gate and a GitHub Actions dry-run publish on a candidate branch.
3. Run `pnpm run release:latest -- --dist-tag <canary|rc|latest>` to push `main`, publish through `release.yml`, create the Git tag, and create the GitHub release from the matching versioned `CHANGELOG.md` section.
4. Confirm npm dist-tags for the root package and each native sidecar package.
   Stable publication uses the configured npm Trusted Publisher (OIDC) and does
   not require an `NPM_TOKEN` secret. Canary is an independent channel: the
   established tokenless release path updates `latest` only. The optional legacy
   canary-mirroring step runs when `NPM_TOKEN` is configured; it is not a stable
   release prerequisite. Do not report canary alignment unless registry readback
   confirms it.

For a candidate PR, first synchronize versions and release notes, then run
`gh workflow run release.yml --ref <candidate-branch> -f dist_tag=latest -f dry_run=true`.
The `release:latest` wrapper requires starting from `main`; even `--validate-only`
pushes a candidate branch and dispatches CI. Neither command publishes when its
dry-run mode is selected. The candidate version must be unused on npm.

Before merge/publication, complete all hosted checks and the manual Safari gate
in `SAFARI_VALIDATION_RUNBOOK.md`. A locally trusted loopback HTTPS staging server
is acceptable when explicitly selected for the release; record its certificate,
source revision, browser evidence, and matching curl checks.

## Local Gate

GNU Linux prebuilds must pass `scripts/check-linux-prebuild-abi.py` before upload.
The GLIBC_2.34 symbol limit preserves the published 0.9.2 floor. Build Linux x64
on the pinned Ubuntu 22.04 runner: newer headers redirect BoringSSL 5's CPU
environment parsing to `__isoc23_strtoull`, raising the floor to GLIBC_2.38.
Keep runtime testing on newer Linux versions as well; a green test on a newer
runner alone does not establish compatibility with older deployed systems.

- `pnpm run release:check`
- This runs lint, typecheck, the release-blocking TypeScript and Rust lanes, browser checks, and the concurrency/load gates.
- `pnpm run release:local-gate`
- This runs `pnpm run release:check` plus the packed-install smoke test locally before CI publish.

## Manual Perf Campaign

Before promoting transport-topology or runtime-selection changes, capture a
small artifact set and review it with the analyzer:

```bash
pnpm run bench:quic -- --profile smoke --results-dir perf-results --label release-quic-smoke
pnpm run bench:h3 -- --profile smoke --results-dir perf-results --label release-h3-smoke
pnpm run bench:quic:docker -- --profile smoke --results-dir perf-results --label release-quic-docker
pnpm run bench:h3:docker -- --profile smoke --results-dir perf-results --label release-h3-docker
pnpm run perf:analyze -- --results-dir perf-results
```

When deeper Linux or macOS diagnosis is needed, add profiler wrapper runs from
[`PERF_PROFILING.md`](./PERF_PROFILING.md) and keep the generated profiler
artifacts next to the benchmark JSON.

Do not hard-wire new benchmark gates into CI until the baseline criteria in
`PERF_PROFILING.md` are stable.

## One-Time Bootstrap For A New Native Package

- New native sidecar packages must exist on npm before Trusted Publisher can be configured for them.
- Use a prerelease bootstrap version such as `<version>-bootstrap.0`, publish it with `pnpm run release:bootstrap:native -- --binary <downloaded-node-file> --version <version>-bootstrap.0 --publish`, then configure Trusted Publisher for that package in npm.
- After Trusted Publisher is saved for the new package, publish the real release version through `release.yml`.

## Post-Release Validation

- Install the published package in a clean project and verify `import "@currentspace/http3"` and `import "@currentspace/http3/fetch"` both resolve.
- Confirm the root tarball contains `dist/` (including WASM), `bin/`, and the
  native loader, but no `.node` files or docs/examples. Verify that a clean install
  with optional dependencies loads its matching native platform package.
  `scripts/smoke-install.mjs` packs the current host sidecar as well as the root;
  candidate browser installs must do the same to avoid using an older
  same-version registry binary. Use `node scripts/pack-host-native.mjs DIR`.
- Confirm npm dist-tags for the root and every sidecar listed in its
  `optionalDependencies`, including the new musl and Intel macOS packages once
  they are published. Do not maintain a separate three-package release list.

## Platform coverage qualification

The platform series adds `linux-x64-musl`, `linux-arm64-musl`, and `darwin-x64`.
Bootstrap each new npm package and configure its Trusted Publisher before its
first real release using the procedure above. A source manifest entry does not
prove the sidecar exists in the registry. All root/sidecar versions must match.

Musl builds need both Rust host build scripts and the addon to use a compatible
dynamic CRT configuration: BoringSSL bindgen loads libclang at build time.
Verify architecture and dynamic dependencies, then test in a clean Alpine Node
runtime without the compiler environment. The GNU GLIBC symbol-floor check
does not apply to musl; do not weaken it to accept a different ABI.

Use `scripts/platform-smoke.mjs` for an isolated installed candidate:

```sh
# Build dist and the WASM artifact first. Creates a fresh npm consumer and
# installs the packed root with --omit=optional, then checks H2/H3/raw QUIC.
node scripts/platform-smoke.mjs

# Existing prepacked candidates, including a newly built matching sidecar:
node scripts/platform-smoke.mjs --native --tarball /path/root.tgz --sidecar /path/sidecar.tgz
```

The harness requires OpenSSL on PATH for temporary certificates; it installs no
system trust. It asserts response status/content, runtime metadata, strict-mode
errors, and process warning counts, and fails nonzero on timeout. Explicit WASM
is also checked when native is present. `scripts/smoke-install.mjs` separately
checks installed declarations in all supported TypeScript resolution modes.

`platform-fallback.yml` is a reusable release gate for Node 24/26 on Windows,
GNU Linux x64/arm64, and macOS arm64/Intel. It consumes the same WASM build as
the release. Musl consumer jobs test both absent and installed native sidecars
on real x64 and arm64 runners. Default Docker can emit the existing native
fast-to-portable warning; only a binding-unavailable/WASM fallback is a failure
for the normal musl install. Native x64 runs are required for qualification;
emulated runs are supplementary reproduction evidence.

When preparing several PRs with Actions temporarily disabled, retain the prior
repository Actions permissions and verify the disabled setting before pushing.
Record local checks and unrun platform gates explicitly. Restore the original
settings after the series is prepared and run consolidated qualification on the
final candidate; the absence of per-PR checks is not qualification evidence.
No npm publication is part of preparing the PR series.

For implementation PRs that intentionally retain an already published version,
dispatch `release.yml` with `qualification_only=true` and `dry_run=true`.
This runs build and installed-consumer gates but skips the publish job, including
its registry-version availability check. The normal release dry run still
requires an unused version and remains required before actual publication.

## Rollback

- Publish a fixed patch quickly under the same dist-tag.
- Pin downstream consumers to the last known-good version until the patch is available.
- Record the incident and remediation in the changelog and release notes.


## Batch integration fixes for release-age consumers

Group related framework fixes into one qualified candidate and release the root
plus its native sidecars together. Avoid publishing a new patch for each example
adjustment. Test candidate tarballs before publishing, so consumers can validate
once rather than repeatedly exempting exact versions from their release-age rule.
A `stable` dist-tag would only name a version; it does not make a new publication
older or bypass a package manager's age policy. This batch keeps the existing
`latest` channel. Consumers can wait out their configured delay before updating;
urgent exemptions should remain explicit and limited to the versions they need.
