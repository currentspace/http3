# Platform coverage implementation evidence

Date: 2026-10-30. This records candidate implementation checks, not npm
publication or production qualification. Source requirements are in
[PLATFORM_COVERAGE_PLAN.md](./PLATFORM_COVERAGE_PLAN.md).

## Review sequence

1. [Automatic runtime fallback and installed-consumer checks](https://github.com/currentspace/http3/pull/24).
2. [Bounded WASM writes, trailer ordering, and cancellation](https://github.com/currentspace/http3/pull/25).
3. [Native musl/Intel packages, ABI compatibility, and release gates](https://github.com/currentspace/http3/pull/26).
4. [Runtime/platform documentation and consolidated validation evidence](https://github.com/currentspace/http3/pull/27).

The PRs form a stack and should be reviewed/merged in that order. Root and
sidecar versions remain 0.9.9 during review; no new npm package is implied by
the candidate manifests. New sidecars still require their first-publication
bootstrap and Trusted Publisher setup before a release.

## Local evidence

- Stacked code candidate `13d8a31f93657872b0c02ceceac694cac6011bd3`:
  isolated npm installs with `--omit=optional` pass
  H2/H3 and raw QUIC auto fallback, strict-mode errors, facade runtime metadata,
  explicit WASM, and warning deduplication on macOS arm64 / Node 26.13.0.
- Hosted/packed candidate `74a7c977db89b79c0fb71630e55b13cbdfd7e261`,
  installed with its matching local native sidecar, selects
  native `fast`/`kqueue` without fallback. Explicit WASM also passes.
  The only change from that tree to `13d8a31` is test-fixture cleanup in
  `test/core/server-stream-lifecycle.test.ts`; production code, package
  manifests, and workflows are identical.
- `scripts/smoke-install.mjs` validates root/subpath exports and installed
  declarations under `node`, `node16`, `nodenext`, and `bundler`, plus both
  `worker` and `workerd` conditions without Node types.
- The final rebuilt candidate passed 571 Node core/runtime/interop/release
  tests, 67 FFI tests, and 135 WASM tests with zero failures, cancellations,
  or skips. Lint,
  both typecheck targets, and actionlint passed. Content hashes cover transfers through 128 MiB,
  uploads, raw QUIC, paused concurrent responses, trailer ordering, and resets.
  Three paused/resumed 32 MiB responses peaked at 9,043,968 bytes of WASM memory
  from an initial 1,966,080 bytes.
- The final macOS native addon was rebuilt before those tests. All 228 Rust
  unit tests and 72 Rust interop/mock tests passed. Both Clippy configurations
  passed with existing warnings. Fresh packed fallback/native installs and
  declaration checks passed, including 1,000 native portable GETs at concurrency
  50. Fresh-process regressions prove cancellation and failed raw QUIC binds
  release native callbacks without relying on forced process exit. UDP bind
  failures release their sockets; raw QUIC supports restart from its close event.
  [Final local results and artifact hashes](../.github/release-evidence/platform-coverage/final-local-validation.json).
- Native Alpine arm64: 54 transport tests, 6 ABI checker tests, and actual addon
  load passed with Rust 1.99.1, GCC 15.2.0, CMake 4.2.3, Alpine 3.24.3, and
  BoringSSL 5.3.0. Clean installed consumers on Node 24.22.1 and 26.12.1 pass
  both missing/native sidecar cases plus 1,000 portable GETs at concurrency 50.
  [Logs, toolchain receipt, and artifact hashes](../.github/release-evidence/platform-coverage/)
  retain the observed results. Separate hosted runs below qualify x64 and Intel
  macOS on their real architectures.
- The supplied 0.9.9 repros were run before changes using existing compiled
  artifacts. Missing native addons failed under auto; explicit WASM succeeded.
  A single WASM 32 MiB response reset with H3 code 268, while streamed data
  succeeded. Repro exit status alone was not treated as a pass.
- The unchanged supplied scripts also pass against fresh installs of final
  candidate `74a7c97`: auto serves H2/H3 with and without the native sidecar;
  the missing-sidecar case emits exactly one fallback warning. Single-buffer
  WASM downloads complete at 32 and 128 MiB, and the portable request-rate
  script completes. Output assertions reject errors/timeouts despite the
  scripts' unconditional zero exit status; the separate stream tests assert
  content hashes. [Final repro output](../.github/release-evidence/platform-coverage/final-supplied-repros.json).

An earlier local full-suite run timed out while connecting the native H3
fixture, before its 20 MiB response began. Its listener remained alive because
the fixture registered cleanup only after connection succeeded. Cleanup is now
registered before startup and the connection uses the test's abort signal;
a regression verifies both TCP and UDP ports can be rebound after cancellation.
The underlying one-off connection timeout cause remains unknown. The lifecycle
suite (31 tests), five focused repeats, the hosted run, and the final full local
run passed with natural process exit. This establishes cleanup and subsequent
successful runs, not a diagnosed transport-timeout fix.
[Sanitized failure history](../.github/release-evidence/platform-coverage/local-startup-timeout.json).

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

A follow-up interleaved comparison compiled archived `966c611` and `a812b37`
independently, used byte-identical native/WASM artifacts, and alternated fresh
consumer processes after warmup while development builds were idle:

| Workload | Baseline median | Candidate median |
| --- | --- | --- |
| Native portable 1,000 GETs, concurrency 50; five runs each | 10,113 requests/s | 10,010 requests/s |
| WASM streamed 32 MiB; three runs each | 91 MiB/s | 91 MiB/s |

Native ranges overlap (9,876–10,230 versus 9,745–10,736 requests/s); the native
mean changed by +0.42% and median by -1.02%. This short comparison shows no
material regression, with the limits of a single shared host. The previously
failing single-buffer WASM repro completes at 32 and 128 MiB; byte/hash integrity
is established separately by the stream tests.
[Full interleaved measurements](../.github/release-evidence/platform-coverage/interleaved-benchmark.json).

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

The added installed-native gate in [run 41148612055](https://github.com/currentspace/http3/actions/runs/41148612055)
found a GNU Linux x64 fast-driver startup failure on both Node majors. Its other
27 jobs passed and publication was skipped. The native error retained errno 22
but did not identify the failing setup operation; the JavaScript selector also
did not recognize the native structured error prefix. Follow-up changes preserve
setup stage/errno, return unavailable driver capabilities to the JavaScript policy
selector, and remove a hidden Rust fast-to-poll substitution that bypassed strict
policy and reported the wrong driver. Failed buffer-ring registration also
releases its memory mapping. Only recognized capability failures permit retry;
unrelated I/O and configuration errors remain errors.
This run is retained as a failure receipt, not final qualification.

[Final run 41151580339](https://github.com/currentspace/http3/actions/runs/41151580339)
passed at `74a7c977db89b79c0fb71630e55b13cbdfd7e261`: **29 successful jobs,
publication skipped**. It builds all six native targets and the shared WASM
artifact, runs ten no-addon platform/Node combinations, four Alpine combinations
with and without native sidecars, and eight GNU/macOS native combinations.
Node versions were 24.22.1 and 26.12.1. All twelve installed-native consumers
passed H2/H3, raw QUIC, and 1,000 portable GETs at concurrency 50.
[Run receipt](../.github/release-evidence/platform-coverage/final-hosted-run.json),
[native receipts](../.github/release-evidence/platform-coverage/hosted-native/),
[WASM/fallback receipts](../.github/release-evidence/platform-coverage/hosted-fallback/).

| Hosted environment | Observed native auto selection |
| --- | --- |
| GNU Linux arm64 | `fast` / `io_uring`, no fallback warning |
| GNU Linux x64, kernel 6.8.0-1070-azure | `portable` / `poll`; required buffer-ring registration returns `EINVAL` (22) |
| Alpine x64/arm64 containers | `portable` / `poll`; container policy denies `io_uring_setup` with `EPERM` (1) |
| macOS arm64 and Intel | `fast` / `kqueue`, no fallback warning |

The Linux fallback cases remain native and emit exactly one existing
`fast-path-unavailable` warning, with the actual errno and syscall. Each also
passes eight strict-mode rejection checks across H3/raw clients and servers;
strict modes never silently select poll. This is the acceptance clarification
in the plan: installing a musl addon avoids WASM fallback, but cannot guarantee
that the host permits io_uring. A separate real Linux seccomp fault test allows
ring creation and rejects only buffer-ring registration with `EINVAL`, proving
the same policy behavior on Node 24/26.
[Fault-test attribution and receipts](../.github/release-evidence/platform-coverage/hosted-native/pbuf-fault/summary.json).

The combined candidate is qualified; intermediate PR heads did not each run
automatic CI. The final follow-up contains only test-fixture cleanup and
documentation/evidence. Merging, selecting an unused release version, first
publication of the three new sidecars, Trusted Publisher setup, and clean
registry-install verification remain separate release work.
