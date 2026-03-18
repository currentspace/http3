# Release Evidence

This document is the supporting audit ledger for `0.5.0`. It captures the real
`v0.4.0..HEAD` delta, the evidence behind release-note claims, and the caveats
we should keep visible for downstream users.

`CHANGELOG.md` is the public release-note source; this file is the evidence
behind that release entry.

## Scope

- Base tag: `v0.4.0`
- Audited commits:
  - `5cdf143` `finish io_uring benchmarking and shared fast client reuse`
  - `37a9a63` `align shared client reactors across QUIC and H3`
  - `fb15b47` `add cross-platform perf campaign tooling`
  - `31a4921` `fix QUIC bottlenecks and add targeted profiling`
  - `e66ff4e` `add QUIC attribution harnesses and reduce auto fallback churn`
- Evidence sources:
  - public API/docs: `README.md`, `docs/RUNTIME_MODES.md`, `index.d.ts`, `lib/runtime.ts`
  - test lanes: `test/runtime/`, `test/interop/`, `test/release/`, `test/perf-gates/`
  - perf artifacts: `perf-results/quic-bottlenecks/`, `perf-results/mock-bridge-scale/`
  - analyzer and findings guide: `scripts/analyze-perf-results.mjs`, `docs/PERF_PROFILING.md`

## Release framing

This delta is best described as release hardening plus observability and
performance-attribution work, not a brand-new API chapter.

The user-visible wins are:

- shared fast-client ownership is now aligned across raw QUIC and H3
- runtime fallback is less noisy and more predictable in unsupported Linux environments
- perf investigation is now repeatable across host, Docker, macOS, loopback, quiche-direct, and Node mock-transport harnesses
- raw QUIC regression cases now have reproducible tests plus committed before/after artifacts

## Downstream-visible outcomes

### Shared fast-client topology is explicit and tested for both protocols

Evidence:

- `test/runtime/quic-fast-shared-worker.test.ts`
- `test/runtime/h3-fast-shared-worker.test.ts`
- `README.md`
- `docs/RUNTIME_MODES.md`

Outcome:

- fast client lanes reuse one worker and one local UDP port per bind family
- QUIC and H3 now describe the same shared-client ownership model in docs, tests, and benchmark telemetry

### Auto fallback now caches fast-path unavailability for the life of the process

Evidence:

- `lib/runtime.ts`
- `test/runtime/runtime-selection.test.ts`

Outcome:

- unsupported Linux environments stop re-probing `io_uring` on every `auto` connection attempt
- fallback behavior stays observable through `runtimeInfo`, runtime events, and warning metadata

### Cross-platform perf tooling became a first-class workflow

Evidence:

- `scripts/quic-benchmark.mjs`
- `scripts/h3-benchmark.mjs`
- `scripts/docker-quic-benchmark.mjs`
- `scripts/docker-h3-benchmark.mjs`
- `scripts/profile-linux-benchmark.mjs`
- `scripts/profile-macos-benchmark.mjs`
- `scripts/profile-rust-quic-loopback.mjs`
- `scripts/profile-rust-quic-direct.mjs`
- `scripts/profile-node-mock-transport.mjs`
- `docs/PERF_PROFILING.md`

Outcome:

- QUIC/H3 host runs, Docker matrices, Linux/macOS profiler wrappers, and attribution harnesses now write comparable artifacts
- release notes can point to real persisted evidence instead of one-off benchmark logs

### Raw QUIC bottleneck debugging now has reproducible regression evidence

Evidence:

- `perf-results/quic-bottlenecks/`
- `docs/PERF_PROFILING.md`
- `test/interop/quic-stress.test.ts`

Outcome:

- timer-deadline and reap regressions can be checked against known-good artifacts
- the release can explain what improved and why with evidence instead of only with commit messages

## Verified findings

### Shared fast-client reuse flattens setup work on fast lanes

The committed analyzer output for `perf-results/quic-bottlenecks` shows the
same pattern across the aligned fast-client lanes:

- Linux H3 fast client groups: `driverSetups=2`, `sharedReuses=18`, `portReuse=18`
- Linux QUIC fast client groups: `driverSetups=2`, `sharedReuses=18`, `portReuse=18`
- Linux H3 portable client groups remain the outlier at `driverSetups=20-40`, `sharedReuses=0`, `portReuse=0`

This is the clearest release-level proof that shared fast-client ownership is
working and that Linux portable H3 is still the main client-topology caveat.

### The raw-QUIC regression is fixed

`docs/PERF_PROFILING.md` records the pre-fix `unconfined server portable,
client fast` lane at:

- `950/1000` completed streams
- `50` timeouts
- `p95=91.91ms`
- `rawQuicClientReapsWithKnownStreams=1`

The same doc records the post-fix lane at:

- `1000/1000` completed streams
- `0` errors
- `p95=15.53-18.63ms`
- `rawQuicClientReapsWithKnownStreams=0`

That is strong enough to support a changelog claim about a real correctness and
tail-latency fix, not just a profiling cleanup.

### H3 still trails like-for-like QUIC controls

`docs/PERF_PROFILING.md` records two representative mixed-shape comparisons:

- `H3 server portable, client fast`: about `129.8 Mbps` versus QUIC's `158.7 Mbps`
- `H3 server fast, client portable`: about `146.3 Mbps` versus QUIC's `167.6 Mbps`

The important release takeaway is that the remaining gap is still above raw
transport. The current docs should present the new tooling and topology fixes as
hardening work, not as proof that protocol-level overhead is gone.

### The attribution harnesses can separate Rust batching cost from TSFN and JS work

The committed analyzer output for `perf-results/mock-bridge-scale` shows
distinct `mock-transport` groups for:

- `counting`
- `tsfn-noop-js`
- `tsfn-real-js`

Those harness families matter because they let us ask a much better release
question: is a regression in the Rust protocol path, the worker/reactor path, or
the JS callback shape?

## Caveats To Disclose

- Do not market H3 as performance-equivalent to raw QUIC yet.
- Linux portable H3 client lanes are still the main topology outlier.
- The perf campaign is artifact-driven and manually reviewed; only the concurrency/load smoke lanes are CI gates.
- `tests/transport_quiche_pair.rs` and `tests/release_curl_standalone.rs` are valuable diagnostics, but they are not part of the default release-blocking CI bundle.

## Release-Blocking Checks

- Full local release gate: `npm run release:local-gate`
- TypeScript lane bundle: `npm test`
- Browser lane: `npm run test:browser:e2e`
- Perf gates: `npm run perf:concurrency-gate` and `npm run perf:load-smoke-gate`
- Rust release-blocking lanes: `npm run test:rust`
- Packed install smoke: `npm run smoke:install`
- Manual release validation: `docs/SAFARI_VALIDATION_RUNBOOK.md`
- Manual perf review when transport topology changes: `npm run perf:analyze -- --results-dir perf-results`

## 0.5.0 Changelog Entry

- Align fast-client worker ownership across raw QUIC and H3 so fast Linux/macOS client lanes reuse one worker and one UDP port per bind family instead of scaling setup with session count.
- Reduce `auto` runtime fallback churn by caching fast-path unavailability for the lifetime of a process, so restricted Linux environments stop repeatedly probing `io_uring`.
- Add a repeatable cross-platform perf campaign with host and Docker benchmark artifacts, Linux/macOS profiler wrappers, and attribution harnesses for quiche-direct, loopback, and Node mock-transport analysis.
- Fix the raw-QUIC timeout/reap bottleneck that previously dropped a 1000-stream unconfined lane to `950/1000`; the committed post-fix artifacts return `1000/1000` with materially lower tail latency.
- Keep one important caveat explicit: like-for-like H3 lanes still sit below raw QUIC controls, so this release should be framed as hardening and observability work rather than as the end of all higher-layer overhead.
