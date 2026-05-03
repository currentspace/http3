# Node HTTP/3 and QUIC Implementation Direction

**Date:** 2026-05-02
**Scope:** Top-down implementation direction for making this a stronger Node.js
HTTP/3 and raw QUIC library. This is performance and correctness focused; CI
plumbing is out of scope.

## North Star

This package should have two clean public layers:

1. **HTTP/3 layer:** an idiomatic Node API that feels like `node:http2` where
   the protocols overlap: secure server, client session, request stream,
   response stream, headers, trailers, GOAWAY, ping, session metrics, stream
   backpressure, and AbortSignal behavior.
2. **Raw QUIC layer:** a lower-level transport API that feels closer to
   `node:net` plus QUIC concepts: bidirectional streams, datagrams, session
   tickets, ALPN, connection close, stream reset, per-path/transport metrics,
   and optional protocol features.

The native layer should hide quiche and platform-driver quirks behind a small
set of invariants:

- write completion means local/native admission, not peer ACK;
- Node backpressure is bounded by `writableHighWaterMark` plus a measured
  native queue budget;
- quiche flow-control remainders retain owned native buffers instead of
  flattening;
- unsafe code is contained in narrow modules with testable invariants;
- kqueue, poll, and io_uring expose the same semantic behavior, even if their
  batching mechanisms differ.

## Current Strengths

- The public API already uses the right Node primitives: `Duplex` streams,
  EventEmitter sessions, `createSecureServer`, `connect`, `connectAsync`,
  `createQuicServer`, and `connectQuic`.
- Stream write callbacks now apply a local Node-style admission window using
  the stream `writableHighWaterMark`, which matches Node stream semantics much
  better than unbounded synchronous callback completion.
- Outbound body and datagram payloads now land in `ChunkPoolIngress` at the
  N-API boundary and flow into quiche through `ArcBuf`.
- H3 and raw QUIC pending writes share a state machine and keep `ArcBuf`
  remainders instead of copying tails back into fresh vectors.
- Empty FIN is now explicit: H3 and raw QUIC use quiche's borrowed empty-slice
  APIs for FIN-only sends, not zero-copy buffer leases.
- Unsafe code is now mostly isolated behind `InitializedPacketBuf`,
  `ExternalVecLease`, and `ProvidedBufferId`.
- The transport drivers have usable telemetry for SQ pressure, pending writes,
  command backlog, and outbound copy-boundary behavior.

## Highest-Impact Improvements

### 1. Unify the JS stream implementation

`QuicStream`, `ServerHttp3Stream`, and `ClientHttp3Stream` repeat the same core
logic: `_write`, `_final`, read backpressure, native drain callbacks, blocked
state, stream close, timeout handling, and native-write-window release.

Create one internal stream core, for example `NativeBidirectionalStream`, with a
small protocol adapter:

- `send(data, fin) -> admittedBytes`
- `close(code)`
- `streamId`
- optional `respond`, `trailers`, and request/response state hooks for H3

This would reduce the chance that a backpressure or FIN fix lands in one stream
class but not the other two. It would also make `_writev`, timeout, destroy,
and drain semantics easier to prove once.

### 2. Replace sentinel write results with a structured admission model

The JS side currently treats `0` as blocked and nonzero as progress, with
FIN-only writes represented as one byte of local pressure. That is acceptable
at the public Writable boundary, but internally it should become structured:

```ts
interface WriteAdmission {
  acceptedBytes: number;
  finAccepted: boolean;
  locallyQueuedBytes: number;
  blocked: boolean;
}
```

The public stream can still count FIN as one byte of local pressure for
`Writable` behavior, but the event-loop and worker code should stop inferring
FIN correctness from byte counts. Native already has typed outcomes; carrying
that shape up to the private JS event-loop boundary would remove a class of
empty-FIN and partial-FIN bugs.

### 3. Make write admission one coherent contract

The right contract is:

- callbacks fire when bytes are under native ownership or locally queued under
  a bounded native budget;
- callbacks do not wait for peer ACK;
- `write()` returns `false` once JS plus native local backlog reaches the stream
  high-water mark;
- native drain fires when the local backlog drops below that mark or quiche
  reports stream capacity for the pending low watermark.

The current implementation is close, but the command-channel path still has
more than one notion of "accepted". The next step is to centralize admission in
one native writer object per stream, with explicit counters:

- JS-admitted bytes;
- bytes copied or leased into native memory;
- worker command bytes not yet processed;
- bytes retained in `PendingWrite`;
- FIN pending/accepted state.

Keep the default idiomatic: Node's binary stream high-water mark is the default
window. Allow overrides per stream/session, but do not invent an ACK-based
window.

### 4. Add `_writev` and buffer-view normalization

Node Writable implementations can provide `_writev` for batched writes. This
library does not yet have it, so a burst of small writes crosses N-API once per
chunk.

Add a shared `_writev` path in the unified stream core:

- for small aggregate batches, copy once into a pooled `Chunk`;
- for larger batches, preserve chunk boundaries in a native `ChunkChain` or
  equivalent queue and submit progressively to quiche;
- avoid `Buffer.concat()` in JS for final body materialization;
- normalize `Buffer | Uint8Array | DataView` without an extra JS copy before the
  unavoidable N-API native-owned copy.

This is the most pragmatic next copy reduction. It avoids pinning V8 memory
across worker threads while removing avoidable JS-side materialization and
N-API call overhead.

### 5. Keep native-owned copies until a safer zero-copy API exists

Directly leasing arbitrary JS `Buffer` memory across worker-thread and quiche
lifetimes is not the right default. It is hard to make safe, hard to explain to
users, and hard to test under GC/finalizer timing.

A safer future zero-copy story is to expose native-owned writable buffers to
JS, then require users to send those buffers back without copying:

```ts
const lease = session.allocSendBuffer(64 * 1024);
const view = lease.buffer;
// user fills view
stream.writeLease(lease, length);
```

That API would make ownership explicit: the buffer starts native-owned, JS gets
a temporary view, and the lease is consumed by the stream write. Until profiles
show the pooled copy is the bottleneck, `ChunkPoolIngress` is the right default.

### 6. Reduce H3 overhead above raw QUIC

Existing perf notes point at H3 request/session/header work as the remaining
overhead over raw QUIC. The next optimizations should be measurement-driven:

- reuse native header vectors and reduce per-header allocation;
- validate and normalize pseudo-headers once at the API edge;
- add a fast path for common response shapes (`:status`, content type, body);
- keep `respondWithBody()` as the common single-call optimization, but make it
  part of the same stream-state machine rather than a separate behavioral path;
- add header-heavy and trailers-heavy benchmarks before changing QPACK knobs.

### 7. Improve H3 protocol completeness and state modeling

The public API should make protocol states explicit:

- interim 1xx responses;
- trailers before/after body completion;
- request body half-close;
- response body half-close;
- GOAWAY drain and request rejection;
- stream reset versus graceful FIN;
- 0-RTT accepted/rejected status and replay-safe request gating.

Represent these as per-stream/session state machines with focused tests. This
is more valuable than scattering protocol checks across event handlers.

### 8. Use quiche scheduling signals more directly

quiche exposes send capacity, writability low-watermarks, pacing release time,
and send quantum. The transport and pending-write loops should use those as the
source of scheduling truth:

- call `stream_writable(stream_id, len)` with the next pending chunk size or the
  configured low watermark before relying on a future writable event;
- use `writable()` or `stream_writable_next()` consistently, not mixed on the
  same connection;
- size UDP/GSO batches around `send_quantum()`;
- respect pacing release time where the platform driver can delay sends
  cheaply;
- keep FIN-only sends on borrowed empty-slice APIs.

This should improve fairness under many streams and avoid overfilling one
stream while another has priority.

### 9. Fix H3 client topology cost

The perf runbook identifies Linux H3 client dedicated-per-session topology as a
remaining owner. The server already has a one-worker-per-bound-port shape; the
client side should converge on a shared reactor where practical:

- key by runtime driver, local bind address, and socket strategy;
- multiplex sessions through one client worker/reactor;
- keep per-session close and metrics isolated;
- preserve AbortSignal and startup error behavior;
- prove no cross-session event delivery leak with binding-isolation tests.

This should reduce setup cost and improve high-session-count throughput without
changing user API.

### 10. Expand raw QUIC as a real transport API

Raw QUIC should not just be "HTTP/3 without headers". Useful additions:

- unidirectional stream support;
- explicit stream direction/type in events;
- stream priority APIs;
- per-path events and migration state;
- datagram send queue pressure and drop reason visibility;
- connection close frame details;
- reset/stop-sending helpers with typed error codes;
- session ticket and 0-RTT status events matching the H3 layer.

These features should remain lower-level than H3 and should not bleed quiche
types into the public API.

### 11. Keep unsafe code auditable

The right long-term policy is:

- all ordinary modules use `#![deny(unsafe_code)]`;
- `unsafe_boundary.rs` is the only normal unsafe island for N-API ownership;
- io_uring-specific unsafe code is isolated in narrow helper modules for CQE,
  SQE, provided-buffer, and sockaddr handling;
- every unsafe wrapper has unit tests plus Miri/loom/fuzz coverage where that
  tool can prove the invariant;
- unsafe blocks carry a short invariant comment, not broad narrative.

The next useful hardening step is to split the remaining io_uring unsafe
operations into wrappers similar to `ProvidedBufferId`, then test those wrappers
directly rather than only testing full event loops.

### 12. Treat observability as API, not debug output

This library should expose stable diagnostics for serious users:

- qlog paths and lifecycle;
- Node `diagnostics_channel` events for session/stream state transitions;
- `perf_hooks`-style timing entries for handshake, request, stream finish, and
  reset;
- transport counters already used by benchmarks;
- per-stream write backlog and native queue pressure snapshots.

The goal is that a user can explain a slow or stuck request without attaching a
debugger or enabling ad hoc logs.

## What Not To Do

- Do not make write callbacks wait for far-end ACKs. That is not Node stream
  backpressure and would couple app throughput to network RTT incorrectly.
- Do not pin arbitrary JS `Buffer` memory across worker/quiche lifetimes as the
  default outbound path.
- Do not expose quiche error enums directly as the public API. Map them into
  stable Node-style error codes and structured properties.
- Do not optimize QPACK/header allocation before header-heavy benchmarks show
  it is hot.
- Do not let platform driver behavior change JS-visible stream semantics.

## Suggested Next Commits

1. Extract a shared JS native-stream writer/core used by H3 client, H3 server,
   and raw QUIC streams.
2. Replace private `streamSend(): number | boolean` with structured admission
   results at the event-loop boundary.
3. Add `_writev` to the shared stream core and a native batched write command
   that copies small chunks once into `ChunkPool`.
4. Add `Uint8Array`/`DataView` buffer-view normalization for stream and datagram
   sends to avoid pre-N-API `Buffer.from()` copies.
5. Refactor H3 response/request state into explicit state machines and port the
   existing edge-case tests onto those states.
6. Build the shared H3 client reactor topology and compare it against the
   dedicated-per-session baseline with existing perf harnesses.
7. Split io_uring unsafe helpers around CQE/SQE/provided-buffer/sockaddr
   invariants and add direct Miri/loom/fuzz tests for each wrapper.

## References

- Node stream backpressure and `highWaterMark` semantics:
  https://nodejs.org/api/stream.html
- Node HTTP/2 API shape and flow-control settings:
  https://nodejs.org/api/http2.html
- quiche 0.28 `stream_send`, empty FIN, stream capacity, writability, pacing,
  and send quantum:
  https://docs.rs/quiche/latest/quiche/struct.Connection.html
- quiche 0.28 HTTP/3 `send_body`, `send_body_zc`, stream blocked retry, and
  priority update APIs:
  https://docs.rs/quiche/latest/quiche/h3/struct.Connection.html
