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

Linux prebuilds must pass `scripts/check-linux-prebuild-abi.py` before upload.
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
- Confirm npm dist-tags for `@currentspace/http3`, `@currentspace/http3-linux-x64-gnu`, `@currentspace/http3-linux-arm64-gnu`, and `@currentspace/http3-darwin-arm64`.

## Rollback

- Publish a fixed patch quickly under the same dist-tag.
- Pin downstream consumers to the last known-good version until the patch is available.
- Record the incident and remediation in the changelog and release notes.
