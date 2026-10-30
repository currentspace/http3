# Platform coverage implementation evidence

Date: 2026-10-30. This records candidate implementation checks, not npm
publication or production qualification. Source requirements are in
[PLATFORM_COVERAGE_PLAN.md](./PLATFORM_COVERAGE_PLAN.md).

## Review sequence

1. [Automatic runtime fallback and installed-consumer checks](https://github.com/currentspace/http3/pull/24).
2. [Bounded WASM writes, trailer ordering, and cancellation](https://github.com/currentspace/http3/pull/25).
3. [Native musl/Intel packages, ABI compatibility, and release gates](https://github.com/currentspace/http3/pull/26).
4. Runtime/platform documentation and consolidated validation evidence (this change).

The PRs form a stack and should be reviewed/merged in that order. Root and
sidecar versions remain 0.9.9 during review; no new npm package is implied by
the candidate manifests. New sidecars still require their first-publication
bootstrap and Trusted Publisher setup before a release.

## Local evidence

- Runtime branch `0dad84c`: isolated npm installs with `--omit=optional` pass
  H2/H3 and raw QUIC auto fallback, strict-mode errors, facade runtime metadata,
  explicit WASM, and warning deduplication on macOS arm64 / Node 26.13.0.
- The same candidate installed with its matching local native sidecar selects
  native `fast`/`kqueue` without fallback. Explicit WASM also passes.
- `scripts/smoke-install.mjs` validates root/subpath exports and installed
  declarations under `node`, `node16`, `nodenext`, and `bundler`, plus both
  `worker` and `workerd` conditions without Node types.
- Combined runtime/write revision `831d135`: 552 Node core/runtime/interop/release
  tests, 67 FFI tests, and 128 WASM tests passed with zero skips. Lint and both
  typecheck targets passed. Content hashes cover transfers through 128 MiB,
  uploads, raw QUIC, paused concurrent responses, trailer ordering, and resets.
  Three paused/resumed 32 MiB responses peaked at 9,043,968 bytes of WASM memory
  from an initial 1,966,080 bytes.
- Native-package revision `a812b37`: the macOS native addon was rebuilt;
  228 Rust unit tests, the full Rust interop/mock suite, and 38 release tests
  passed. Fresh packed fallback/native installs and declaration checks passed.
- Native Alpine arm64: 49 transport tests, 6 ABI checker tests, and actual addon
  load passed with Rust 1.99.1, GCC 15.2.0, CMake 4.2.3, Alpine 3.24.3, and
  BoringSSL 5.3.0. Clean installed consumers on Node 24.22.1 and 26.12.1 pass
  both missing/native sidecar cases plus 1,000 portable GETs at concurrency 50.
  [Logs, toolchain receipt, and artifact hashes](../.github/release-evidence/platform-coverage/)
  retain the observed results; they do not qualify x64 or Intel macOS.
- The supplied 0.9.9 repros were run before changes using existing compiled
  artifacts. Missing native addons failed under auto; explicit WASM succeeded.
  A single WASM 32 MiB response reset with H3 code 268, while streamed data
  succeeded. Repro exit status alone was not treated as a pass.

The installed-consumer gate is `node scripts/platform-smoke.mjs`; the native
variant takes `--native --sidecar <candidate.tgz>` and optionally
`--tarball <root.tgz>`. It uses real package resolution outside the checkout and
fails nonzero for wrong status/body, runtime metadata, warning count, or timeout.

## Performance baseline

On macOS arm64 / Node 26.13.0, three exploratory runs per case using the supplied
consumer scripts and pre-change artifacts produced:

| Workload | Native portable | WASM |
| --- | --- | --- |
| 1,000 GETs, concurrency 50 | 10,366–11,803 requests/s | 5,111–5,169 requests/s |
| 32 MiB download | 774–814 MiB/s, single buffer | 92–93 MiB/s, 64 KiB streaming |

These single-host measurements were taken during development, not on an
isolated benchmark machine. They are regression context, not cross-platform
performance guarantees.

## Hosted and release gates

GitHub Actions was verified disabled before the first PR was pushed, as
requested. The original repository setting was enabled with all Actions
allowed. No missing per-PR check is counted as a passing test.

The consolidated `release.yml` `qualification_only=true` mode builds all six
native targets and the shared WASM artifact, runs native and no-addon installed
consumer gates, and skips publication. Future release dry runs still require an
unused version and registry checks. Windows and Intel/macOS/Linux x64 results
must come from actual hosted runs; local arm64 or emulated x64 results do not
prove those targets.
