# Runtime Modes

`@currentspace/http3` now exposes runtime selection as an explicit part of the
public API. The package keeps the Linux fast path, adds a portable Linux QUIC
path for ordinary containers, and never silently swaps QUIC/HTTP/3 out for a
different transport.

## Public API

These options are available anywhere the library creates QUIC state:

- `createSecureServer()`
- `serveFetch()`
- `connect()` / `connectAsync()`
- `createQuicServer()`
- `connectQuic()` / `connectQuicAsync()`

### Options

| Option | Type | Default | Meaning |
| --- | --- | --- | --- |
| `runtimeMode` | `'auto' \| 'fast' \| 'portable'` | `'auto'` | Requested runtime policy. |
| `fallbackPolicy` | `'error' \| 'warn-and-fallback'` | `'warn-and-fallback'` | Whether `auto` may fall back from `fast` to `portable`. |
| `onRuntimeEvent` | `(info) => void` | `none` | Callback invoked when runtime selection completes or falls back. |

### Returned metadata

Client sessions and server objects expose `runtimeInfo`:

```ts
type RuntimeInfo = {
  requestedMode: 'auto' | 'fast' | 'portable';
  fallbackPolicy: 'error' | 'warn-and-fallback';
  selectedMode: 'fast' | 'portable' | null;
  driver: 'io_uring' | 'poll' | 'kqueue' | null;
  fallbackOccurred: boolean;
  reasonCode: string | null;
  message?: string;
  errno?: number;
  syscall?: string;
  warningCode?: string;
  fastAttempt?: {
    code: string;
    message: string;
    driver?: 'io_uring' | 'poll' | 'kqueue';
    errno?: number;
    syscall?: string;
  } | null;
};
```

The same object is emitted on the `'runtime'` event and passed to
`onRuntimeEvent`.

## Mode semantics

### `runtimeMode: 'fast'`

- Uses the best-performance native driver for the platform.
- Linux: `io_uring`
- macOS: `kqueue`
- Never falls back automatically.
- If unavailable, startup/connect fails with `ERR_HTTP3_FAST_PATH_UNAVAILABLE`.

### `runtimeMode: 'portable'`

- Avoids Linux `io_uring` setup entirely.
- Linux: readiness-based `poll(2)` + `eventfd`
- macOS: still uses `kqueue`, but reports `selectedMode: 'portable'`
- Intended for ordinary Docker/Kubernetes containers and restricted hosts.

### `runtimeMode: 'auto'`

- Tries `fast` first.
- If the fast path is unavailable and `fallbackPolicy` is `warn-and-fallback`,
  retries with `portable`.
- Emits a structured `'runtime'` event, populates `runtimeInfo`, and emits a
  process warning with code `WARN_HTTP3_RUNTIME_FALLBACK`.
- If fallback is forbidden, fails with `ERR_HTTP3_FAST_PATH_UNAVAILABLE`.

## Topology and worker ownership

Runtime mode selects the backend/driver. It does not always imply a unique
worker/socket topology:

- Raw QUIC server: one worker per bound UDP port, many sessions multiplexed on it.
- H3 server: one worker per bound UDP port, many sessions multiplexed on it.
- Raw QUIC client fast mode: one shared worker and one local UDP port per bind family.
- H3 client fast mode: one shared worker and one local UDP port per bind family.
- macOS `kqueue`: the shared client ownership model is also used for portable mode,
  so macOS topology stays aligned even though both runtime modes use `kqueue`.
- Linux portable mode: still uses the compatibility driver and may use dedicated
  client workers when the topology policy requires it.

Observable surfaces:

- `session.runtimeInfo` / `server.runtimeInfo` tells you the selected mode and driver.
- `session._eventLoop.worker.localAddress()` remains the easiest way to confirm
  client-port reuse in focused tests.
- `npm run bench:quic` and `npm run bench:h3` now include internal reactor telemetry:
  driver setup attempts/successes, worker spawns, shared-worker reuse, session
  open/close counts, and TX buffer recycling.

## Environment matrix

| Environment | `fast` | `portable` | `auto` |
| --- | --- | --- | --- |
| macOS | `kqueue` | `kqueue` | selects `fast` |
| Native Linux with `io_uring` allowed | `io_uring` | `poll` | selects `fast` |
| Ordinary Docker/Kubernetes container on Linux | usually fails with `ERR_HTTP3_FAST_PATH_UNAVAILABLE` | works | falls back to `portable` if allowed |
| Docker/Kubernetes with `seccomp=unconfined` (or equivalent custom seccomp allowing `io_uring_*`) | works | works | selects `fast` |
| `privileged: true` container | works, but broader than necessary | works | selects `fast` |

## Capability and privilege matrix

The fast Linux path is gated by syscall policy first, not by broad capabilities.

| Setting | Fast path outcome | Notes |
| --- | --- | --- |
| Default Docker/Kubernetes seccomp profile | blocked | `io_uring_setup` commonly returns `EPERM` / `Operation not permitted`. |
| `cap_add` only | still blocked in tested Docker Desktop arm64 setups | Capabilities do not override seccomp-denied `io_uring_*` syscalls. |
| `security_opt: ['seccomp=unconfined']` | enabled | Narrowest tested Docker change that restored the fast path. |
| `privileged: true` | enabled | Broad workaround; not the preferred recommendation. |
| Host `io_uring` disabled by kernel policy/sysctl | unavailable | `fast` still fails precisely; use `portable`. |

In the tested Linux arm64 Docker Desktop matrix for 0.4.0:

- ordinary containers worked in `portable`
- ordinary containers failed precisely in `fast`
- `cap_add` alone did not restore `fast`
- `seccomp=unconfined` restored `fast` without `privileged: true`

## Performance trade-offs

| Choice | What you gain | What you give up |
| --- | --- | --- |
| `fast` | Lowest syscall overhead, highest throughput headroom on Linux | Requires host/container support for the fast driver |
| `portable` | Works in ordinary Linux containers and restricted environments | Higher per-I/O overhead than `io_uring` on Linux |
| `auto` | Best available runtime with explicit fallback visibility | You must handle warnings/runtime events if you care about placement |

The package keeps QUIC and HTTP/3 in all three modes. Runtime fallback never
silently downgrades to HTTP/2 or HTTP/1.

## Endpoint formats and hostname support

Client APIs accept:

- URL or authority strings such as `https://sfu:9080`, `sfu:9080`, or `127.0.0.1:4433`
- `{ host, port, servername? }`
- `{ address, port, servername }`

Hostname behavior:

- URL strings with hostnames and Docker service names are supported.
- Hostnames are resolved before the native client receives a socket address.
- When both A and AAAA results exist, the resolver currently prefers IPv4 first.
- `servername` controls TLS SNI independently from the transport address.
- Use `{ address, port, servername }` when DNS resolution is not desired or when
  the transport address and certificate identity differ.

## Structured errors and warnings

| Code | Meaning |
| --- | --- |
| `ERR_HTTP3_ENDPOINT_INVALID` | Endpoint string/object is malformed. |
| `ERR_HTTP3_ENDPOINT_RESOLUTION` | Hostname lookup failed. |
| `ERR_HTTP3_FAST_PATH_UNAVAILABLE` | Requested `fast` runtime cannot be used in the current environment. |
| `ERR_HTTP3_RUNTIME_UNSUPPORTED` | Runtime driver failed for an unsupported/environmental reason outside normal fast-path fallback. |
| `ERR_HTTP3_RUNTIME_FALLBACK` | Runtime fallback category for consumers that classify warnings/events. |
| `WARN_HTTP3_RUNTIME_FALLBACK` | Process warning emitted when `auto` falls back with `warn-and-fallback`. |

Fast-path errors preserve `driver`, `errno`, and `syscall` metadata when the
native layer can provide it.

## Examples

### Explicit portable mode

```ts
import { connectQuicAsync } from '@currentspace/http3';

const session = await connectQuicAsync('https://sfu:9080', {
  alpn: ['sfu-repl'],
  rejectUnauthorized: false,
  runtimeMode: 'portable',
  fallbackPolicy: 'error',
});

console.log(session.runtimeInfo);
```

### Explicit fast mode

```ts
import { serveFetch } from '@currentspace/http3/fetch';

const server = serveFetch({
  port: 443,
  host: '0.0.0.0',
  key,
  cert,
  fetch: () => new Response('ok'),
  runtimeMode: 'fast',
  fallbackPolicy: 'error',
});

console.log(server.runtimeInfo);
```

### Auto mode with observable fallback

```ts
import { connectAsync } from '@currentspace/http3';

const session = await connectAsync('https://sfu:9443', {
  rejectUnauthorized: false,
  runtimeMode: 'auto',
  fallbackPolicy: 'warn-and-fallback',
  onRuntimeEvent(info) {
    console.log('runtime selection', info);
  },
});

session.on('runtime', (info) => {
  console.log('runtime event', info);
});
```

## Docker validation

The repository includes a Linux runtime matrix that validates:

- portable mode in an ordinary container
- auto fallback visibility in an ordinary container
- precise fast-mode failure in an ordinary container
- `cap_add` without seccomp changes still failing
- fast mode succeeding with `seccomp=unconfined`

Run it locally with:

```bash
npm run test:docker:runtime
```

To include the optional privileged confirmation lane:

```bash
HTTP3_RUNTIME_TEST_PRIVILEGED=1 npm run test:docker:runtime
```

## Benchmark observability

Two host-side benchmark harnesses now expose runtime selection plus internal
reactor counters in both human and JSON output:

```bash
npm run bench:quic -- --profile smoke --json
npm run bench:h3 -- --profile smoke --json
```

Look for:

- `driverSetupAttemptsTotal` / `driverSetupSuccessTotal`
- protocol-specific shared-worker counters such as
  `rawQuicClientSharedWorkersCreated` or `h3ClientSharedWorkersCreated`
- `clientLocalPortReuseHits`
- protocol-specific session open/close counts
- `txBuffersRecycled`
