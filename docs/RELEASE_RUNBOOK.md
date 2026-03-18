# Release Runbook

## Dist Tags

- `canary`: fast iterative validation
- `rc`: pre-production candidate
- `latest`: stable

## Standard Release Flow

1. Prepare the release commit on `main` with the target version and changelog entry.
2. Run `npm run release:latest -- --validate-only --dist-tag <canary|rc|latest>` to execute the full local gate and a GitHub Actions dry-run publish on a candidate branch.
3. Run `npm run release:latest -- --dist-tag <canary|rc|latest>` to push `main`, publish through `release.yml`, create the Git tag, and create the GitHub release.
4. Confirm npm dist-tags for the root package and each native sidecar package.

## Local Gate

- `npm run release:local-gate`
- This runs lint, typecheck, tests, browser checks, concurrency/load gates, and the packed-install smoke test locally before CI publish.

## Manual Perf Campaign

Before promoting transport-topology or runtime-selection changes, capture a
small artifact set and review it with the analyzer:

```bash
npm run bench:quic -- --profile smoke --results-dir perf-results --label release-quic-smoke
npm run bench:h3 -- --profile smoke --results-dir perf-results --label release-h3-smoke
npm run bench:quic:docker -- --results-dir perf-results --label release-quic-docker
npm run bench:h3:docker -- --results-dir perf-results --label release-h3-docker
npm run perf:analyze -- --results-dir perf-results
```

When deeper Linux or macOS diagnosis is needed, add profiler wrapper runs from
[`PERF_PROFILING.md`](./PERF_PROFILING.md) and keep the generated profiler
artifacts next to the benchmark JSON.

Do not hard-wire new benchmark gates into CI until the baseline criteria in
`PERF_PROFILING.md` are stable.

## One-Time Bootstrap For A New Native Package

- New native sidecar packages must exist on npm before Trusted Publisher can be configured for them.
- Use a prerelease bootstrap version such as `0.4.0-bootstrap.0`, publish it with `npm run release:bootstrap:native -- --binary <downloaded-node-file> --version 0.4.0-bootstrap.0 --publish`, then configure Trusted Publisher for that package in npm.
- After Trusted Publisher is saved for the new package, publish the real release version through `release.yml`.

## Post-Release Validation

- Install the published package in a clean project and verify `import "@currentspace/http3"` and `import "@currentspace/http3/fetch"` both resolve.
- Confirm the published root tarball contains `dist/` plus the expected `.node` prebuilds.
- Confirm npm dist-tags for `@currentspace/http3`, `@currentspace/http3-linux-x64-gnu`, `@currentspace/http3-linux-arm64-gnu`, and `@currentspace/http3-darwin-arm64`.

## Rollback

- Publish a fixed patch quickly under the same dist-tag.
- Pin downstream consumers to the last known-good version until the patch is available.
- Record the incident and remediation in the changelog and release notes.
