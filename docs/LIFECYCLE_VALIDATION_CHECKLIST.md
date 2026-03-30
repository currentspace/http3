# Lifecycle Validation Checklist

This checklist turns the current lifecycle debugging work into one explicit
maintainer plan. The goal is to reduce combinatoric debugging, keep durable
tracing in the codebase, and validate the runtime through representative
integration and load tests instead of one-off local repros.

Baseline as of 2026-03-22 after a fresh `npm run build && npm run build:test`
and sequential per-file runs:

- `44` built test files executed sequentially
- `41` passed
- `3` deterministic failures remain:
  - `dist-test/test/core/event-loop.test.js`
  - `dist-test/test/core/protocol-correctness.test.js`
  - `dist-test/test/interop/quic-protocol.test.js`

## Success criteria

- [ ] The three deterministic red lanes pass `10/10` sequential reruns.
- [ ] Integration and load suites complete with `0` hangs and `0` cancelled tests.
- [ ] Worker, session, stream, blocked-stream, and pending-write counts converge to zero after teardown.
- [ ] Each failure emits useful artifacts automatically.
- [ ] Each driver and bridge representative lane records the requested mode and the selected driver.
- [ ] No unexpected runtime fallback goes unnoticed during integration or load runs.
- [ ] Throughput and p95/p99 latency remain within agreed driver-specific budgets.

## Current deterministic red lanes

- [ ] `dist-test/test/core/event-loop.test.js`
  - Current symptom: all four tests cancel with `Promise resolution is still pending but the event loop has already resolved`.
  - Primary concern: worker bridge shutdown ordering and unresolved JS/native lifecycle promises.
- [ ] `dist-test/test/core/protocol-correctness.test.js`
  - Current symptom: single GOAWAY-before-close regression test times out at `120000ms`.
  - Primary concern: H3 GOAWAY and session close ordering.
- [ ] `dist-test/test/interop/quic-protocol.test.js`
  - Current symptom: isolated failure is the final `server graceful close with active streams` case.
  - Primary concern: raw QUIC graceful close with active streams, blocked writes, or unfinished bridge teardown.

## Lifecycle invariants to make explicit

- [ ] Worker shutdown completes only after the last queued event batch is observable from JS.
- [ ] H3 GOAWAY is emitted before H3 session close.
- [ ] Raw QUIC graceful close resolves or errors every active stream and never leaves a hanging stream.
- [ ] Data and FIN survive flow-control stalls and drain cycles.
- [ ] Active stream count, blocked stream count, and pending write count converge to zero after session close.
- [ ] Driver selection and runtime fallback are recorded for every integration and load run.
- [ ] TSFN/event-batch drops are counted and surfaced in failure artifacts.
- [ ] Session close reason, close path, and shutdown completion are visible in trace snapshots.

## Durable instrumentation and tracing to keep in-tree

### Native counters and snapshots

- [ ] Extend `src/reactor_metrics.rs` with lifecycle counters:
  - `goaway_sent_total`
  - `goaway_received_total`
  - `session_close_requested_total`
  - `session_close_completed_total`
  - `shutdown_complete_emitted_total`
  - `tsfn_batch_drop_total`
  - `streams_reset_total`
  - `fin_events_total`
  - `drain_events_total`
  - `workers_started_total`
  - `workers_stopped_total`
- [ ] Add lifecycle high-watermark counters:
  - `active_streams_on_close_high_watermark`
  - `blocked_streams_on_close_high_watermark`
  - `pending_writes_on_close_high_watermark`
- [ ] Add an opt-in bounded lifecycle trace buffer per worker or session with:
  - timestamp
  - driver
  - requested runtime mode
  - selected driver
  - conn handle
  - stream id
  - event kind
  - active stream count
  - blocked stream count
  - pending write count
  - close reason
  - shutdown state
- [ ] Expose lifecycle snapshot APIs without turning on noisy hot-path logging by default.

### Existing tracing to preserve and standardize

- [ ] Keep qlog support as the primary deep protocol trace.
- [ ] Keep keylog support for TLS/Wireshark debugging.
- [ ] Keep `runtimeTelemetry()` and session metrics as the always-available low-noise layer.
- [ ] Keep focused `RUST_LOG` support for scoped incident debugging.
- [ ] Keep failure-only debug repro scripts under `test/debug/`.

### Trace hygiene rules

- [ ] Do not keep ad hoc `debug!` spam in hot paths.
- [ ] Prefer counters and bounded lifecycle snapshots over unbounded logs.
- [ ] Emit full trace artifacts only on failure in integration and load tests.
- [ ] Make failure artifacts deterministic and discoverable from the test harness.

## File-by-file implementation plan

| Area | Files | Planned work |
| --- | --- | --- |
| Runtime counters | `src/reactor_metrics.rs`, `src/lib.rs` | Add lifecycle counters, high-watermark tracking, and snapshot export plumbing. |
| Unified event loop | `src/event_loop.rs` | Record worker start/stop, batch delivery, TSFN drop counts, shutdown request/completion, and selected driver metadata. |
| H3 session lifecycle | `src/worker.rs`, `src/connection.rs` | Record GOAWAY send/receive, session close ordering, active stream counts, drain state, and final shutdown path. |
| Raw QUIC lifecycle | `src/quic_worker.rs`, `src/quic_connection.rs` | Record active-stream close paths, FIN/drain transitions, blocked writes, resets, and shutdown completion. |
| JS worker/session bridge | `lib/event-loop.ts`, `lib/session.ts`, `lib/server.ts`, `lib/client.ts` | Expose lifecycle snapshots to tests, capture runtime metadata, and attach failure artifact collection. |
| Raw QUIC JS bridge | `lib/quic-stream.ts`, `lib/quic-client.ts`, `lib/quic-server.ts` | Expose raw QUIC lifecycle debug hooks and ensure stream close/error artifacts can be captured on failure. |
| Shared test harness | `test/support/lifecycle-debug.ts`, `test/support/load-harness.ts`, `test/support/driver-matrix.ts`, `test/support/failure-artifacts.ts` | Add common setup/teardown, telemetry snapshotting, qlog capture, leak assertions, and failure artifact dumps. |
| Integration coverage | `test/runtime/worker-lifecycle.test.ts`, `test/interop/h3-session-lifecycle.test.ts`, `test/interop/quic-session-lifecycle.test.ts`, `test/interop/driver-bridge-parity.test.ts` | Add deterministic lifecycle coverage with shared tracing harnesses. |
| Load and stability coverage | `test/perf-gates/session-churn-lifecycle.test.ts`, `test/perf-gates/goaway-stability.test.ts`, `test/perf-gates/raw-quic-active-close.test.ts`, `test/perf-gates/fin-drain-stability.test.ts`, `test/perf-gates/datagram-mixed-load.test.ts`, `test/perf-gates/driver-throughput-parity.test.ts` | Add throughput and stability coverage around the same lifecycle invariants. |
| Native-only transport validation | `tests/transport_quiche_pair.rs` | Add native-only close ordering and flow-control cases that are hard to isolate from JS. |
| Scripts and docs | `scripts/run-lifecycle-matrix.mjs`, `scripts/collect-lifecycle-artifacts.mjs`, `docs/TEST_STRATEGY.md`, `docs/README.md` | Add repeatable matrix execution and document the lifecycle hardening plan. |

## Integration test checklist

### `test/runtime/worker-lifecycle.test.ts`

- [ ] Bind, connect, request, response, and shutdown in the worker bridge.
- [ ] Assert `SHUTDOWN_COMPLETE` is emitted after all observable event batches.
- [ ] Cover `reusePort` worker teardown.
- [ ] Assert no unresolved promises remain after close.
- [ ] Capture selected driver, runtime telemetry, and lifecycle snapshots on failure.

### `test/interop/h3-session-lifecycle.test.ts`

- [ ] GOAWAY arrives before H3 session close.
- [ ] Graceful session close succeeds with active requests.
- [ ] Close ordering is stable with in-flight response bodies.
- [ ] Repeated graceful-close cycles do not leak sessions, workers, or timers.
- [ ] qlog and lifecycle artifacts are captured automatically on failure.

### `test/interop/quic-session-lifecycle.test.ts`

- [ ] Graceful raw QUIC close with active streams does not hang.
- [ ] Stream reset ordering is deterministic across server and client.
- [ ] Data and FIN survive drain cycles and flow-control stalls.
- [ ] Datagram traffic coexists with streams during close and teardown.
- [ ] `maxConnections`, idle timeout, and explicit close all clean up without leaks.

### `test/interop/driver-bridge-parity.test.ts`

- [ ] Run the same lifecycle assertions through the worker bridge, H3 public API, and raw QUIC API.
- [ ] Assert actual selected driver, not just requested runtime mode.
- [ ] Use one shared artifact format across all bridges.

### Native-only transport additions

- [ ] Add close ordering and flow-control renewal cases in `tests/transport_quiche_pair.rs`.
- [ ] Keep Rust-only additions focused on cases that JS integration tests cannot observe precisely.

## Load and stability test checklist

### `test/perf-gates/session-churn-lifecycle.test.ts`

- [ ] Repeated connect, request, close cycles under moderate concurrency.
- [ ] Assert no leaked sessions, workers, blocked streams, or pending writes.
- [ ] Record throughput and cleanup latency.

### `test/perf-gates/goaway-stability.test.ts`

- [ ] Repeated H3 graceful close under concurrency.
- [ ] Assert GOAWAY-before-close for every client.
- [ ] Record failed closes, late closes, and close latency.

### `test/perf-gates/raw-quic-active-close.test.ts`

- [ ] Graceful raw QUIC close while many streams are active.
- [ ] Assert every stream resolves or errors cleanly.
- [ ] Record active-stream and blocked-stream high watermarks.

### `test/perf-gates/fin-drain-stability.test.ts`

- [ ] Large payloads with intentionally small flow-control windows.
- [ ] Assert zero lost FINs and zero hanging streams.
- [ ] Record drain counts, blocked-stream peaks, and end-to-end latency.

### `test/perf-gates/datagram-mixed-load.test.ts`

- [ ] Mixed datagram and stream traffic under session churn.
- [ ] Assert stable close behavior, acceptable datagram echo success, and no teardown leaks.
- [ ] Record throughput, packet rates, and close latency.

### `test/perf-gates/driver-throughput-parity.test.ts`

- [ ] Representative throughput test for each supported driver and bridge.
- [ ] Compare throughput and p95/p99 latency with driver-specific baselines.
- [ ] Record selected driver and fallback events as part of the perf artifacts.

## Driver and bridge matrix

The goal is representative coverage, not a full cartesian explosion.

| Bridge | Linux portable (`poll`) | Linux fast (`io_uring`) | macOS (`kqueue`) | Notes |
| --- | --- | --- | --- | --- |
| Worker bridge | Integration + load | Integration + load | Smoke + integration | Prioritize shutdown ordering and leak-free teardown. |
| H3 public API | Integration + load | Integration + load | Integration + load | Prioritize GOAWAY-before-close and graceful close under load. |
| Raw QUIC API | Integration + load | Integration + load | Integration + load | Prioritize active-stream close, FIN/drain, and datagrams. |
| Browser HTTPS entrypoint | Smoke | Smoke | Smoke | Keep correctness smoke in browser tests; run throughput via Node harnesses. |
| Docker runtime matrix | Portable + auto | Fast when available | N/A | Use as environment and runtime selection validation. |

## Failure artifact checklist

Every integration and load failure should capture:

- [ ] selected driver and requested runtime mode
- [ ] runtime telemetry snapshot before and after the test
- [ ] lifecycle snapshot for the affected worker or session
- [ ] qlog path when qlog is enabled
- [ ] keylog path when keylog is enabled
- [ ] active stream count at close
- [ ] blocked stream count at close
- [ ] pending write count at close
- [ ] close reason and shutdown state
- [ ] concise repro configuration including concurrency, payload size, and timeout values

## Rollout order

- [ ] Phase 1: land lifecycle counters, lifecycle snapshot APIs, and failure artifact helpers.
- [ ] Phase 2: make the three deterministic red lanes green with the new tracing layer in place.
- [ ] Phase 3: add the new integration lifecycle suites.
- [ ] Phase 4: add the new load and stability suites.
- [ ] Phase 5: run the reduced driver and bridge matrix.
- [ ] Phase 6: re-run browser, Docker runtime, Rust interop, and release lanes.
- [ ] Phase 7: promote the new lifecycle matrix into normal contributor and CI workflows.

## Promotion rules

- [ ] Do not promote a new lifecycle or perf gate to CI until it is stable locally and in the relevant platform lane.
- [ ] Do not merge a tracing change that only helps one ad hoc repro but does not fit the durable artifact model above.
- [ ] Do not add more one-off shutdown fixes without also adding a lifecycle invariant, a trace signal, and an integration or load test that proves the fix.
