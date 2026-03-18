# Cross-Platform Profiling and Benchmarking

This runbook defines the repeatable performance campaign for raw QUIC and H3
across Linux `io_uring`/`poll` and macOS `kqueue`.

## Goals

- Use one artifact shape for QUIC and H3 host benchmarks.
- Keep Docker matrix results comparable with host runs.
- Capture profiler outputs next to the benchmark JSON that explains them.
- Compare topology policy separately from backend choice, especially on macOS.

## Artifact model

All benchmark and profiling commands can write timestamped artifacts under
`perf-results/` or another directory you provide with `--results-dir`.

Current artifact types:

- `benchmark-summary`: host QUIC or H3 benchmark output, including:
  - environment metadata
  - full benchmark settings
  - per-round summaries
  - per-client raw results
  - runtime selections
  - reactor telemetry
  - server lifecycle summary fields such as `sessionCount`, `sessionsClosed`, and `activeSessions`
- `docker-benchmark-matrix`: Docker lane matrix for QUIC or H3, including each
  success/failure lane plus the embedded benchmark summaries from successful runs
- `profiler-manifest`: Linux/macOS wrapper output describing the profiler tool,
  command, and profiler artifact paths written alongside the benchmark JSON
- `quic-loopback-profile`: native Rust QUIC loopback artifact with separate
  client/server summaries, sink stats, and reactor telemetry
- `quic-direct-profile`: direct `quiche` floor artifact with no worker/reactor
  wrapper
- `quic-mock-profile`: Node mock-transport artifact with Rust summary plus JS
  callback counters

The analyzer consumes the persisted `benchmark-summary` and
`docker-benchmark-matrix` artifacts, plus the Rust loopback, quiche-direct, and
Node mock-transport profiling artifacts:

```bash
npm run perf:analyze -- --results-dir perf-results
```

## Commands

### Host benchmark artifacts

```bash
npm run bench:quic -- --profile smoke --results-dir perf-results --label quic-smoke
npm run bench:h3 -- --profile smoke --results-dir perf-results --label h3-smoke
```

### Docker matrix artifacts

```bash
npm run bench:quic:docker -- --results-dir perf-results --label quic-docker
npm run bench:h3:docker -- --results-dir perf-results --label h3-docker
```

Add `--include-privileged` when you want the broader privileged confirmation lane:

```bash
npm run bench:quic:docker -- --results-dir perf-results --label quic-docker --include-privileged
npm run bench:h3:docker -- --results-dir perf-results --label h3-docker --include-privileged
```

### Linux profiler wrappers

Use these when you want benchmark JSON and profiler artifacts produced together.

```bash
npm run perf:linux:quic -- --perf-stat --profile throughput --results-dir perf-results --label quic-linux
npm run perf:linux:h3 -- --perf-record --strace --profile stress --results-dir perf-results --label h3-linux
npm run perf:linux:quic:loopback -- --runtime-mode fast --results-dir perf-results --label quic-loopback
npm run perf:linux:quic:direct -- --results-dir perf-results --label quic-direct
npm run perf:node:quic:mock -- --sink-mode counting --callback-mode noop --results-dir perf-results --label quic-mock-counting
npm run perf:node:quic:mock -- --sink-mode tsfn --callback-mode noop --results-dir perf-results --label quic-mock-tsfn
npm run perf:node:quic:mock -- --sink-mode tsfn --callback-mode touch-data --results-dir perf-results --label quic-mock-js
```

Notes:

- `--perf-stat` is for aggregate CPU counters.
- `--perf-record` is for sampled CPU profiles.
- `--strace` uses `strace -fc` and is meant for setup-cost and syscall-mix
  validation, not as the primary throughput metric.
- `--docker` can wrap the Docker matrix runner when you want the same artifact
  layout for container campaigns.

### macOS profiler wrappers

```bash
npm run perf:macos:quic -- --sample --profile throughput --results-dir perf-results --label quic-macos
npm run perf:macos:h3 -- --xctrace --profile stress --results-dir perf-results --label h3-macos
```

Notes:

- Prefer `sample` for quick local captures.
- Prefer `xctrace` / Instruments Time Profiler for deeper captures you plan to
  inspect in Instruments.
- Use longer workloads than `smoke` when you need the profiler to capture enough
  activity to be useful.

## Recommended matrix

Run the same workload families for both protocols:

- connection-heavy
- stream/request-heavy
- payload-heavy
- long-lived soak

Use the benchmark presets as starting points:

- `smoke`: wiring and artifact sanity checks
- `balanced`: default comparison lane
- `throughput`: larger payloads and more throughput pressure
- `stress`: repeated rounds and multi-client pressure

Evaluate both roles from the same artifacts:

- client
- server

Linux matrix:

- host `fast`
- host `portable`
- Docker ordinary container
- Docker `seccomp=unconfined`
- optional Docker `privileged`

macOS matrix:

- `fast`
- `portable`

On macOS, both public modes still use `kqueue`. Treat those as topology-policy
comparisons, not as backend comparisons.

## Comparison workflow

1. Run the same profile and payload shape for QUIC and H3.
2. Persist every run with `--results-dir` and a clear `--label`.
3. Use `npm run perf:analyze` to group artifacts by:
   - protocol
   - role
   - harness kind
   - sink kind
   - transport kind
   - OS
   - target (`host` vs `docker`)
   - Docker policy
   - selected runtime mode
   - driver/backend
   - topology policy
4. Compare only like-for-like workloads:
   - same profile
   - same connection count
   - same streams/requests per connection
   - same payload size
   - same target class
5. Inspect profiler artifacts only after the JSON summaries show a real delta.

When reading server-side benchmark summaries, treat `activeSessions > 0` as a
signal that the benchmark ended while some server-side closes were still
draining. In those cases, prefer `sessionCount`, `sessionsClosed`, and
`activeSessions` over assuming the close count has fully settled.

## Current bottleneck owners

The current `quic-bottlenecks` artifact set produced a stable first-pass owner
for each meaningful delta:

- `setup-cost / fallback overhead`: `H3 ordinary auto fallback` versus
  `H3 ordinary portable` in ordinary Docker. Both land on `portable/poll`, but
  auto fallback doubles client setup work (`driverSetupAttemptsTotal=40` vs
  `20`) and `strace -fc` shows more failed `io_uring_setup` probes (`34` vs
  `13`) without any steady-state backlog signal.
- `client topology cost`: Linux H3 portable client lanes remain the main
  ownership-model outlier because they stay `dedicated-per-session`. The saved
  groups show `driverSetupAttempts=20-40`, `h3ClientDedicatedWorkerSpawns=20`,
  and much wider throughput and p95 spread than shared-per-port fast lanes.
- `raw-QUIC correctness bug`: the pre-fix `QUIC unconfined server portable,
  client fast` lane fell to `950/1000` streams with `50` timeouts,
  `rawQuicClientReapsWithKnownStreams=1`, and second-round `p95=91.91ms`. After
  the timer-deadline refresh fix, the same lane returns `1000/1000`, `0`
  errors, `rawQuicClientReapsWithKnownStreams=0`, and `p95=15.53-18.63ms`.
- `higher-layer H3 overhead`: the aligned H3 mixed lanes stay slower than
  like-for-like QUIC controls even when backend counters stay clean.
  `H3 server portable, client fast` reaches about `129.8 Mbps` versus QUIC's
  `158.7 Mbps` in the same mixed shape, and `H3 server fast, client portable`
  reaches about `146.3 Mbps` versus QUIC's `167.6 Mbps`. That points at H3
  request/session/header work above raw transport, not at `io_uring` queue
  pressure.
- `server topology cost`: no separate server-topology owner emerged in this
  campaign. QUIC and H3 servers already share the
  `one-worker-per-bound-port` shape, and the `QUIC server fast` control lane
  stayed stable enough to act as the transport baseline.
- `macOS note`: both public modes still land on `kqueue`, and the quick
  `sample` plus follow-up `xctrace` passes did not show a sustained `kqueue`
  backlog hotspot. Treat the remaining macOS H3 delta as the same
  higher-layer-H3 bucket unless a longer trace shows otherwise.

## Owner-specific recipes

Use these focused runs when one bottleneck class regresses:

`client topology cost` is analyzer-driven rather than tied to one profiler
capture. Re-run `npm run perf:analyze -- --results-dir perf-results/quic-bottlenecks`
and compare the Linux H3 client groups with `topology=dedicated-per-session`
against the ones with `topology=shared-per-port`.

```bash
# setup-cost / fallback overhead
npm run perf:linux:h3 -- --docker --docker-lane "ordinary auto fallback" --strace --perf-stat --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label h3-ordinary-auto-fallback
npm run perf:linux:h3 -- --docker --docker-lane "ordinary portable" --strace --perf-stat --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label h3-ordinary-portable

# raw-QUIC correctness regression check
npm run bench:quic:docker -- --lane "unconfined server portable, client fast" --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label quic-client-fast-regression

# higher-layer H3 overhead versus QUIC transport control
npm run perf:linux:h3 -- --docker --docker-lane "unconfined server portable, client fast" --perf-record --perf-stat --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label h3-client-fast
npm run perf:linux:h3 -- --docker --docker-lane "unconfined server fast, client portable" --perf-record --perf-stat --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label h3-server-fast
npm run perf:linux:quic -- --docker --docker-lane "unconfined server fast, client portable" --perf-record --perf-stat --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label quic-server-fast

# macOS same-backend check
npm run perf:macos:h3 -- --sample --xctrace --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label h3-macos
npm run perf:macos:quic -- --sample --profile balanced --rounds 2 --results-dir perf-results/quic-bottlenecks --label quic-macos
```

## Backend-specific counters

Generic reactor counters remain useful for setup counts, worker spawns, shared
worker reuse, and session lifecycle.

Use backend-specific counters when generic totals stop explaining the result:

- `ioUringRxInFlightHighWatermark`
- `ioUringTxInFlightHighWatermark`
- `ioUringPendingTxHighWatermark`
- `ioUringRetryableSendCompletions`
- `kqueueUnsentHighWatermark`
- `kqueueWouldBlockSends`
- `kqueueWriteWakeups`

Interpretation:

- Rising `ioUringPendingTxHighWatermark` or `ioUringRetryableSendCompletions`
  means the fast path is spending time retrying or queueing sends even though it
  is still on `io_uring`.
- Rising `ioUringSubmitCalls` with low `ioUringSubmittedSqesTotal` per call means
  the ring is not batching effectively.
- Rising `ioUringCompletionBatchHighWatermark` without matching throughput gains
  means completion draining may be dominating wakeups.
- Rising `ioUringWakeCompletions` or `ioUringWakeWrites` with flat traffic volume
  suggests avoidable wake churn.
- Non-zero `ioUringSqFullEvents` means the SQ is saturating and batching or queue
  sizing should be reviewed before assuming protocol cost.
- Rising `kqueueUnsentHighWatermark` with high `kqueueWouldBlockSends` indicates
  the readiness path is building a backlog and retry loop pressure.
- `kqueueWriteWakeups` helps distinguish genuine write-side pressure from a run
  that is mostly receive/timer bound.

## Attribution harnesses

Use the three profiling harness families together when you need cost ownership:

- `quiche-direct`: protocol floor with minimal wrapper overhead
- `quic-loopback`: real Rust worker/reactor/transport path over loopback UDP
- `quic-mock`: Rust protocol path plus optional TSFN and JS listener work with
  no kernel UDP noise

Recommended mock sink / listener combinations:

- `counting`: Rust batching cost without TSFN or JS listener work
- `tsfn-noop-js`: TSFN and N-API dispatch cost with a minimal JS callback
- `tsfn-real-js`: TSFN plus realistic JS object/event handling cost

Interpretation rules:

- If `quiche-direct` and `quic-loopback` are close, `io_uring` is probably not
  the first owner.
- If `quic-loopback` is much slower than `quiche-direct`, optimize Rust
  reactor/driver batching and wake behavior.
- If `quic-mock` `counting` is fast but `tsfn-noop-js` is slow, optimize Rust
  batch delivery and TSFN pressure.
- If `tsfn-noop-js` is fine but `tsfn-real-js` is slow, optimize JS event shape,
  callback behavior, or listener allocation patterns.

## Baseline criteria before new gates

Do not promote new benchmark gates into CI until these conditions hold:

- At least 3 comparable samples exist for each group you want to gate.
- Runtime selection matches the expected mode/driver for that group.
- Client topology matches the intended ownership model:
  - shared-per-port for fast shared-client lanes
  - dedicated-per-session only where the compatibility policy still requires it
- Server benchmark summaries either settle to `activeSessions = 0` or clearly
  explain that some closes were still draining when the benchmark ended.
- Fast shared-client lanes keep driver setup counts effectively flat as session
  count grows.
- macOS comparisons are judged by topology and backlog behavior, not by trying
  to imitate `io_uring` internals.

Investigate before accepting a new baseline when any like-for-like group shows:

- throughput regression greater than about 10%
- p95 or p99 latency regression greater than about 15%
- CPU utilization increase greater than about 15%
- unexpected fallback or mixed runtime selections
- unexpected growth in `io_uring` retry/queue pressure counters
- unexpected growth in `kqueue` backlog / WouldBlock / write-wakeup counters

Keep the first phase manual and artifact-driven. Add automated gates only after
the artifact format and the observed variance are stable.
