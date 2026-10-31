# Platform coverage implementation evidence

Updated: 2026-10-31. The implementation is merged and published to npm as
1.0.0 from `9655393063f8639d5a8c312850d06e4fcba1f1eb`. This ledger distinguishes
the earlier candidate checks from publication and registry qualification.
Source requirements are in
[PLATFORM_COVERAGE_PLAN.md](./PLATFORM_COVERAGE_PLAN.md).

## Review sequence

1. [Automatic runtime fallback and installed-consumer checks](https://github.com/currentspace/http3/pull/24).
2. [Bounded WASM writes, trailer ordering, and cancellation](https://github.com/currentspace/http3/pull/25).
3. [Native musl/Intel packages, ABI compatibility, and release gates](https://github.com/currentspace/http3/pull/26).
4. [Runtime/platform documentation and consolidated validation evidence](https://github.com/currentspace/http3/pull/27).

The PRs were merged in that order. Root, Rust and all six native sidecars were
then aligned to 1.0.0; bootstrap and Trusted Publisher setup for the three new
sidecars were completed before stable publication. The checks below at 0.9.9
remain historical candidate evidence, separate from the 1.0.0 results.

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

[Implementation qualification 41151580339](https://github.com/currentspace/http3/actions/runs/41151580339)
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

Intermediate PR heads did not each run automatic CI. The combined candidate
was qualified before merge, and versioned hosted checks ran after Actions was
restored. The original enabled permissions and 13 workflow states were restored;
the five newly added workflows bring the current total to 18 active workflows.

## Published 1.0.0 evidence

[Release 41233850938, attempt 2](https://github.com/currentspace/http3/actions/runs/41233850938/attempts/2)
published all seven npm packages from `9655393063f8639d5a8c312850d06e4fcba1f1eb`.
All 30 latest job results passed: 28 successful jobs were inherited from attempt
1, and the Intel Node 24 validator and publisher ran successfully in attempt 2.
The seven build artifact IDs, archive hashes, and native/WASM bytes were
unchanged across that targeted retry. All seven `latest` tags now identify
1.0.0; existing `canary` tags are unchanged.

Attempt 1's Intel Node 24 runtime step stalled and was cancelled before the
publisher executed. GitHub supplied neither its job/step log nor an entry in
the run-log archive; its annotation only records the cancellation. The cause
and final test/cleanup state remain unknown. At the unchanged source, the retry
passed 73 runtime tests in 27.18 seconds, 44 streaming/cancellation tests in
16.62 seconds, packed installation, and the installed native H2/H3/raw QUIC
and 1,000 portable GET scenario. This establishes a successful retry without
claiming a diagnosed cause for the original stall.

Independent anonymous registry downloads passed SHA-512/SHA-1 integrity,
package-layout/export/declaration checks, exact optional-version pins, and
sidecar platform metadata checks for all seven packages. All six native
binaries and the WASM core match the actual publishing artifacts byte for
byte. A separate cryptographic signature/attestation audit verified all seven
packages with no invalid or missing signatures, including provenance for the
publishing source and workflow attempt.

Fresh registry consumers passed 13 type/export/CLI checks per Node major and
single 32/128 MiB WASM byte/hash checks on Node 24 and 26. On each major,
standalone Nest passed 28 tests and the combined Next/Express/Nest application
passed 29 development and 29 production tests. The copied Nest assertion
initially expected obsolete `Alt-Svc` `ma=60` and was corrected to the documented
`ma=86400` contract; the published library and lockfile were unchanged. The
failed runs, fixture diff and hashes are retained with the successful results.

The separate [registry consumer matrix](https://github.com/currentspace/http3/actions/runs/41236654798)
passed all 29 jobs: an input validator and 28 cells covering seven targets,
two Node majors and two install profiles. Independent receipt inspection
verified all 12 native and 16 WASM outcomes, registry 1.0.0/source `9655393`,
registry URLs/integrity, and every installed-file hash and size against the
independently downloaded tarballs. Receipts include actual H2/H3/raw QUIC output
and the native 1,000-GET scenario; downloaded artifact archive hashes match
GitHub's digests.

The x64 musl bootstrap was successfully published from GitHub Actions even
though its workflow later failed a tarball comparison: manifest/README file
modes differed between the private preparation directory and the publishing
helper. Repacking reproduced both hashes; independent complete-payload,
native-byte, integrity and cryptographic provenance audits passed. That failed
workflow and the earlier bootstrap, fixture and qualification failures remain
recorded in [RELEASE_EVIDENCE.md](./RELEASE_EVIDENCE.md#100). Available sanitized
receipts are retained under `artifacts/releases/1.0.0/`.

The [v1.0.0 GitHub release](https://github.com/currentspace/http3/releases/tag/v1.0.0)
is published, with its annotated tag resolving to the exact npm source commit.
Its [public evidence asset](https://github.com/currentspace/http3/releases/download/v1.0.0/release-evidence.json)
records the run/artifact identities, hashes and qualification summaries.
Manual Safari remains unqualified under the user's explicitly approved
documented release limitation.
Windows browser trust also remains unqualified. Automated WebKit and Windows
Node/WASM results do not establish those browser qualifications.
