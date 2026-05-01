# Plan: @currentspace/http3 hardening sprint

**Branch base:** `main` (audit rollup `fix/audit-rollup` already merged 36 findings as of 2026-05-01)
**Codebase:** `@currentspace/http3`, ~21k LoC Rust + ~7.5k LoC TS, quiche 0.28, napi-rs 3, Node ≥ 24
**Repo conventions:**
- pnpm only — never npm/yarn
- Never prefix env vars on commands; use `.cargo/config.toml`
- Tests use `runtimeMode: 'portable'` to avoid io_uring ring exhaustion
- Always rebuild after Rust: `npx napi build --platform --release`
- Verification gate: `pnpm verify` (scripts/verify.sh) — must pass clean for every commit
- Commit message style: `<type>(<scope>): <subject>` (see recent commits)
- Never revert/checkout/reset to older code — fix forward only

**Per-task discipline:**
- Each task = one focused commit
- Every commit gates on: `pnpm run lint`, `pnpm run typecheck`, `cargo clippy --lib`, `cargo clippy --tests --features bench-internals`, plus the focused tests called out in the task
- Mark task done only when all observables are green and success criteria met

---

## Phase 1 — This sprint (high ROI, low risk)

### P1-A · Wire `send_body_owned` into the H3 worker hot path

**Why:** `send_body` (`src/connection.rs:324`) calls `ArcBuf::from_vec(data.to_vec())` at line 334 — a redundant copy. `send_body_owned` (`src/connection.rs:362`) takes `Vec<u8>` directly. Five worker callsites still take the slow path.

**Files:**
- `src/connection.rs:320-397` (already has `send_body_owned`; no change needed)
- `src/worker.rs:2026, 2066, 2764, 3157, 3186` (the 5 callsites)
- `src/worker.rs` PendingWrite struct (currently `{ chunk: Chunk, fin: bool }`) — needs to carry `Option<Vec<u8>>` remainder

**Steps:**
1. Read all 5 callsites and the PendingWrite struct definition.
2. Decide whether to: (a) change PendingWrite to hold `Vec<u8>`, or (b) keep Chunk and convert at call boundary via `chunk.into_vec()`.
3. Migrate one callsite at a time; run `cargo test --lib --no-default-features` after each to catch regressions early.
4. Update the partial-write path: `send_body_owned` returns `(usize, Option<Vec<u8>>)` — stash the `Some(remainder)` as the new PendingWrite payload.
5. After all five sites migrated, deprecate `send_body` (mark `#[deprecated]`) but keep it for now — `quic_worker.rs` is a separate task.

**Observables:**
- `cargo bench --bench quic_mock_pair` before vs after (record numbers)
- `pnpm run test:rust:mock:extended` clean
- `pnpm test` clean
- Watch for new clippy lints on the migrated functions (especially `clippy::pedantic`)

**Success criteria:**
- All 5 worker.rs callsites use `send_body_owned`
- No regression in `pnpm test` (12s baseline) or `pnpm verify`
- Bench shows measurable throughput improvement on full-write hot path (target: ≥5% on h3 large-body benchmark) OR documented neutral if measurement noise dominates
- Partial-write tests in `test/core/stream-close-pending-write.test.ts` still pass

**Dependencies:** none. This is the foundation for P2-G (apply same pattern to `quic_worker.rs`).

**Risk:** medium-low. Partial-write retry semantics must remain identical. Roll back by reverting the commit.

---

### P1-B · Promote io_uring `buffer_data` SAFETY comment to a release-mode assert

**Why:** `src/transport/io_uring.rs:183-186` does `unsafe { from_raw_parts(self.buf_base.add(offset), RX_BUF_SIZE) }` where `offset = (bid as usize) * RX_BUF_SIZE`. The `bid` comes from kernel CQE flags. The SAFETY comment says "bid is within [0, RX_RING_SIZE)" but no code enforces it. If the kernel ever returns `bid >= RX_RING_SIZE` (kernel bug, race, hostile environment), this is immediate UB.

**Files:**
- `src/transport/io_uring.rs:183-186`

**Steps:**
1. Add `assert!(bid < RX_RING_SIZE as u16, "kernel returned out-of-range bid: {bid}");` as the first line of `buffer_data`.
2. Update the SAFETY comment to read: "Invariant enforced by the assert above."
3. Add a parallel guard at the two call sites (`io_uring.rs:743, 1520`): if the assert ever trips in production builds, log the offending bid and the CQE flags before crashing.

**Observables:**
- `cargo test --features driver-tracing -- iouring` clean
- Run the docker io_uring lane: `pnpm run docker:up && pnpm test` — assert never trips under normal load
- Check release build size: assert is one cmp + branch per packet, should be negligible

**Success criteria:**
- Release-mode assert in place
- All tests pass (assert never trips, which is the whole point)
- Code review confirms the assert is *release-mode* not `debug_assert!` — this is a memory-safety boundary

**Dependencies:** none.

**Risk:** very low. One-line change. If perf regression measurable (it shouldn't be — this is a pre-recv operation, not on the network hot path), narrow to `debug_assert!` and add a fuzz target instead.

---

### P1-C · Unit test for `RecyclableBuffer::to_napi_value` error path

**Why:** Audit #22 fixed leak-on-error in `src/h3_event.rs:99-167` for the case where `napi_create_external_buffer` returns non-`napi_ok`. There's no test for that branch — it could silently regress. The reclaim path uses `Box::from_raw(hint_ptr)` + `Vec::from_raw_parts(...)` and *must* run exactly once.

**Files:**
- `src/h3_event.rs:99-167` (no source change)
- `tests/recyclable_buffer_napi_error.rs` (new test file) OR a `#[cfg(test)] mod tests` block in `h3_event.rs`

**Steps:**
1. Introduce a `#[cfg(test)] static NAPI_FORCE_ERROR: AtomicBool` (or use `napi-derive` testing hooks if they exist).
2. Wrap the `napi_create_external_buffer` FFI call in a `#[cfg(test)]`-aware indirection that returns the forced error code when set.
3. Use a `Drop` counter on a wrapped `Vec<u8>` (or a custom hint type) to assert that the Vec is exactly-once-deallocated in both branches: success-with-NAPI-finalize and error-path reclaim.
4. Cover three cases: `napi_ok` (NAPI takes ownership), `napi_no_external_buffers_allowed` (fallback to Buffer copy + drop hint), other error (raw-parts reclaim).

**Observables:**
- `cargo test --lib --no-default-features` clean
- Run with `cargo +nightly miri test` if the unsafe path can be exercised under miri (NAPI FFI may not be miri-compatible — document if so)
- Watch valgrind output if available: zero leaks across all three branches

**Success criteria:**
- Three test cases pass; each asserts exactly-once deallocation via Drop counter
- No memory leak on any branch
- Test is deterministic (no flakiness)

**Dependencies:** none.

**Risk:** low. The test scaffolding is the bulk of the work; the assertions are simple.

---

### P1-D · `session.ping(cb?)` overload for `node:http2` ergonomics

**Why:** `lib/session.ts:152` exposes `ping(): number` returning a synchronous RTT snapshot. The H2 adapter (`lib/session.ts:279-281`) wraps `node:http2`'s callback-based ping and *discards the duration argument*. A developer migrating from http2 with `session.ping((err, dur) => ...)` will type-error.

**Files:**
- `lib/session.ts:152` (Http3ServerSession)
- `lib/session.ts:279-281` (Http2ServerSessionAdapter)
- `lib/session.ts:433` (Http3ClientSession)
- `lib/client.ts` if there's a client-side ping
- `index.d.ts` (regenerated by napi build, but check the TS type)

**Steps:**
1. Add an overload: `ping(cb?: (err: Error | null, duration: number) => void): number`.
2. Synchronous behavior: still returns the cached RTT number.
3. If `cb` is provided, invoke it with `(null, snapshot)` on `process.nextTick` to match http2's async semantics.
4. Update the H2 adapter to forward `cb` faithfully to `_h2Session.ping(cb)` (preserving the duration argument that's currently discarded at line 281).
5. Add a test in `test/core/` for both forms.
6. Update README / docs section "Migrating from node:http2" to mention this matches.

**Observables:**
- `pnpm run typecheck` clean
- `pnpm run lint` clean
- `pnpm test` clean — new test passes for both call shapes

**Success criteria:**
- Both `session.ping()` and `session.ping(cb)` work and match http2 muscle memory
- H2 adapter no longer discards the http2 callback's duration

**Dependencies:** none.

**Risk:** very low. Backward-compatible API addition.

---

### P1-E · Remove PendingWrite on StreamClose dispatch

**Why:** `worker.rs:2094-2101` dispatches `StreamClose` and calls `conn.stream_close()` but doesn't remove the corresponding entry from `pending_writes`. The stale entry lingers until the next `flush_pending_writes()` iteration tries to send and fails. Memory waste + misleading metric. Confirmed by the worker.rs deep audit.

**Files:**
- `src/worker.rs:2094-2101` (StreamClose handler)
- `src/quic_worker.rs:2027-2029` (parallel — verify it has the same gap)

**Steps:**
1. After `conn.stream_close(stream_id)` succeeds, remove the matching `(conn_handle, stream_id)` key from `self.pending_writes`. If a remainder was present, emit `EVENT_RESET` for the abandoned write (matching the cleanup_closed pattern at worker.rs:2579).
2. Mirror in quic_worker.rs.
3. Add a regression test: open stream, write a large body, immediately call `stream.destroy()`, verify pending_writes count drops to zero on next event loop iteration AND the JS-side write callback fires with an abort error.

**Observables:**
- `pnpm run test:rust:mock:extended` clean
- New regression test passes
- `getMetrics()` `pendingWrites` count drops correctly

**Success criteria:**
- Both H3 and QUIC paths handle StreamClose by purging pending writes
- New regression test passes
- No `let _ = ...` regression — the new code path explicitly handles the Result

**Dependencies:** none.

**Risk:** low.

---

### P1-F · RX_PAUSE self-healing watchdog

**Why:** `src/event_loop.rs:557-582` skips RX when the outstanding-events gauge exceeds `RX_PAUSE_HIGH_WATER`. The gauge increments on every TSFN flush, decrements on `ackEventBatch()`. **If JS dispatch throws an uncaught exception before acking, the gauge stays high and RX pauses forever** — the connection silently stops receiving. Confirmed by the worker.rs deep audit (BUG #2).

**Files:**
- `src/event_loop.rs:540-590` (RX pause logic)
- `src/reactor_metrics.rs` (gauge implementation)
- `lib/event-loop.ts` (TSFN dispatcher — check current try/catch behavior)

**Steps:**
1. Inspect `lib/event-loop.ts` TSFN dispatcher — verify whether it wraps the JS callback in try/catch and acks even on throw. If yes, this bug is closed; if no, fix the JS side first.
2. On the Rust side, add a watchdog: track `last_ack_timestamp`. If `outstanding > RX_PAUSE_HIGH_WATER` AND `now - last_ack_timestamp > 5s`, log a warning AND auto-decrement the gauge by the stuck batch count (logged separately as `eventBatchSelfHealedTotal`).
3. Add a test: deliberately throw from the TSFN dispatcher; verify RX resumes within 5–6 seconds.

**Observables:**
- New telemetry counter `eventBatchSelfHealedTotal` exposed via `getMetrics()`
- Watchdog test passes
- Normal-operation tests show `eventBatchSelfHealedTotal == 0`

**Success criteria:**
- A throwing JS dispatcher does not permanently wedge the connection
- Watchdog only activates when truly stuck (no false positives at high load)
- Telemetry surfaces the event for ops visibility

**Dependencies:** none. Optionally informed by P2-D batch-ordering work.

**Risk:** medium. The watchdog timeout (5s) is a tuning choice; too aggressive and it masks real backpressure, too loose and it doesn't help. Document the rationale.

---

### P1-G · Apply RX_PAUSE check to shared QUIC client reactor

**Why:** `quic_worker.rs:1541-1914` (`run_shared_quic_client_event_loop`) is a custom event loop separate from `event_loop::run_event_loop`. It does **not** check `RX_PAUSE_HIGH_WATER` — line 1753-1756 processes all RX with no backpressure. JS-side dispatch can fall behind unboundedly. Surfaced by the quic_worker.rs deep audit as DRIFT #2.

**Files:**
- `src/quic_worker.rs:1541-1914` (shared client reactor loop)
- `src/event_loop.rs:540-590` (canonical RX-pause logic to mirror)

**Steps:**
1. Read both loops side-by-side. Identify the exact lines in the shared QUIC client reactor where RX is consumed.
2. Mirror the gauge check + skip pattern from `event_loop.rs`. Add the same `eventBatchRxPausesTotal` increment.
3. Audit `worker.rs` for an analogous shared H3 client reactor — if one exists, fix that too.
4. Add a load test: shared client + slow JS dispatcher → verify RX pauses correctly and resumes.

**Observables:**
- `eventBatchRxPausesTotal` increments under simulated slow-JS load
- Throughput test confirms no buffer overflow at the kernel level
- All existing shared-client tests still pass

**Success criteria:**
- Shared QUIC client reactor honors `RX_PAUSE_HIGH_WATER`
- Same fix applied to shared H3 client reactor if it exists with the same gap
- Load test demonstrates pause/resume cycle

**Dependencies:** none.

**Risk:** medium. The shared client reactor is structurally different from the main event loop; the pause logic must be inserted at the correct point in the iteration to avoid stalling timer processing.

---

### P1-H · Add error logging to silent command failures

**Why:** `worker.rs:2100` (`StreamClose`) and `worker.rs:2113` (`SendTrailers`) use `let _ = conn.stream_close(...)` / `let _ = conn.send_trailers(...)`. Both swallow errors silently. That's intentional fire-and-forget for the JS API contract, but operators have zero diagnostic signal when something fails. Surfaced by the worker.rs deep audit (GAP #1).

**Files:**
- `src/worker.rs:2100, 2113`
- `src/quic_worker.rs` parallel sites (check stream_close + close calls)

**Steps:**
1. Replace `let _ = expr` with `if let Err(e) = expr { log::debug!("..."); }`.
2. Use `log::debug!` not `warn!` — these failures are mostly benign races (peer reset first, connection already closing).
3. Mirror in quic_worker.rs.

**Observables:**
- `RUST_LOG=debug pnpm test` shows the new log lines under simulated peer-reset races
- `pnpm verify` clean

**Success criteria:**
- Every silenced error has a `log::debug!` with context (handle, stream_id, error)
- No new log spam at default INFO level

**Dependencies:** none.

**Risk:** very low.

---

## Phase 2 — Next sprint

### P2-A · io_uring CI lane (default-on, not gated)

**Why:** Production users on Linux ≥ 6.0 hit io_uring. Current CI runs poll only; io_uring lives behind `HTTP3_RUNTIME_TEST_PRIVILEGED=1` Docker. Per-PR signal is missing.

**Files:**
- `.github/workflows/ci.yml`
- `Dockerfile.runtime-test`, `docker-compose.runtime-tests.yml`

**Steps:**
1. Add a CI job `runtime-io-uring` that runs in privileged Docker on every PR.
2. Run `pnpm run docker:up` + `pnpm test` + a minimal io_uring-specific suite.
3. Ensure failure of this job blocks PR merge.

**Observables:** every PR shows green/red on the new job; mean run time < 5 min.
**Success criteria:** io_uring lane runs by default; flaky-test rate < 1%.

---

### P2-B · Stream-reset (`RESET_STREAM` / `STOP_SENDING`) tests for H3 and QUIC

**Why:** No explicit tests for per-stream cancellation. Core QUIC mechanic missing coverage.

**Files:**
- `test/interop/stream-reset.test.ts` (new)
- Possibly extend `tests/quic_shutdown.rs`

**Steps:**
1. Test client → reset → server observes EVENT_RESET with code; server's pending writes purge.
2. Test server → reset → client observes; in-flight Duplex writes error appropriately.
3. Cover both H3 and raw QUIC paths.

**Observables / Success:** new tests pass; they fail clean if `stream_shutdown` plumbing breaks.

---

### P2-C · `cargo-fuzz` target for `parse_recv_cmsgs`

**Why:** `src/transport/socket.rs:332-418` walks kernel cmsg buffers with `read_unaligned`. Audit #20 already caught a Darwin-vs-Linux alignment bug here. A fuzz target is the right safety net.

**Files:**
- `fuzz/fuzz_targets/parse_recv_cmsgs.rs` (new)
- `fuzz/Cargo.toml` (new)

**Steps:**
1. `cargo install cargo-fuzz`; init `fuzz/`.
2. Target accepts arbitrary `&[u8]` as the cmsg control buffer; calls `parse_recv_cmsgs`; asserts no panic.
3. Run for at least 1 hour; corpus checked in.
4. Optional: add `proptest` variant for fast in-CI smoke (10s).

**Observables / Success:** fuzz runs ≥ 1 hour with zero crashes; proptest variant runs in CI.

---

### P2-D · Per-connection event batching to preserve HEADERS→DATA→TRAILERS ordering

**Why:** Worker audit BUG #3: at high event rates (>2048 events/iteration), a single connection's events can split across batch flushes, breaking the JS-side stream parser's HEADERS→DATA→TRAILERS expectation.

**Files:**
- `src/event_loop.rs` (batcher)
- `src/h3_event.rs` (event types)

**Steps:**
1. Reproduce the issue with a stress test (10k concurrent streams, single connection).
2. Decide approach: (a) buffer per-connection events and flush as atomic group, or (b) tag events with sequence numbers and have JS reorder.
3. Implement (a) — it's simpler.

**Observables / Success:** stress test confirms no out-of-order delivery; throughput regression < 5%.

**Risk:** medium. Per-connection buffering changes batch semantics; may surface latent ordering bugs in JS.

---

### P2-E · Heap-drift assertions in longhaul tests

**Why:** Longhaul tests print RSS but don't assert. Memory leaks ship.

**Files:**
- `test/longhaul/h3-mix.test.ts`, `test/longhaul/h3-sustained.test.ts`, `test/longhaul/quic-mix.test.ts`, `test/longhaul/quic-sustained.test.ts`

**Steps:**
1. Capture baseline RSS at 30s (after warmup). At end of soak, assert `final_RSS < baseline + 50 MB`.
2. Same for `process.memoryUsage().heapUsed`.
3. Document the threshold rationale in a comment.

**Observables / Success:** assertions trigger on real leaks; never trigger on healthy runs.

---

### P2-F · Verify `getRemoteSettings` semantics on raw QUIC

**Why:** Quic worker audit flagged it as a "missing command". Not yet confirmed as a bug — raw QUIC has no SETTINGS frames; the JS API for `QuicClientSession` may not expose `getRemoteSettings()` at all. Worth a 30-min investigation.

**Files:**
- `lib/quic-client.ts`, `lib/quic-server.ts`
- `src/quic_worker.rs` (commands)

**Steps:**
1. Grep JS-side for any `getRemoteSettings` call on a raw QUIC session.
2. If unreachable: add a comment in quic_worker.rs explaining why the variant is intentionally absent. Close.
3. If reachable: either remove the JS API or add a no-op variant returning `null`.

**Observables / Success:** clear documented decision in code; no JS API can reach an unimplemented native command.

---

### P2-G · Apply `send_body_owned` migration to `quic_worker.rs`

**Why:** Same redundant-copy issue as P1-A, applied to raw QUIC. Defer until P1-A lands and proves out the pattern.

**Files:**
- `src/quic_connection.rs` (raw QUIC stream send) — verify analogous owned variant exists or add one
- `src/quic_worker.rs` callsites

**Steps:** mirror P1-A. raw QUIC has no h3 framer, so the owned variant is even simpler — no `BufFactory` plumbing.

**Observables / Success:** same as P1-A, applied to raw QUIC paths.

---

### P2-H · Datagram outbound queue (RFC 9221)

**Why:** `quic_worker.rs:2055-2064` (`SendDatagram`): if quiche refuses (flow control / congestion), the datagram is silently dropped. The JS API returns `false`, but apps may expect at-most-once-with-retry semantics. Surfaced by quic_worker.rs audit.

**Files:**
- `src/quic_connection.rs` (add bounded outbound dgram queue per connection)
- `src/quic_worker.rs:2055-2064` (push on queue; drain on each send opportunity)
- `lib/quic-client.ts`, `lib/quic-server.ts` (document new behavior; expose queue depth metric)

**Steps:**
1. Decide queue policy: bounded (e.g., 64 datagrams), drop-oldest or drop-newest. Document.
2. On each send opportunity, attempt to drain queued datagrams before quiche sends regular streams.
3. Surface queue depth in `getMetrics()`.

**Observables / Success:** under flow-control pressure, datagrams are buffered and delivered in order; queue overflow returns `false` to JS as today.

**Risk:** medium. Changes API semantics from "fire-and-forget bool" to "queued bool". Document carefully.

---

## Phase 3 — Backlog (do once justified by data)

### P3-A · NAPI external-buffer pinning for outbound (eliminate `Buffer::to_vec()` at `server.rs:209`)

Holds a `Reference<Buffer>` from NAPI to pin JS-held memory across the worker thread send. Complex synchronization. **Benchmark first** — only justified if outbound throughput becomes a measured bottleneck above what P1-A delivers.

### P3-B · QPACK pre-encode on JS side

Skip per-header `String` allocations on the worker thread. Only worth it for header-heavy workloads (gRPC-style RPCs).

### P3-C · macOS GitHub Actions runner

Currently kqueue tests are local-only. macOS-hosted runners exist; add a CI job that runs the full suite on macOS-13 and macOS-14. Cost: minutes per PR.

### P3-D · `loom` test for `BufferRecycler`

Permutation test for the V8-GC-thread → recycler → worker-thread interaction. Mostly defensive — current correctness is sound, but `loom` would prove it.

### P3-E · Document untested protocol features

NAT rebinding / connection migration / version negotiation / 0-RTT data transfer (vs. resumption) — explicitly call out in `docs/` why each is untested or deferred. Prevents the "do we support X?" question loop.

---

## Verification cadence (per task)

Every commit must pass:
```
pnpm run lint
pnpm run typecheck
cargo clippy --lib --no-default-features -- -D warnings
cargo clippy --tests --features bench-internals --no-default-features -- -D warnings
cargo test --lib --no-default-features
pnpm run test:rust:mock:extended
npx napi build --platform --release
pnpm run build:test && pnpm run build:dist
pnpm test
```
The canonical wrapper is `pnpm verify`. Use it.

For tasks that touch transport drivers (P1-B, P2-A, P2-C):
```
pnpm run docker:up && pnpm run test:rust:full
```

For tasks that touch event loop or backpressure (P1-F, P1-G, P2-D):
```
HTTP3_LONGHAUL=1 pnpm run test:longhaul
```

---

## Summary table of deliverables

| ID | Effort | Risk | Impact | Blocks |
|---|---|---|---|---|
| P1-A | 0.5 d | M-L | High (perf) | P2-G |
| P1-B | 1 hr | VL | Critical (UB) | — |
| P1-C | 1 d | L | Medium (regression) | — |
| P1-D | 0.5 d | VL | Medium (ergonomics) | — |
| P1-E | 0.5 d | L | Medium (correctness) | — |
| P1-F | 1 d | M | High (correctness) | — |
| P1-G | 1 d | M | High (correctness) | — |
| P1-H | 1 hr | VL | Low (observability) | — |
| P2-A | 1 d | L | High (CI signal) | — |
| P2-B | 1 d | L | Medium (coverage) | — |
| P2-C | 0.5 d | L | Medium (safety) | — |
| P2-D | 2 d | M | Medium (correctness) | — |
| P2-E | 0.5 d | VL | Medium (regression) | — |
| P2-F | 0.25 d | VL | Low (clarity) | — |
| P2-G | 0.5 d | M-L | Medium (perf) | P1-A |
| P2-H | 2 d | M | Medium (semantics) | — |

**Phase 1 total: ~5.5 days. Phase 2 total: ~7.5 days.** Single engineer can complete Phase 1 in a sprint, Phase 2 in a follow-up sprint.

---

This plan is self-contained; an agent can execute it task by task without further context. Each task is independently committable, testable, and rollback-safe.
