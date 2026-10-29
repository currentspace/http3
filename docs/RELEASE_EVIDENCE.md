# Release Evidence

`CHANGELOG.md` supplies public release notes. This ledger records validation
and remaining qualification work for each release candidate.

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
- The local release wrapper stopped at browser sudo authorization. A separate
  user-keychain attempt timed out during certificate authorization; its temporary
  keychain was removed and the original search list restored. These attempts do
  not qualify the local browser gate or manual Safari gate.
- CodeQL identified disabled certificate validation in a new HTTPS test.
  The follow-up uses a temporary CA and leaves verification enabled for H3, H2
  and HTTPS; all six Node facade tests and ten Fastify fixture tests passed.
  Final-head hosted checks must confirm the finding is resolved.

### Pending release gates

- Local browser release gate, final-head hosted qualification, and versioned
  Linux/macOS Fastify integration checks.
- Manual Safari gate from `SAFARI_VALIDATION_RUNBOOK.md` and any final-head CI
  checks required before publication.
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
