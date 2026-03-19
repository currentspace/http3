# @currentspace/http3

HTTP/3, HTTP/2, and raw QUIC server/client package for Node.js 24+, powered by Rust + quiche.

## Features

- HTTP/3 server and client over QUIC/UDP
- HTTP/2 fallback over TLS/TCP on the same listener
- Raw QUIC: bidirectional streams, datagrams, session resumption, custom ALPN
- Explicit runtime selection: `fast`, `portable`, or `auto`
- Platform-native I/O: kqueue (macOS), io_uring (Linux fast path), `poll` (Linux portable path)
- fetch/SSE/EventSource adapters
- Express compatibility via `@currentspace/http3/express`

## Install

```bash
npm install @currentspace/http3
```

Prebuilt native binaries are currently published for Linux x64/arm64 (glibc)
and macOS arm64. Other platforms may fall back to local native compilation; see
[`docs/SUPPORT_MATRIX.md`](./docs/SUPPORT_MATRIX.md).

## Quick server example

```ts
import { createSecureServer } from '@currentspace/http3';

const server = createSecureServer({
  key: process.env.TLS_KEY_PEM,
  cert: process.env.TLS_CERT_PEM,
}, (stream, headers) => {
  stream.respond({ ':status': '200', 'content-type': 'text/plain' });
  stream.end(`hello ${String(headers[':path'] ?? '/')}`);
});

server.listen(443, '0.0.0.0');
```

## Quick client example

```ts
import { connectAsync } from '@currentspace/http3';

const session = await connectAsync('example.com:443');
const stream = session.request({
  ':method': 'GET',
  ':path': '/',
  ':authority': 'example.com',
  ':scheme': 'https',
}, { endStream: true });
```

## Runtime modes

Every QUIC-capable API accepts:

- `runtimeMode: 'auto' | 'fast' | 'portable'`
- `fallbackPolicy: 'error' | 'warn-and-fallback'`
- `onRuntimeEvent(info)`

Returned client sessions and server objects expose `runtimeInfo`, and `auto`
fallback also emits a process warning with code `WARN_HTTP3_RUNTIME_FALLBACK`.

```ts
import { connectQuicAsync } from '@currentspace/http3';

const session = await connectQuicAsync('https://sfu:9080', {
  alpn: ['sfu-repl'],
  rejectUnauthorized: false,
  runtimeMode: 'auto',
  fallbackPolicy: 'warn-and-fallback',
});

console.log(session.runtimeInfo);
```

See [`docs/RUNTIME_MODES.md`](./docs/RUNTIME_MODES.md) for the deployment matrix,
capability requirements, Docker guidance, topology policy, and the raw endpoint
contract.

Client topology is now explicit in the implementation:

- raw QUIC fast clients share one worker and one local UDP port per bind family
- H3 fast clients share one worker and one local UDP port per bind family
- macOS portable mode keeps the same shared client-worker ownership model on
  top of `kqueue`
- QUIC and H3 servers remain one-worker-per-port architectures

Use the built-in benchmarks to inspect both runtime selection and internal
reactor counters:

```bash
npm run bench:quic -- --profile smoke
npm run bench:h3 -- --profile smoke
```

## New In 0.6.0

- Raw QUIC clients can now use mTLS through the stable public `cert` and `key` options on `connectQuic()` and `connectQuicAsync()`.
- Raw QUIC servers now support explicit client certificate policy with `clientAuth`, defaulting to `require` when a verification `ca` is configured.
- Raw QUIC server sessions now expose the verified peer certificate so applications can inspect or pin exact client certificates with Node's `X509Certificate` API.

See [`CHANGELOG.md`](./CHANGELOG.md) for the 0.6.0 release notes and
[`docs/RELEASE_EVIDENCE.md`](./docs/RELEASE_EVIDENCE.md) for the supporting
audit ledger and caveats behind this release.

## Quick QUIC server

```ts
import { createQuicServer } from '@currentspace/http3';

const server = createQuicServer({
  key: process.env.TLS_KEY_PEM,
  cert: process.env.TLS_CERT_PEM,
});

server.on('session', (session) => {
  session.on('stream', (stream) => {
    stream.pipe(stream); // echo
  });
});

await server.listen(4433, '0.0.0.0');
```

## Quick QUIC client

```ts
import { connectQuicAsync } from '@currentspace/http3';

const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
});

const stream = session.openStream();
stream.end(Buffer.from('hello QUIC'));

const chunks: Buffer[] = [];
stream.on('data', (c) => chunks.push(c));
stream.on('end', () => console.log(Buffer.concat(chunks).toString()));
```

## Compatibility surfaces

- `@currentspace/http3` - canonical API.
- `@currentspace/http3/parity` - http2-style aliases for migrations.
- `@currentspace/http3/h3` - HTTP/3-specific extension namespace.

## Examples

- [Raw API example](./examples/raw-api/README.md)
- [Express adapter example](./examples/express-adapter/README.md)
- [Hono `serveFetch()` example](./examples/hono/README.md)

## Start Here

- [Quickstart](./docs/QUICKSTART.md)
- [Runtime modes and deployment matrix](./docs/RUNTIME_MODES.md)
- [Support matrix](./docs/SUPPORT_MATRIX.md)
- [Configuration options reference](./docs/CONFIGURATION_OPTIONS.md)
- [Error handling guide](./docs/ERROR_HANDLING.md)
- [Changelog](./CHANGELOG.md)

## Deployment and Operations

- [QUIC guide](./docs/QUIC_GUIDE.md)
- [Production docs index](./docs/README.md)
- [HTTP/2 parity matrix](./docs/HTTP2_PARITY_MATRIX.md)
- [ECS/Fargate deployment](./docs/ECS_FARGATE_DEPLOYMENT.md)
- [AWS NLB QUIC passthrough](./docs/AWS_NLB_QUIC_PASSTHROUGH.md)
- [Session ticket keys across instances](./docs/SESSION_TICKET_KEYS.md)

## Contributors and Maintainers

- [Test strategy](./docs/TEST_STRATEGY.md)
- [Contributing guide](./CONTRIBUTING.md)
- [Release evidence ledger](./docs/RELEASE_EVIDENCE.md)
- [Release runbook](./docs/RELEASE_RUNBOOK.md)

