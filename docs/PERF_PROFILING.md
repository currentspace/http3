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

The analyzer consumes the persisted `benchmark-summary` and
`docker-benchmark-matrix` artifacts:

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
- Rising `kqueueUnsentHighWatermark` with high `kqueueWouldBlockSends` indicates
  the readiness path is building a backlog and retry loop pressure.
- `kqueueWriteWakeups` helps distinguish genuine write-side pressure from a run
  that is mostly receive/timer bound.

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
