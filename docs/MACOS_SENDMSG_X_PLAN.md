# macOS sendmsg_x Plan

Status: plan

The macOS fast path currently uses kqueue plus `UdpSocket::send_to` for each
outbound datagram. Darwin exposes a private `sendmsg_x` syscall for batched
datagram sends. The local Xcode SDK exposes `SYS_sendmsg_x`, and Apple XNU
headers describe `sendmsg_x(int s, const struct msghdr_x *msgp, u_int cnt,
int flags)`. The API is private and documented as subject to change, so support
must be optional, runtime-probed, and invisible to the Node API.

## Goals

- Reduce kqueue TX syscall count on macOS by batching multiple QUIC datagrams.
- Preserve the existing kqueue degradation behavior: retry `WouldBlock`, queue
  unsent datagrams, and never turn overload into request timeouts.
- Keep the JS event loop out of the send path; selection and batching stay in
  the native worker thread.
- Keep zero-copy semantics as close as possible: kernel iovecs point at existing
  `TxDatagram` payload buffers, and buffers recycle only after the syscall
  returns.
- Make the optimization easy to disable so benchmarks can compare with and
  without it.

## Design

1. Add a narrow unsafe module, `src/transport/macos_sendmsg_x.rs`, compiled only
   on macOS.
2. Define a Rust representation of Darwin `msghdr_x` in that module only. Keep
   all raw pointers, `iovec` arrays, and syscall interaction private.
3. Public safe wrapper:
   `try_send_batch(fd: RawFd, packets: &mut [TxDatagram]) -> io::Result<usize>`.
   It returns the count of datagrams accepted by the kernel, matching
   `sendmmsg`-style partial-send behavior.
4. Runtime probe on kqueue driver startup:
   - Disabled by default if `HTTP3_MACOS_SENDMSG_X=0`.
   - Enabled by default only when a no-op/one-packet probe proves the syscall is
     present and works on this OS/socket combination.
   - If the syscall returns `ENOSYS`, `EINVAL`, `ENOTSUP`, or an unexpected
     structural error during probe, permanently fall back to `send_to`.
5. Add kqueue TX mode selection:
   - `send_to`: current behavior.
   - `sendmsg_x`: batch path for packets with no per-packet ancillary data.
6. Batch only packets that can legally use `sendmsg_x`:
   - No control message.
   - Destination address handling must be validated. Darwin's private header says
     address and ancillary fields are unsupported for `sendmsg_x`, so if the
     syscall only works for connected sockets, use it only for connected client
     sockets. Servers and multi-peer sockets must stay on `send_to` until proven
     safe.
7. Add telemetry:
   - `kqueueSendmsgXEnabled`
   - `kqueueSendmsgXProbeFailures`
   - `kqueueSendmsgXSubmitCalls`
   - `kqueueSendmsgXDatagramsSubmitted`
   - `kqueueSendmsgXPartialSends`
   - `kqueueSendmsgXFallbacks`
8. Preserve existing backpressure:
   - On partial success, recycle sent packet buffers and enqueue the remainder.
   - On `EWOULDBLOCK`/`EAGAIN`, enqueue the current packet and remainder.
   - On permanent per-call errors after probe, record fallback and retry the
     batch through `send_to` before dropping anything.

## Tests

1. Unit tests for the safe wrapper:
   - Correct `msghdr_x` construction for N payload buffers.
   - Partial-send accounting recycles only sent buffers.
   - `WouldBlock` maps to retryable behavior.
   - Permanent errors do not leak buffers.
2. Miri tests for pointer/lifetime wrappers:
   - The wrapper must not expose raw pointers.
   - Packet buffers must outlive the syscall frame.
3. macOS integration tests:
   - `HTTP3_MACOS_SENDMSG_X=0`: force current `send_to` path.
   - `HTTP3_MACOS_SENDMSG_X=1`: require probe success or skip with an explicit
     reason when the OS/socket combination does not support it.
   - `HTTP3_MACOS_SENDMSG_X=auto`: default runtime probe and fallback.
4. E2E correctness:
   - H3 loopback sustained echo with 0 errors in both modes.
   - QUIC loopback sustained echo with 0 errors in both modes.
   - Overpressure tests in both modes must cleanly backpressure or fail with
     valid protocol/session errors, never timeout.
5. Benchmarks:
   - macOS loopback H3/QUIC, sendmsg_x off vs auto/on.
   - macOS -> Linux and Linux -> macOS H3/QUIC, sendmsg_x off vs auto/on.
   - Compare against HTTPS/1.1, HTTPS/2, HTTPS/3 Node matrix and iperf3.

## Implementation Order

1. Add telemetry counters and config/env parsing, with no behavior change.
2. Add the unsafe `macos_sendmsg_x` wrapper plus unit/Miri tests.
3. Add kqueue driver probe and mode selection.
4. Wire `submit_sends()` and `drain_unsent()` to use batch send when enabled.
5. Add forced-off and forced-on macOS test lanes locally.
6. Run profiler/benchmark matrix with:
   - `HTTP3_MACOS_SENDMSG_X=0`
   - `HTTP3_MACOS_SENDMSG_X=auto`
   - `HTTP3_MACOS_SENDMSG_X=1`
7. Ship only if correctness is identical and the auto path never regresses when
   the syscall is unavailable.

## Open Risk

The private Darwin header says address and ancillary data are unsupported for
`sendmsg_x`. QUIC server sockets need per-packet destinations. If testing proves
`sendmsg_x` cannot send unconnected UDP datagrams to different peers, keep it as
a client-side or connected-socket optimization only. That still lets us measure
whether it helps loopback/cross-host client TX without risking server correctness.
