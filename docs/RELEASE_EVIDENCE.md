# Release Evidence

`CHANGELOG.md` supplies public release notes. This ledger records validation
and remaining qualification work for each release candidate.

## 0.9.9

- Published as [v0.9.9](https://github.com/currentspace/http3/releases/tag/v0.9.9)
  at `aad10f2` after [PR #22](https://github.com/currentspace/http3/pull/22).
  Root, Rust and all three native sidecars are aligned to 0.9.9. All four npm
  packages report 0.9.9 under `latest`; registry tarballs match their published
  SHA-512 integrity values. The independent `canary` channel remains 0.8.3.
- Includes raw server cancellation and stream cleanup, bounded WASM payload
  allocation with application-visible errors, facade EOF cleanup, hot-path
  allocation/CPU reductions and public error/type exports.
- The versioned local gate passed 19 checks, including 452 Node, 114 WASM,
  67 FFI and 228 Rust unit tests, plus lint, TypeScript/workerd checks, native
  and WASM builds, concurrency/load gates and packed installs. Separate Rust
  interop checks passed. Browser checks ran on disposable hosted runners.
  After the test-only correction below, lint, types and all 452 Node/67 FFI
  tests passed again.
- Packed installs passed on Node 24 and 26 across four TypeScript resolution
  modes and both Worker conditions without Node types, with declaration checks
  enabled. See `docs/PUBLIC_TYPES.md`.
- Seven instrumented workloads completed 172,794 requests without recorded
  transfer errors; completed facade streams drained to zero. Profiling results,
  allocation tradeoffs and remaining hot spots are in `docs/HOTPATH_PROFILING.md`.
- Final candidate checks passed, including release dry-run
  [41113781172](https://github.com/currentspace/http3/actions/runs/41113781172).
  Merged-main Verify `41116337126`, CI `41116337175`, Distribution `41116337480`,
  Security `41116337329`, Rust Safety `41116337157`, Interop `41116337755` and
  Next/Nest `41116337322` all passed. Publication run
  [41124206102](https://github.com/currentspace/http3/actions/runs/41124206102)
  passed all three native platforms on Node 24/26, WASM qualification, uploads
  and final registry checks.
- Hosted macOS Node 26 exposed an obsolete write-after-end test assumption:
  server destruction can reset an unfinished response before its queued FIN
  arrives. The test now validates either completion or the expected stream
  error, retains the server write-error assertion, and always closes resources.
- Earlier queued-response drain, shutdown and WASM cancellation tests timed out
  on initial runs, then passed isolated and full reruns; the drain case passed
  20 repeats. The first versioned local run also timed out connecting a Fetch
  client, leaving a session open and failing the later shared-worker shutdown
  assertion. Isolated tests and the unchanged full rerun passed. Causes of
  these initial timeouts remain unproven.
- Intel macOS latency checks needed retries without code or threshold changes.
  An early Node 24 PR job measured p95 131.9 ms against 100 ms; the final
  candidate passed. Merged-main Node 26 measured p95 107.6 and 111.4 ms on its
  first two attempts (the second also had a 273.6 ms maximum against 250 ms).
  An isolated Intel diagnostic run passed three times at p95 23.9, 24.0 and
  18.2 ms; its CPU-profiled run passed at 21.0 ms. The final full Verify retry
  passed at p95 22.9 ms and maximum 26.5 ms. This does not establish the cause
  of the earlier outliers. Diagnostic artifacts are attached to
  [41121763433](https://github.com/currentspace/http3/actions/runs/41121763433).
- A candidate Kani retry-token proof exceeded its 60-second limit, then all
  19 harnesses passed an unchanged retry. Merged-main Rust Safety also passed.
- A fresh registry install loaded the matching macOS arm64 native sidecar,
  bundled WASM, CLI and integration entry points. The root contains no native
  binaries, docs or examples. Public root/WASM error constructors match and
  real rejected allocations narrow to `WasmMemoryError` with the expected code
  and requested byte count. Installed-package checks passed 62 native/framework
  tests and 43 mixed lifecycle/WASM tests on each of Node 24.22.1 and 26.13.0.
  The first Node 24 mixed run timed out in the native queued 20 MiB response
  test; all 29 lifecycle tests passed in isolation and the unchanged 43-test
  mixed rerun passed. That initial timeout remains unexplained.
- Manual Safari and Windows browser trust qualification remain incomplete.
  Publication retains the user's approved documented Safari limitation; no
  automated browser result is represented as manual Safari qualification.
  Release logs, profiles and registry readbacks are retained locally under
  `artifacts/releases/0.9.9/`.

## 0.9.8

- Candidate includes client response receive-credit flow control in native and
  WASM runtimes, cancellation of unfinished client streams, pending-write drain
  ordering, and quiet SSE proxy timeout handling. Root, Rust and all three
  native sidecars are aligned to 0.9.8.
- Before the version bump, macOS arm64 Node 26 passed 414 Node tests and 67 FFI
  tests; Node 24 passed 415 Node tests after the response-FIN regression was
  added. WASM passed 71 tests. Rust passed 228 unit tests and 17 targeted H3
  flow-control, concurrency, shutdown and mock-pair tests. Lint, TypeScript,
  workerd types and Node-API boundary checks passed.
- Paused 20 MiB downloads remain bounded in JavaScript, permit another stream
  on the same connection, and resume into a slow reader with matching SHA-256.
  Tests cover native/WASM blocked uploads and downloads, idle SSE cancellation,
  peer cancellation after response FIN, normal completion without a reset, and
  receive-credit retries when the native command queue is full. A Rust test
  exercises receive credit through the dedicated-worker command dispatcher.
- Offline candidate tarball installs loaded the matching native sidecar and
  WASM runtime. Paused H3 downloads buffered about 128 KiB in JavaScript;
  transport queues and QUIC windows hold additional bytes.
- Published as [v0.9.8](https://github.com/currentspace/http3/releases/tag/v0.9.8)
  at `47afad7` after PR #21. All four npm packages report 0.9.8 under `latest`;
  downloaded registry tarballs match their published SHA-512 integrity values.
- The versioned local gate passed 19 checks with no failures; browser checks
  were skipped locally and ran on disposable hosted runners. The separate Rust
  interop suite passed. Release dry-run
  [41077729812](https://github.com/currentspace/http3/actions/runs/41077729812)
  passed all three native platforms on Node 24/26 and the bundled WASM build.
- Release-commit Verify `41082011706`, Distribution `41082011969`, CI
  `41082011824`, Security `41082011637`, Interop `41082011708`, Rust Safety
  `41082011763` and combined Next/Nest `41082011757` passed.
  Two Intel macOS jobs needed unchanged retries: the PR Node 26 concurrency
  latency check initially measured p95 123.5 ms against 100 ms, then 44.4 ms;
  the merged-main Node 24 raw QUIC rapid-lifecycle test initially reported
  `shared quic client worker not running`. Both retries passed without code or
  threshold changes. These first-attempt failures are retained in the artifacts;
  their underlying causes have not been resolved by this release.
- Publishing run
  [41082068399](https://github.com/currentspace/http3/actions/runs/41082068399)
  built and validated all artifacts and npm accepted all four uploads. Its
  final registry check timed out waiting for Linux x64 and the workflow remains
  failed. The accepted package subsequently became available without another
  upload. Independent reads verified all four versions, `latest` tags, tarball
  integrity and package contents before creating the GitHub release.
- A fresh npm install loaded the matching macOS arm64 sidecar, bundled WASM,
  CLI and integration entry points. The root contains no native binaries,
  examples or docs. Installed-package streaming regressions passed 28 tests
  each on Node 24.22.1 and 26.13.0. A fresh registry-installed Next/Nest example
  passed 29 protocol/lifecycle tests in development and 29 in production.
- A follow-up symmetry check against the published package found that raw
  `ServerHttp3Stream.destroy()` does not cancel the peer in native or WASM mode,
  unlike client destroy and the Node response adapter. Explicit raw-server
  `close()` cancels the peer, but server tracking retains streams closed before
  request FIN until session teardown. These server lifecycle paths need a
  follow-up fix; they are not covered by the client cancellation changes.
- Manual Safari and Windows browser trust qualification remain incomplete.
  Release logs, registry verification and follow-up reproduction evidence are
  retained in `artifacts/releases/0.9.8/`.

## 0.9.7

- Includes configurable one-day Alt-Svc, socket compatibility, dual-stack/listen
  arguments, package/types fixes, Nest/proxy/shutdown helpers, dev certificates,
  diagnostics and documentation. Root, Rust and all three native sidecars are
  aligned to 0.9.7. Published as `v0.9.7` at `d2c9928`; all four packages
  report 0.9.7 under `latest`.
- The full local Node 26 suite passed
  397 tests and FFI passed 67. Node 24 also passed all 397 tests. Rust passed 279 tests plus targeted flow-control,
  shutdown and concurrent-stream integration tests. Rebuilt WASM passed 59 tests
  with no skips, including receive credit and ephemeral-port retries. Final-head
  hosted results are recorded on PR #20.
- Clean packed installs resolve types under node/node16/nodenext/bundler on both
  Node versions, load the freshly packed native sidecar and contain no root
  native binary duplicates. The root including WASM is approximately 3 MB unpacked.
- Actual Nest Express helper tests cover split cookies, SSE and shutdown hooks
  over H1/H2/H3. Hosted candidate Next/Nest tests pass on Node 24/26 in development
  and production, H3 and H2. Final-head platform checks are recorded in PR #20.
- A three-way Chromium idle comparison used 60s/86400s Alt-Svc and 30s/120s server
  QUIC timeouts. All first documents after 75s used H2. With one-day Alt-Svc, the
  next reload used H3 automatically. NetLog recorded a client-advertised 30s idle
  timeout and local closure at about 29s. A longer Alt-Svc lifetime is not a
  guarantee that the first idle reload uses H3.
- macOS's administrative trust-removal API hung on hosted runners. The CLI's
  macOS untrust operation instead applies an explicit deny policy to the generated
  CA; it retains the keychain entry and documents manual deletion. Trust settings
  are changed only by explicit trust/untrust commands. Hosted Distribution run
  `40995937710` passed all four Linux/macOS and Node 24/26 jobs, including browser
  rejection after untrust, automatic H3 negotiation and recovery after idle.
- Peer reset/bad-preface, bare response destroy and upstream SSE-abort tests
  pass. Paused 20 MiB uploads stay bounded, allow a concurrent request on the
  same session and resume with matching SHA-256 over H2/H3, native and WASM.
  Receive-credit tracking is released at body FIN even while the write side stays open.
  A deterministic Rust regression caught reset notification loss after FIN on
  an unread body; the receive-buffer fix preserves one reset without returning
  read credit. Native and WASM Fetch cancellation tests cover this path.
  The WASM lane forces garbage collection to verify Request signal lifetime;
  Fetch retains Requests in their live transports and error guards until close.
  Stock macOS LibreSSL generation/reuse and six concurrent certificate callers
  also pass. Distribution run `40999917830` passed all four Linux/macOS and
  Node 24/26 jobs with the SSL-only macOS trust policy.
- Native and WASM deterministic startup tests cover TCP collisions after UDP
  ephemeral binding, bounded retries, explicit-port errors and released sockets.
  The doctor TCP probe uses a plain connection; TLS trust is checked separately
  with verification enabled.
- Windows browser trust and manual Safari qualification remain unverified. Doctor
  is read-only and does not clear Chrome state. Release artifacts: `artifacts/releases/0.9.7/`.

- PR #20 merged at `7be72a1`. Its final implementation head `03387ca` passed
  all required hosted checks: Verify `41002754429`, Distribution `41002754459`,
  CI `41002754535`, Security `41002754446`, combined Next/Nest `41002754610`,
  Rust Safety `41002754490` and Interop `41002754421`. Browser trust checks
  ran on disposable hosted runners, without changing local machine trust.

### Publication qualification

- Release dry-run [41008592531](https://github.com/currentspace/http3/actions/runs/41008592531)
  and publishing run [41010050585](https://github.com/currentspace/http3/actions/runs/41010050585)
  passed at `d2c9928`. Both validated all three native packages on Node 24/26
  and built/tested the bundled WASM runtime. All hosted release-commit checks
  passed, including Verify `41010050462` and Distribution `41010050473`.
- Versioned local checks passed lint, typechecks, Node/FFI tests, Rust tests,
  concurrency/load gates and packed-install checks in all four TypeScript
  resolution modes. Browser trust checks ran on disposable hosted runners.
- Fresh npm registry installation loaded the 0.9.7 platform package, all public
  integration entry points and bundled WASM, with no native binaries, docs or
  examples in the root. Registry integrity and `latest` were checked for all
  four packages.
- Installed-package regressions passed 12 tests each on Node 24.22.1 and
  26.13.0, covering H2 resets, H1/H2/H3 SSE aborts, proxy handshake timeouts and
  paused 20 MiB native/WASM uploads with concurrent requests.
- A clean registry-installed Next/Nest example passed 29 protocol/lifecycle tests
  in development and 29 in production on Node 26.13.0, including split cookies,
  H1/H2/H3 routing, SSE, uploads and coordinated shutdown.
- [GitHub release](https://github.com/currentspace/http3/releases/tag/v0.9.7).
  Manual Safari and Windows browser trust qualification remain incomplete.

## 0.9.6

### Scope and state

- Include the split Cookie correction (`a1c7fa5`) and Express bridge with the
  combined Next.js/NestJS Express example (`8519a8e`).
- Root, Rust, and all three native sidecars are aligned to 0.9.6. The combined
  example requires 0.9.6 and is included in the root package tarball.
- The user authorized release of 0.9.6 with the example and both fixes.
- Published as tag `v0.9.6` at `ee87a98`; all four packages report 0.9.6.
  See the [release record](https://github.com/currentspace/http3/releases/tag/v0.9.6).

### Implementation qualification before the version bump

- macOS arm64: 240 core tests passed on Node 24.22.1 and 26.13.0, including
  mixed H1/H2/H3 requests to one Express app, redirects, and streamed proxy
  uploads through http-proxy-middleware 4.2.1.
- The full Node/FFI run passed 357 Node and 67 FFI tests before the additional
  proxy regression; that regression then passed on both Node versions.
- Eight Chromium 157.0.8103.15 combinations passed: Node 24/26, development
  and production, H3 and H2. Each H3 run observed two separate Cookie fields
  and verified values through Nest, Next, SSR, and Next-to-BFF H3 calls.
- H3/H2/H1 API tests, Rust tests, lint, typechecks, workflow validation, and
  the canonical Clippy lanes passed. Clippy retains existing warnings.
- Artifacts are under `artifacts/nestjs-investigation/`. These checks used
  the source candidate still labeled 0.9.5, not a published release artifact.

### Publication qualification

- All 30 PR checks and release dry-run
  [40982370278](https://github.com/currentspace/http3/actions/runs/40982370278)
  passed; publishing run
  [40984291721](https://github.com/currentspace/http3/actions/runs/40984291721)
  completed.
- Clean registry installation passed H1/H2/H3 API checks and eight combined-app
  Chromium combinations on Node 24/26, development/production, H3/H2.
- Manual latest-stable Safari qualification remains incomplete. The user
  explicitly approved publishing with that limitation documented. Playwright
  WebKit checks are not a Safari qualification pass.
- Post-merge verification passed on retry after an unchanged Hono example test
  initially encountered `EADDRINUSE` on an ephemeral port.

## 0.9.5 candidate

### Scope and state

- Preserve explicit native UDP limits and bound quiche PMTUD by local and peer
  transport limits, including discovery restart and revalidation.
- Fix Node facade header compatibility, static serving, streaming compression,
  HTTP/1 WebSocket upgrades and shutdown with an active upgraded socket.
- Fix HTTP/2 response buffer ownership so producers can safely reuse buffers
  after write completion. Next.js examples and dependencies are excluded.
- Root, Rust and all three native sidecars are aligned to 0.9.5.
- Base: `15c413899c14d01e38f22aca2aca942ebf23cbb1` (0.9.4).
- This is release preparation. No tag or npm publication is authorized by the
  candidate PR; merge and publication remain separate actions.

### Qualification

- Before the version bump, macOS arm64 / Node 24.22.1 passed 231 core tests,
  97 interoperability tests, six Next middleware tests, 223 Rust unit tests,
  20 mock transport tests and 22 PMTUD tests, plus lint and typechecks.
- Chromium verified hydration, state-preserving Fast Refresh, gzip, POST bodies,
  full RSC reads, client navigation and bounded shutdown over forced HTTP/3 and
  an HTTP/2 control. Each run recorded 20 responses and no failed requests or
  browser errors. This does not qualify automatic Alt-Svc discovery.
- Versioned macOS arm64 checks passed native build, frozen dependency install,
  lint, typechecks (including workerd), Node-API boundary, 269 Rust tests,
  Clippy (warnings only), packed-install smoke, concurrency and load smoke.
- Node 26 exposed two additional compatibility issues: HTTP/2 write callbacks
  released buffers before transmission, and the Writable pipe fast path bypassed
  public middleware write wrappers. Focused regressions reproduce these failures
  and pass with the fixes. Final validation passed 351 Node and 67 FFI tests on both
  Node 24.22.1 and 26.12.1.
- Local Next.js browser validation runs outside the PR. The Node 26 gzip failures
  disappeared after preserving middleware writes. One navigation still reports
  intermittent Chromium ERR_ABORTED despite successful same-document navigation
  and no console errors. An HTTP/2 netlog captured a complete 200 response,
  gzip-decoded body and END_STREAM for that exact navigation, without a server
  reset or network error. The strict harness result remains recorded separately
  from the functional and transport results; it is not called a full pass.
- Final strict Chromium runs passed HTTP/3 on Node 24 and 26, and HTTP/2 on
  Node 24, each with 20 protocol-verified responses and no failed requests or
  browser errors. The Node 26 H2 strict result above remains distinct.
- Hosted checks and the nonpublishing release workflow results are linked in the
  PR. Earlier runs on superseded revisions do not qualify the final candidate.
- Latest stable Safari manual qualification remains required by the release
  runbook before release. Prior 0.9.4 Safari evidence is not a 0.9.5 pass.

## 0.9.4 candidate

### Scope and state

- Experimental Node-style server factory for Fastify integration, exposed as
  `@currentspace/http3/node` without adding a framework dependency.
- Add request stream session metadata and fix shared HTTP/2 HEAD/trailer handling.
- Root, Rust and three native sidecar package versions are aligned to 0.9.4.
- Base: `6c4529d3eaddc18768bb2413d900517f633574c1`. No native transport logic or
  third-party dependencies change in this candidate.
- Publication is pending. Local implementation checks below do not constitute
  full release qualification.

### Completed implementation checks

- Lint and typecheck passed; the Node transport suite passed 341 tests.
- Fastify main at `5df959f3af698823dd582f1c3661f3e80df0b5da` passed its required
  tests: 2,388 unit tests passed, four skipped, and 1,286 type assertions passed.
- Ten real-protocol Fastify integration tests passed on macOS arm64 with
  Node 24.22.1 and 26.12.1, including static SPA hosting and H2/H1 fallback.
- Chromium 156 verified H3 SPA navigation and API requests with the local packed
  artifact. The origin was forced to QUIC and its temporary certificate pinned;
  this is not automatic Alt-Svc discovery evidence.
- These results used the development artifact before the version bump. Repeat
  package qualification against the versioned candidate and hosted artifacts.

### Versioned candidate checks

- Candidate `d80d117fc9ea32239ff6050cfd4d58fcec753fdb` passed
  [release dry-run 40826266471](https://github.com/currentspace/http3/actions/runs/40826266471):
  all three native builds, WASM build/tests, six Node 24/26 native validation
  jobs, packed installs and npm publication dry-run. Actual publication was skipped.
- Local versioned checks passed lint/typecheck, 341 Node tests, 67 FFI tests,
  concurrency (1,381 ms / 10,000 ms budget), load smoke, 223 Rust unit tests,
  three UDP and one direct interoperability tests, nine QUIC mock and eleven
  H3 mock tests, and native packed-install smoke.
- After certificate authorization, the local browser gate passed all three
  Chromium, Firefox and Playwright WebKit page/fetch/SSE tests over H3 on
  October 29, 2026. Earlier authorization failures remain recorded separately.
- CodeQL identified disabled certificate validation in a new HTTPS test.
  The follow-up uses a temporary CA and leaves verification enabled for H3, H2
  and HTTPS; all six Node facade tests and ten Fastify fixture tests passed.
  All 28 hosted PR checks passed at `f0ea679c28e3ee9fc291cf786a8577e2ceba764f`,
  with no open CodeQL PR findings; the scheduled deep-safety job was skipped.
- [Release dry-run 40827407812](https://github.com/currentspace/http3/actions/runs/40827407812)
  passed all 11 jobs at that revision. All native/WASM hashes match the tested
  artifacts. Actual npm publication was skipped.
- The versioned Fastify fixture at `0c1956809ffc61a2922c2bea57c73a9e96b9c82d`
  passed ten tests each on macOS ARM64, Linux ARM64 and native Linux x64, with
  Node 24.22.1 and 26.12.1. Native x64 evidence is
  [run 40828705200](https://github.com/currentspace/http3/actions/runs/40828705200).
  Both fixture server and clients explicitly select portable mode.
- Real Safari 27.0.1 on macOS 27.0.1 passed trusted loopback page, fetch, SSE
  reconnect with Last-Event-ID, and reload checks. Safari used H2 fallback.
  The actual Web Inspector Network panel was captured; curl H3-only and H2
  both passed against the same endpoint with certificate verification enabled.
  Evidence is in `artifacts/release-0.9.4/`.
- The Safari wrapper remains failed: final WebDriver session deletion returned
  HTTP 404. Functional checks passed and trust cleanup recorded no errors;
  temporary CA removal, original search-list restoration, and stopped server
  and driver were verified. The original failure record is preserved.
- This does not satisfy the latest-stable Safari prerequisite:
  [Safari 27.2 was released October 26](https://support.apple.com/en-us/149527).
  This Mac offers macOS 27.2 as a restart-required update; it was not installed.

### Pending release gates

- Repeat the manual Safari gate from `SAFARI_VALIDATION_RUNBOOK.md` on the
  latest stable Safari, including clean orchestration cleanup. Any subsequent
  source revision must complete its required hosted checks before publication.
- Clean registry install of the eventual published version before the Fastify
  example can be submitted as installable.

### Public limitations

The Node facade is experimental. TCP upgrades, WebSockets, CONNECT,
informational responses, native-object instanceof compatibility and Fastify's
TypeScript raw-server union are outside its supported surface. Native H3
shutdown uses the existing GOAWAY/deferred-close path and provides bounded,
best-effort draining rather than response acknowledgement guarantees. See
`NODE_SERVER.md` for the complete supported surface.

## 0.9.3

### Candidate and scope

- PR #14: dependency, native/WASM TLS, compiler, container, and CI refresh.
- The dependency qualification ledger is `docs/DEPENDENCY_UPDATE.md`.
- Root npm, three native sidecars, and Rust crate versions are aligned to 0.9.3.
- Linux x64 release builds and validation use Ubuntu 22.04; ARM64 uses 24.04.
  An artifact gate prevents either target requiring symbols above GLIBC_2.34,
  matching the published 0.9.2 native packages.
- macOS verification now installs NSS and exposes certutil for isolated Firefox
  profiles. The dependency candidate failed hosted browser tests without it.
- Restored Security and Rust Safety workflows disabled by GitHub inactivity.
- Stable publishing retains the established OIDC `latest` path. Canary mirroring
  is optional, not a release prerequisite; no new npm token is needed.

### Completed release qualification

- Qualified release source: `84335c235df9e4ffa1fc518cfde46d87517d487d`.
  Runtime code and dependency lockfiles are unchanged from the broader local
  qualification at `c97b3d17e53dbe68a884d0fdc4d8d9b8e1901979`; the follow-up fixes
  the x64 build environment and adds an artifact compatibility gate.
- Versioned local release tests passed: 335 Node tests, 67 FFI tests, three
  browsers, 223 Rust unit tests plus integration tests, packed install,
  concurrency (1249 ms / 10000 ms budget), load smoke, and 39 WASM tests.
  The enclosing storage guard returned 125 at its final check: system usage
  reached 94.03%, exceeding its conservative 94% threshold after tests completed.
  This is not an unqualified green wrapper run; the requested 95% ceiling held.
- [Qualified release dry-run 40755081567](https://github.com/currentspace/http3/actions/runs/40755081567)
  passed all 11 jobs: native builds on Linux x64, Linux ARM64, macOS ARM64;
  WASM build/tests; six Node 24/26 native validation jobs with packed installs;
  and dry-run publication of all four 0.9.3 packages. No registry writes occurred.
- Downloaded all four CI artifacts. Verified architectures and SHA-256 hashes,
  then packed the exact candidate with those binaries and checked that every
  embedded binary/WASM hash matched its downloaded artifact.
- Clean installed root and native-sidecar packages loaded expected exports and
  every public subpath, and instantiated WASM on macOS ARM64 with Node 24.22.1
  and 26.12.1. All 39 WASM tests also passed locally against the hosted artifact;
  its hash is unchanged in the qualified replacement run.
- Real Safari 27.0.1 on macOS 27.0.1 passed trusted loopback HTTPS page, fetch,
  SSE, automatic Last-Event-ID resumption, and reload checks. Captured the actual
  Web Inspector Network panel, showing HTTP 200 for document, fetch, and both
  SSE requests. The second SSE request is red because the fixture intentionally
  closes EventSource after its second message. Safari used HTTP/2 in this run.
- Curl HTTP/3-only and HTTP/2 both passed against the same manually inspected
  endpoint, `https://127.0.0.1:62802`, using `--cacert` without TLS bypass.
  Brian explicitly selected loopback or Docker staging. Temporary CA trust and
  the isolated keychain were removed, the original search list was restored,
  and the staging server/test windows were stopped.
- Receipts, screenshots, artifact and package hashes:
  [0.9.3 evidence bundle](../.github/release-evidence/0.9.3/README.md).

### Compatibility correction and security triage

Artifact inspection found that the first green dry-run's Linux x64 binary
required `__isoc23_strtoull@GLIBC_2.38` from BoringSSL 5's CPU environment parser.
Both published 0.9.2 Linux binaries require at most GLIBC_2.34. The x64 build and
runtime-validation runner has therefore moved from Ubuntu 24.04 to 22.04, and a
new release gate checks actual ELF version requirements. The first dry-run's
x64 artifact is **not approved for publication**. Replacement run 40755081567
passed the gate and runtime validation on Ubuntu 22.04. Both Linux artifacts now
require at most GLIBC_2.34. Mac ARM64, Linux ARM64, and WASM hashes are unchanged;
only Linux x64 was rebuilt differently. The
[corrected artifact receipts](../.github/release-evidence/0.9.3/abi-corrected/)
supersede the first run for release packaging.

The Security analysis/audit jobs and Rust Safety workflow passed. CodeQL's 17
Rust findings were individually reviewed against their source and SARIF flows:
the mandatory public
QUIC Initial salt, in-memory queue operations incorrectly classified as logging,
and test diagnostics containing only byte counts/booleans. Brian explicitly
approved dismissing alerts #32–48 as false positives; all 17 dispositions were
applied through `brianathere` and independently read back from GitHub. Reasons and
receipts are in the [CodeQL triage](release-evidence/0.9.3/CODEQL_TRIAGE.md).
No security query was disabled. All 27 workflow jobs passed on c97b3d1; check the
[PR's current checks](https://github.com/currentspace/http3/pull/14/checks) for the
final evidence-only commit before merging.

No publication, release tag, main-branch merge, or production deployment has been
performed as part of this preparation. Before publication, require green checks
on the final PR head, merge through the normal release process, and recheck
version/tag availability and Trusted Publisher permissions. A dry-run does not
exercise registry write authorization; the release wrapper performs another
dry-run from the eventual main-branch release source.

## 0.9.2

### Scope and validated source

- Base: merged PR #11, `da7a1c62a2feb1f85b88c63ac31a757c8fb1493f`.
- Its tree equals validated PR head `0e9537d930c51ca13c03ae675cde3e56628d135a`.
- Fix retired EventSource sessions scheduling extra reconnects, bounded SSE
  heartbeat writes, and cleanup after failed lifecycle tests.
- Release preparation aligns root, Rust, native-package, and lockfile versions.
- Extend npm dist-tag verification to ten minutes after the 0.9.1 publisher
  falsely failed its one-minute check during npm processing and CDN propagation.

### Completed implementation validation

- All 29 PR checks passed, including Linux, Docker, macOS ARM64 and Intel,
  browsers, WASM, interop, and package installation.
- Full verification: https://github.com/currentspace/http3/actions/runs/35009793316
- Previously hanging Intel macOS Node 24 job: 19 stages passed, zero failed;
  its expected embedded WASM skip is covered by dedicated WASM CI.
- Local native/runtime/interop/release tests: 335 passed; FFI tests: 67 passed.
- Normal and delayed reconnect cases: ten consecutive successful runs each
  on Node 24.20.0 and Node 26.8.1.
- Heartbeat accumulation and failed-assertion cleanup reproduced before fixes.
  Post-fix regressions, lint, and typechecks passed.
- Receipts: ignored `results/ci-hang/` directory and PR #11.

### Intel concurrency investigation

- Post-merge Intel Node 26 verification intermittently failed the concurrency
  gate, first exceeding its time budget and then timing out opening 50 clients.
  An unchanged-source diagnostic rerun passed in 10526 ms against 12000 ms:
  https://github.com/currentspace/http3/actions/runs/35013520180/job/104541154652
- Every H3 and raw QUIC client eagerly allocated 256 packet buffers of 65535
  bytes, even when its shared worker used a separate pool. Per-client pools
  now start empty; dedicated and direct/WASM clients allocate on first use.
- Three alternating x64 runs under Rosetta reduced mean peak memory from
  1414 MiB to 508 MiB and mean concurrency-suite time from 3971 ms to 3438 ms.
  This confirms the allocation cost; it does not prove the sole cause of the
  hosted timeout. Performance budgets and test timeouts are unchanged.
- Concurrency tests now close clients after failures. Fault injection proved
  that the next test previously inherited open clients; cleanup preserves the
  injected failure while removing the leaked-client failure in the next test.
- Changed-source local validation: 271 Rust unit, 335 native Node, 67 FFI,
  39 rebuilt WASM, and 13 concurrency tests passed; lint and compilation passed.
- Receipts and source/artifact hashes:
  ignored `results/release-0.9.2/investigation/` directory.

### Event-loop sampling correction

- PR #12 passed all 29 checks. Its Intel Node 24/25/26 concurrency gates took
  3876/4007/3243 ms against the unchanged 12000 ms limit; an independent Intel
  Node 26 run also passed. The merged tree exactly matched the PR candidate.
- Post-merge ARM64 Node 24 then failed the latency test with a p95 computed
  from only two gaps. All other concurrency cases passed. Publication was
  canceled before uploads. The old test could also pass with only one tick,
  discarded its first gap, and omitted an unobserved final interval.
- The corrected test warms the session, continues 50-request batches until it
  has at least 40 timer samples, and includes every measured gap, including
  the final interval. Limits remain 100 ms p95 and 250 ms maximum.
- Three full concurrency runs each on ARM64 Node 24, ARM64 Node 26, and x64
  Node 26 passed. Injected persistent 130 ms stalls failed the p95 check;
  a single 350 ms final stall failed the maximum check. Compilation and lint
  passed. Controls and logs are in the investigation receipt directory.

### macOS worker-reply scheduling correction

- The stronger latency test exposed additional ARM64 Node 25 and Intel Node
  24/25 failures. A matching optimized hosted diagnostic reproduced 119.8 ms
  p95 and a 243.5 ms maximum: successive synchronous `sendRequest` calls took
  roughly 10 ms each while CPU use remained low.
- Crossbeam 0.5.15's bounded `recv_timeout` uses scheduler-yield backoff before
  parking. A local scheduler-yield interposer captured that exact call path;
  adding a 10 ms yield delay reproduced 645.2 ms p95 and 649.7 ms maximum.
- macOS worker reply waits now use Crossbeam `Select`, which registers the
  receive and parks directly. Existing timeouts, disconnect behavior, queued
  replies, command queues, and non-macOS receive behavior are preserved.
- With the same injected yield delay, the corrected optimized addon passed
  at 8.0 ms p95 and 8.5 ms maximum, with zero main-thread scheduler yields.
- Five response-semantics tests cover queued replies, delayed replies,
  sender disconnect, timeout cleanup, and 1000 repeated handoffs. All 276 Rust
  unit tests, 335 native Node tests, 67 FFI tests, Clippy, and lint passed.
- Three optimized concurrency runs each on ARM64 Node 24/25/26 and Intel
  Node 26 passed with unchanged thresholds. Hosted qualification of this
  correction is still required before publishing.

### Release qualification status

The 0.9.2 dry run, publication build matrix, registry verification, and clean
published-package install remain gates. Their receipts are retained under
`results/release-0.9.2/`; version preparation alone does not complete them.

## 0.9.1

### Scope

- Base: `7f41ad3` on `main`, following release `v0.9.0`.
- Streaming Fetch uploads, request-size limits, and disconnect cleanup.
- WASM queued-write correctness, peer-credit enforcement, and prevention of
  repeated `DATA_BLOCKED`/ACK exchanges.
- Shared lifecycle helpers, extracted worker modules, and CI consolidation.
- Version alignment across the root npm package, Rust crate, native sidecars,
  and lockfiles.

### Local validation — 2026-09-15

Host: macOS ARM64, Node 26.8.1. Final implementation checks:

| Check | Result |
| --- | --- |
| Native TypeScript and FFI | 398 passed |
| Rust unit, mock, Loom, interop, and WASM ABI | 295 passed |
| WASM runtime suite | 39 passed in each of five consecutive runs |
| Shared interop with WASM clients | 23 passed |
| Application end-to-end | 34 passed |
| Concurrency and load smoke | 14 passed |
| Lint, typechecks (including workerd), Rust clippy | Passed; existing Rust warnings remain |
| Native/WASM builds and package installation smoke | Passed |

The WASM upload regressions retain small receive windows and the 64 MiB
module memory limit. New tests check exact body bytes and FIN, deliberately
delay the server session event, and bound packet amplification.

Local receipts are retained under the ignored `results/wasm-fixes/` directory:
`REPORT.md`, `final-results.json`, and `source-and-artifact-sha256.json`.
The pull request records the additional release-metadata validation.

### Remaining release qualification

Browser, Docker/Linux, longhaul, sanitizer, fuzz, and formal-verification
suites were not rerun against the final flow-control fixes. The canonical
local run explicitly skipped browser E2E and its embedded WASM step; WASM
was built and tested separately. GitHub Actions results and the full publish
validation remain release gates. Opening this PR does not complete them.

## 0.9.0

This document is the supporting audit ledger for `0.9.0`. It captures the
release story behind the WASM client/server runtime.

`CHANGELOG.md` is the public release-note source; this file records the working
evidence behind that release entry.

### Scope

- Base tag: `v0.8.6`
- Release framing: WASM runtime release (client and server)
- Evidence sources:
  - design doc: `docs/WASM_CLIENT_PLAN.md`
  - usage guide: `docs/WASM_RUNTIME.md`
  - wasm ABI crate: `crates/http3-wasm/`
  - TS wasm runtime: `lib/wasm/`, `lib/client-event-loop-factory.ts`, `lib/wasm-event-bridge.ts`
  - workerd verification: `examples/workerd-client/`
  - proof-friendly retry-token/pool models: `src/proof_core/`
  - Kani harnesses/fuzz target for the above: `src/proofs/kani_harnesses.rs`, `fuzz/fuzz_targets/retry_token_roundtrip.rs`
  - detached-promise cleanup: `lib/run-detached.ts` and its call sites
  - release metadata: `package.json`, `Cargo.toml`, `npm/*/package.json`, `CHANGELOG.md`

### Release framing

This delta is best described as a WASM runtime release:

- the client (HTTP/3 and raw QUIC) and, Node-only, the server now run on a
  `wasm32-wasip1` build of the same quiche + BoringSSL protocol core used
  natively, reachable via `runtimeMode: 'wasm'`
- the client build is verified running inside real Cloudflare workerd, not
  just Node — the only remaining gap is workerd's own lack of an outbound UDP
  client socket API
- two real bugs (a pool-bucketing inefficiency, a retry-token panic on
  hostile input) surfaced by writing Kani proofs for the server-side code
  this work touched, both fixed and now proof-covered
- a systemic `void`-detached-promise pattern in `lib/` (no rejection
  handling, no way for shutdown to wait for background work) was replaced
  with a drainable task registry

### Downstream-visible outcomes

#### WASM runtime is a first-class `runtimeMode`

Evidence:

- `lib/client.ts`, `lib/quic-client.ts` (`connect()`/`connectAsync()`/`connectQuic()`/`connectQuicAsync()`)
- `lib/server.ts`, `lib/quic-server.ts` (`Http3SecureServer.listen()`/`QuicServer.listen()`)
- `lib/client-event-loop-factory.ts` (native/wasm branch, lazy `import()` so native-only consumers never load wasm code)
- `crates/http3-wasm/src/{h3,quic,h3_server,quic_server}.rs` (the `h3c_*`/`qc_*`/`hs_*`/`qs_*` extern-C ABI)

Outcome:

- `runtimeMode: 'wasm'` works end to end for both HTTP/3 and raw QUIC, both
  client and server, verified across the full native x wasm x client x
  server x QUIC x HTTP/3 matrix (8 cells), including a wasm client talking
  to a wasm server over real loopback UDP with zero native code involved
- servers remain Node-only by design (N1) — workerd has no inbound-listening-
  socket model, so a "workerd server" isn't a coherent concept

#### Verified inside real Cloudflare workerd, not just asserted

Evidence:

- `examples/workerd-client/worker.ts`, `wrangler.jsonc`, `README.md`
- `lib/wasm/index.workerd.ts`, `lib/wasm/wasi-shim.ts` (host-agnostic — no `node:wasi`, no Buffer, no `node:*`)

Outcome:

- the compiled `http3_client.wasm` artifact instantiates under real
  `wrangler dev`/`workerd`, its full ABI export surface resolves, and
  `WasmH3ClientEventLoop.connect()` generates a real, valid 1200-byte QUIC
  Initial packet inside workerd's own V8 isolate
- `wrangler deploy --dry-run` reproduces cleanly (bundle size confirmed)
- real network handshakes are blocked purely by workerd's own missing
  outbound-UDP API (cloudflare/workerd#4463), not by anything in this package

#### Server-side retry-token/connection-routing logic is wasm-compatible and proof-covered

Evidence:

- `src/retry_token.rs` (HMAC-SHA256 via `boring`, replacing `ring` for the server-side token path)
- `src/proof_core/retry_token_model.rs` (payload build/parse, extracted from
  duplicated code in `src/connection_map.rs` and `src/quic_worker.rs`)
- `src/proofs/kani_harnesses.rs` (round-trip correctness, no-panic-on-
  arbitrary-bytes, clock-skew regression — all proven, not just example-tested)
- `fuzz/fuzz_targets/retry_token_roundtrip.rs` (real HMAC-integrated
  `ConnectionMap` path, coverage-guided mutation, 1.69M runs / 46s locally
  with no crashes)

Outcome:

- the retry-token clock-skew check's `i64`-cast-and-`.abs()` panic on
  hostile input (found by writing the no-panic Kani proof) is fixed with
  `u64::abs_diff`, proven correct over the full `u64` domain
- the two server implementations' previously-duplicated token parsing logic
  now has one source of truth

#### `buffer_pool.rs`'s checkin bucketing bug is fixed and proof-covered

Evidence:

- `src/proof_core/buffer_pool_model.rs`, `src/proof_core/chunk_pool_model.rs`
- `src/proofs/kani_harnesses.rs` (`*_returns_largest_class_leq_cap`, `*_accepts_every_*_allocation`)

Outcome:

- `class_for_capacity` now returns the largest class `<=` capacity (matching
  `chunk_pool.rs`'s existing fix for the identical bug shape) instead of
  reusing the checkout-side "smallest class `>=`" classification, which
  filed a buffer into a bucket whose declared capacity it didn't meet

#### No promise in `lib/` is fire-and-forget without a rejection handler or a shutdown drain

Evidence:

- `lib/run-detached.ts` (`runDetached`, `DetachedTasks`)
- `lib/client.ts`, `lib/quic-client.ts`, `lib/server.ts` (own a `DetachedTasks` registry, drain it in `close()`/`destroy()`)
- `lib/eventsource.ts` (`_startConnection()` gained the try/catch its sibling `_finalizeClose()` already had)

Outcome:

- every `void asyncCall()` site in `lib/` (constructors, event-handler
  callbacks, timers) now either routes its rejection through the object's
  own error-reporting path, or is tracked in a registry the object's own
  `close()`/`destroy()` awaits before completing
- `pnpm run test:core` previously hung 40+ minutes after every test had
  already passed, with no visible cause until the process was killed; after
  this fix it exits in under 10 seconds — the hang was exactly this class of
  bug (a test's own detached background work outliving the test)

### Caveats To Disclose

- Real outbound UDP from Cloudflare Workers/workerd does not exist yet
  (cloudflare/workerd#4463); the workerd verification in this release proves
  module instantiation, ABI resolution, and in-memory protocol-core
  correctness, not a live network handshake from a deployed Worker.
- The wasm ABI crate is client-and-server-capable, but only the client half
  is reachable from workerd; the server half is Node-only by design.
- `crates/http3-wasm/src/abi.rs`'s pointer/length trust boundary
  (`bytes_in`/`write_out_ptr_len`) is enforced by the TypeScript caller, not
  provable in Rust alone — a caller bug there is a real OOB risk in wasm
  linear memory, same as any FFI boundary.
- No `NPM_TOKEN` repository secret is configured; the `latest` release's
  canary-dist-tag mirror step is skipped (logged, non-fatal) rather than
  failing outright. The core publish itself uses npm Trusted Publisher
  (OIDC, `--provenance`), unaffected by this.

### Release-Blocking Checks

- Full local release gate: `npm run release:local-gate`
- Dry-run publish validation: `npm run release:latest -- --validate-only --dist-tag latest`

Validated in this release pass:

- `cargo test --lib --no-default-features` (223/223)
- `pnpm run test:rust:mock:extended` (11/11 integration test binaries)
- `cargo clippy --lib --no-default-features --features os-runtime,node-api`
- `cargo check --no-default-features --features wasm-abi`
- Kani: 19 always-run harnesses + 3 deep harnesses (`HTTP3_KANI_DEEP=1`), all passing
- `cargo +nightly fuzz run retry_token_roundtrip -- -max_total_time=45` (1.69M runs, no crashes)
- `npx napi build --platform --release`
- `pnpm test` (core + runtime + interop + release + ffi, 349 tests)
- `pnpm run lint`, `pnpm run typecheck`
- Full GitHub Actions CI on PR #9: 38/38 checks green, including every
  `verify (macos-15-intel, kqueue)` job (previously intermittently flaky)

### 0.8.4 Changelog Entry

- Refactored unsafe-adjacent Rust logic into proof-friendly pure models for outbound admission, pending writes, connection IDs, recv-buffer accounting, cmsg cursor walking, and io_uring provided-buffer layout.
- Added Kani contracts and bounded harnesses for admission accounting, pending-write release accounting, cmsg cursor bounds, recv-buffer capacity, QUIC-LB CID encoding, and provided-buffer range validation.
- Added standalone Verus sidecar proofs with a bootstrap/smoke path that installs and verifies against the latest Verus binary locally.
- Added structured fuzz targets, Miri smoke coverage, sanitizer scripts, and a Rust safety CI workflow for the new proof/fuzz lanes.
- Fixed deferred-FIN outbound admission accounting so a full payload write whose FIN is not accepted keeps one admission unit held until the FIN is accepted.
- Fixed the Docker curl interop harness so header capture no longer depends on `/dev/stderr` being openable by a spawned curl process.
- Bumped the package line to `0.8.4`, including Cargo metadata, native sidecar package manifests, and root optional sidecar pins.
- Updated the minimum supported Rust version and Clippy MSRV setting to `1.95`.
- Hardened npm release publishing so the latest/canary dist-tag mirror can use either `NPM_TOKEN` or `NODE_AUTH_TOKEN`.

### Historical 0.6.0 Evidence

### 0.6.0 Changelog Entry

- Added first-class raw QUIC client mTLS support through the public `connectQuic()` and `connectQuicAsync()` options, including `cert`/`key` validation and explicit `ERR_HTTP3_TLS_CONFIG_ERROR` failures for invalid TLS input.
- Added raw QUIC server-side client certificate policy control with `clientAuth: 'none' | 'request' | 'require'`, defaulting to `require` whenever a client-verification `ca` is configured.
- Added peer-certificate inspection on `QuicServerSession` with `peerCertificatePresented`, `getPeerCertificate()`, and `getPeerCertificateChain()` so applications can pin exact client certificates after CA verification.
- Updated the raw QUIC API contract, configuration reference, and QUIC guide with explicit mTLS examples, server policy semantics, and a documented certificate-pinning pattern.
