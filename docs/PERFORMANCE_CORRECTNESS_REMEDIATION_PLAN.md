# Performance and Correctness Remediation Plan

**Date:** 2026-05-01
**Scope:** Runtime performance, protocol correctness, unsafe-code containment, and API semantics for H3 and raw QUIC. CI-only work is intentionally out of scope.

## Completed in this pass

1. **Outbound payload copies reduced.**
   N-API stream payload copies now land in `ChunkPoolIngress`, and H3/raw QUIC pending-write queues retain `ArcBuf` chunks instead of flattening remainders into new `Vec<u8>` values.

2. **H3 empty-FIN handling made explicit.**
   H3 and raw QUIC send paths now return typed outcomes with `fin_accepted` and optional remainders instead of inferring correctness from `written == 0`.

3. **Unsafe boundaries narrowed.**
   `InitializedPacketBuf`, `ExternalVecLease`, and `ProvidedBufferId` now hold the core invariants for initialized buffers, N-API external-buffer ownership, and io_uring provided-buffer IDs.

4. **Wrapper-level tests added.**
   Unit, loom, fuzz, and focused Miri coverage exercise the unsafe wrappers directly. Broader Miri remains blocked locally by unsupported macOS syscalls in dependencies such as `ring`/`loom`.

5. **Local write backpressure added.**
   Stream write callbacks now honor a Node-style local native admission window based on `writableHighWaterMark`. This does not wait for peer ACKs; it prevents fast producers from draining unlimited data through N-API in one event-loop turn.

6. **Write-pressure telemetry added.**
   Reactor telemetry now reports bytes admitted from JS, bytes queued in native worker commands, and bytes retained in per-stream pending-write queues, including high-water marks.

7. **io_uring retry/drain correctness hardened.**
   Send-bundle CQE draining now preserves interleaved RX/send/waker completions, SQ-full retry paths no longer panic on non-GSO sends or drop remaining GSO fallback batches, send-bundle admission no longer truncates oversized datagrams, and waker read rearm state/errors are explicit.

8. **poll GSO EMSGSIZE fallback fixed.**
   GSO `sendmmsg` failures with `EMSGSIZE` now split batches back into individual datagrams for retry instead of being classified as permanent drops.

9. **kqueue send errors classified.**
   kqueue sends now retry only `WouldBlock`; permanent send failures are logged and dropped instead of flowing through the same branch as successful sends.

10. **Raw QUIC client stream admission moved into native.**
    `QuicClientSession.openStream()` now asks the native worker to reserve the next client-initiated bidirectional stream before constructing a JS `QuicStream`. The worker uses quiche's empty non-FIN stream creation path, so peer stream limits fail synchronously at `openStream()` instead of after JS has minted an unusable stream ID.

11. **Connect aborts now cover the handshake window.**
    HTTP/3 and raw QUIC client connect paths keep an AbortSignal listener alive until `ready()` settles. Aborting during worker startup or QUIC handshake rejects `ready()` with `AbortError` and closes any native event loop already created.

12. **HTTP/3 ping callbacks are ACK-driven.**
    H3 client/server `ping(cb)` now queues the callback until native emits a ping-ack event after quiche observes ACK progress for packets sent after the ping request. The return value remains the last RTT snapshot for compatibility.

13. **GOAWAY and close payloads are structured.**
    H3 GOAWAY events now carry `{ lastStreamId }`, H3/raw QUIC close events carry `{ errorCode, reason }` from quiche `peer_error()`/`local_error()`, close emission is idempotent, and H3 clients reject new requests after GOAWAY with `ERR_HTTP3_GOAWAY`.

14. **Headers and trailers preserve duplicate fields.**
    H3 request, response, and trailer conversion now flattens outgoing arrays into repeated native header fields and folds inbound duplicates back into arrays. `set-cookie` and other repeated fields no longer collapse to the first value at the core API boundary.

15. **Request-before-ready behavior is explicit.**
    HTTP/3 clients intentionally require `ready()`/`'connect'` before normal requests. The documented exception is opt-in 0-RTT with safe-method guards. Regression coverage now asserts the pre-ready failure is `ERR_HTTP3_INVALID_STATE` rather than an accidental low-level crash.

16. **Unsafe policy and pending-write state tests tightened.**
    Safe Rust modules now carry `deny(unsafe_code)` fences, `ExternalVecLease` has direct tests for generic failure, `napi_no_external_buffers_allowed`, and finalizer recycle ownership, and H3/raw QUIC share one pending-write queue state machine covered by unit tests plus a fuzz target.

17. **Native write-admission measurement completed.**
    Matching H3 and raw QUIC write-pressure runs on macOS/kqueue, each with 10 connections, 8 in-flight streams per connection, 64 KiB payloads, and a 3 second steady-state window, showed zero final native command backlog, zero per-stream pending-write backlog, and a 64 KiB native command-queue high-water mark. That matches the default binary stream high-water mark, so no second native admission window is justified by this profile.

18. **DATAGRAM payload copies reduced.**
    H3 and raw QUIC DATAGRAM sends now enter native through `ChunkPoolIngress`, worker commands carry pooled `Chunk` values, and quiche is called through `dgram_send_buf()` with `ArcBuf` ownership. DATAGRAM receive paths use `dgram_recv_buf()` and hand the owned buffer to JS event delivery when it is uniquely owned, avoiding the extra receive-queue copy in the common case.

19. **N-API outbound copy-boundary telemetry added.**
    Runtime telemetry now records `outboundIngressBufferReuses`, `outboundIngressBufferAllocations`, and `outboundIngressCopiedBytes` for the JS `Buffer` to Rust `ChunkPoolIngress` boundary. This makes the external-buffer pinning decision measurable without adding cross-thread JS buffer lifetime risk first.

20. **Outbound ingress pool classes now cover normal Node write sizes.**
    `ChunkPool` now retains size classes through 64 KiB, matching the default binary stream `writableHighWaterMark`. Large bins use a lower retention cap than 1-4 KiB bins, so 8-64 KiB writes can be reused without turning every session into a large-buffer cache.

21. **64 KiB ingress-pool validation completed.**
    Short macOS/kqueue H3 and raw QUIC steady-state runs with 5 connections, 2 in-flight streams per connection, and 64 KiB payloads completed with zero errors. H3 recorded ingress reuse/allocation of client `215/15` and server `225/5`; raw QUIC recorded client `110/10` and server `116/4`, confirming the new top size class is used on the benchmark path.

22. **FIN-only writes skip outbound ingress-pool checkout.**
    Empty stream-final writes now use a shared JS empty buffer and a zero-capacity, unpooled native `Chunk::empty()`, so sending FIN without payload no longer checks out a 1 KiB ingress-pool buffer just to copy zero bytes. H3 and raw QUIC route empty FIN through quiche's borrowed `&[]` send APIs instead of the zero-copy body path because there is no payload buffer lifetime to preserve.

## P0: Finish write backpressure semantics

**Problem:** The new JS window restores Node stream pressure at the public API boundary, but native command channels still report "queued" rather than "accepted by worker/quiche".

**Plan:**
1. [done] Add native telemetry for bytes admitted from JS, bytes sitting in worker command queues, and bytes sitting in per-stream pending writes.
2. [done] Add small-window H3 and raw QUIC e2e tests that assert:
   - `write()` returns `false` under pressure.
   - `'drain'` fires only after local/native backlog drops below the stream window.
   - peer ACK is not required for write callbacks.
3. [done by measurement] If telemetry shows worker command queues can still grow faster than the worker drains them, add a bounded native admission budget with low/high water marks and a drain event on local-budget release.
   - macOS/kqueue write-pressure measurement did not show unbounded worker-command growth. H3 and raw QUIC both peaked at `outboundCommandQueuedBytesHighWatermark = 65,536` with `outboundPendingWriteBytesHighWatermark = 0`, so the existing Node-style `writableHighWaterMark` admission window is the active backpressure boundary for this path.
   - Keep the telemetry as the regression guard. Reopen this item only if Linux/io_uring or a larger targeted write-pressure profile shows command-queue growth materially above the configured stream window.
4. [done] Keep the default window idiomatic: use the stream's `writableHighWaterMark` (`64 KiB` for binary Node streams by default) unless callers configure a stream-specific value.

## P1: Transport-driver correctness

1. **[done] io_uring completion draining.**
   Ensure send-bundle CQ draining never consumes unrelated RX/send/waker completions. Add a fault-injected test that interleaves bundle CQEs with normal CQEs.

2. **[done] io_uring SQ-full fallback.**
   Guard the single-packet `chunks(0)` panic path and test forced SQ push failure.

3. **[done] io_uring TX bundle sizing.**
   Reject or fall back when a datagram exceeds `TX_BUF_ENTRY_SIZE` instead of truncating.

4. **[done] io_uring waker rearm.**
   Track waker armed/unarmed state and surface rearm failures instead of silently losing command wakeups.

5. **[done] poll `EMSGSIZE` GSO fallback.**
   Make the comment true: split and requeue oversized GSO batches, with a regression test.

6. **[done] kqueue permanent send errors.**
   Stop treating non-`WouldBlock` send failures as successful completion. Classify and surface permanent failures consistently with poll/io_uring.

## P2: Public API correctness and Node parity

1. **[done] Raw QUIC stream admission.**
   Move `openStream()` admission into native or define an explicit pending/ready/error state so stream limits are enforced before JS mints IDs.

2. **[done] AbortSignal through handshake.**
   Current abort handling covers endpoint resolution. Wire it through worker creation and QUIC handshake, reject `ready()` with `AbortError`, and close native resources.

3. **[done for H3] PING parity.**
   Implement ACK-driven callback semantics for `ping()` instead of returning only a cached RTT snapshot.

4. **[done] GOAWAY and close payloads.**
   Define event args and idempotency: error code, last stream ID, reason, and deterministic post-GOAWAY request rejection.

5. **[done] Headers/trailers preservation.**
   Preserve duplicate headers such as `set-cookie`, stop collapsing outgoing arrays to the first value, and make trailer delivery deterministic.

6. **[done] Request-before-ready behavior.**
   Either queue requests like `node:http2` or document and test the intentional divergence.

## P3: Unsafe-code hardening

1. [done] Add a crate policy that keeps `unsafe_code = "warn"` globally but uses `deny(unsafe_code)` for modules that should never contain unsafe blocks.
2. [done] Expand `ExternalVecLease` tests with success-finalizer and `napi_no_external_buffers_allowed` fallback coverage.
3. [done for wrappers] Add Miri coverage for `InitializedPacketBuf` and `ProvidedBufferId` on a nightly toolchain where Miri is available. Broader dependency-heavy Miri remains blocked locally by platform syscall support.
4. [done] Add a pending-write state-machine fuzz target covering append, partial accept, `Done`, close, and FIN ordering.

## P4: Performance work only after measurement

1. **Outbound external-buffer pinning.**
   Do not pin JS `Buffer` memory across worker threads unless allocation profiles show the current pooled copy remains material.
   - The required copy into Rust-owned memory is now measured through outbound ingress buffer reuse/allocation/copy counters in every H3/raw QUIC benchmark summary.

2. **[done] DATAGRAM zero-copy.**
   Use quiche `dgram_send_buf` / `dgram_recv_buf` with the existing `ChunkPool`/`ArcBuf` ownership model now that stream-body paths are stable.

3. **Header/QPACK allocation reduction.**
   Only optimize header conversion after header-heavy benchmarks prove it is hot.
