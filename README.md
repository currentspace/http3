# @currentspace/http3

HTTP/3, HTTP/2, and raw QUIC server/client package for Node.js 24+, powered by Rust + quiche.

## Features

- HTTP/3 server and client over QUIC/UDP
- HTTP/2 fallback over TLS/TCP on the same listener
- Raw QUIC: bidirectional streams, datagrams, session resumption, custom ALPN
- Runtime selection: `auto`, `fast`, `portable`, or `wasm`
- Platform-native I/O: kqueue (macOS), io_uring (Linux fast path), `poll` (Linux portable path)
- WASM runtime (`runtimeMode: 'wasm'`) for HTTP/3 and raw QUIC clients, plus a
  Node-only WASM server — no native `.node` addon required, and confirmed
  running inside real Cloudflare workerd
- fetch/SSE/EventSource adapters
- Express compatibility via `@currentspace/http3/express`

## Install

```bash
npm install @currentspace/http3
```

Version **1.0.0** adds automatic WASM fallback, bounded large writes, facade
runtime metadata, and native packages for Linux musl and Intel macOS. Earlier
0.9.9 installations require explicit WASM when a native addon is unavailable.

Keep optional dependencies enabled for native performance. If the native addon
cannot load, the default `auto` mode selects the bundled WASM runtime and emits
`WARN_HTTP3_RUNTIME_FALLBACK`. The root package contains JavaScript, declarations,
WASM, and the CLI; it does not bundle native binaries, docs, or examples.

| Platform | Native package target | When the addon cannot load |
| --- | --- | --- |
| macOS arm64 / x64 | `darwin-arm64` / `darwin-x64` | WASM |
| Linux glibc arm64 / x64 | `linux-arm64-gnu` / `linux-x64-gnu` | WASM |
| Linux musl arm64 / x64 (Alpine) | `linux-arm64-musl` / `linux-x64-musl` | WASM |
| Windows | None | WASM |
| Other Node platforms | None in the release matrix | WASM; platform qualification varies |

All six native targets use matching 1.0.0 sidecars. There is no install-time
compiler fallback. See
[`docs/SUPPORT_MATRIX.md`](https://github.com/currentspace/http3/blob/HEAD/docs/SUPPORT_MATRIX.md).

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

server.listen({ port: 443 }); // Dual-stack ::, all interfaces.
```

## Local development

In 0.9.7+, `npx @currentspace/http3 dev-cert --out .certs --trust` creates a constrained
localhost CA, installs local trust explicitly, and issues a certificate. From a
built source checkout use `node bin/http3.cjs dev-cert --out .certs --trust`.
Use `key.pem`/`cert.pem` on the server and set
`NODE_EXTRA_CA_CERTS=/absolute/path/.certs/ca.pem` **before starting Node clients**.
This library's native/WASM clients need an explicit `ca` option instead.

Chrome also needs `--origin-to-force-quic-on=localhost:1` for locally trusted QUIC.
The unused port enables the local-root exception; the application still
negotiates H3 through Alt-Svc. Listeners default to dual-stack `::` (all
interfaces), so localhost works over IPv4 and IPv6. See the
[local development guide](https://github.com/currentspace/http3/blob/HEAD/docs/LOCAL_DEVELOPMENT.md) for trust removal, server
configuration, browser launch commands, and loopback-only binding.

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

## Streaming and cancellation

H3 request and response bodies apply receive-credit flow control in native and
WASM runtimes. Pausing a reader eventually backpressures the peer; other streams
on the connection can continue. QUIC receive windows and transport queues are
separate from Node's readable high-water mark.

Use `pipeline()` or wait for `drain` when `write()` returns `false`. Write
callbacks, `drain`, and `finish` describe local write admission, not receipt or
acknowledgement by the peer. Destroying an unfinished client stream cancels it
remotely and rejects pending writes; normal duplex completion does not reset it.

## Runtime modes

Every QUIC-capable API accepts:

- `runtimeMode: 'auto' | 'fast' | 'portable' | 'wasm'`
- `fallbackPolicy: 'error' | 'warn-and-fallback'`
- `onRuntimeEvent(info)`

Returned client sessions and server objects expose `runtimeInfo`, and `auto`
fallback also emits a process warning with code `WARN_HTTP3_RUNTIME_FALLBACK`.
This includes the Node facade returned by `createNodeServerFactory`.

`auto` prefers native, retaining the Linux `fast` → `portable` fallback when
io_uring is unavailable. If the binding itself cannot load, `auto` with the
default `warn-and-fallback` policy selects WASM before constructing native state.
The warning identifies the OS, architecture, Linux libc, and loader failure.
`runtimeInfo.reasonCode` is `native-binding-unavailable` for this case.
Explicit `fast`/`portable`, or `auto` with `fallbackPolicy: 'error'`, instead fail
with `ERR_HTTP3_RUNTIME_UNSUPPORTED`. Explicit WASM never probes native loading.

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

See [`docs/RUNTIME_MODES.md`](https://github.com/currentspace/http3/blob/HEAD/docs/RUNTIME_MODES.md) for the deployment matrix,
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

## WASM runtime

`runtimeMode: 'wasm'` runs HTTP/3 and raw QUIC clients and Node servers on a
`wasm32-wasip1` build of the same quiche + BoringSSL protocol core, with no
native `.node` addon:

```ts
import { connectAsync } from '@currentspace/http3';

const session = await connectAsync('example.com:443', { runtimeMode: 'wasm' });
```

Large Node stream writes, including `res.end(buffer)`, are admitted in bounded
slices under WASM. Continue to respect writable backpressure; concurrent work
still shares a finite WASM heap. Native writes retain their existing path.

Consumer measurements on 2026-10-30, using 0.9.9 on macOS arm64 / Node 26.13
with a native client, found about 5,060–5,270 requests/s under WASM versus
10,500–12,300 native portable for 1,000 small GETs at concurrency 50. Streamed
32 MiB downloads measured 91–93 MiB/s versus 836–933 MiB/s. These describe those
workloads, not performance guarantees on other hosts. Use native packages for
throughput-sensitive deployments; WASM keeps development and smaller workloads
usable without an addon.

The server side (`Http3SecureServer.listen()` / `QuicServer.listen()`) also
supports `runtimeMode: 'wasm'`, Node-only. The client build has additionally
been verified running inside real Cloudflare workerd — see
[`examples/workerd-client`](https://github.com/currentspace/http3/blob/HEAD/examples/workerd-client/README.md); the only
remaining blocker to a real workerd deployment is that Workers has no outbound
UDP client socket API yet ([cloudflare/workerd#4463](https://github.com/cloudflare/workerd/issues/4463)).

See [`docs/WASM_RUNTIME.md`](https://github.com/currentspace/http3/blob/HEAD/docs/WASM_RUNTIME.md) for the full usage guide
and current Node/workerd support matrix, and
[`docs/WASM_CLIENT_PLAN.md`](https://github.com/currentspace/http3/blob/HEAD/docs/WASM_CLIENT_PLAN.md) for the design.

## Raw QUIC mTLS

- Raw QUIC clients can use mTLS through the stable public `cert` and `key` options on `connectQuic()` and `connectQuicAsync()`.
- Raw QUIC servers support explicit client certificate policy with `clientAuth`, defaulting to `require` when a verification `ca` is configured.
- Raw QUIC server sessions expose the verified peer certificate so applications can inspect or pin exact client certificates with Node's `X509Certificate` API.

See [`CHANGELOG.md`](https://github.com/currentspace/http3/blob/HEAD/CHANGELOG.md) for release notes and
[`docs/RELEASE_EVIDENCE.md`](https://github.com/currentspace/http3/blob/HEAD/docs/RELEASE_EVIDENCE.md) for the current
release's audit ledger and caveats.

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

The experimental [Node-style server factory](https://github.com/currentspace/http3/blob/HEAD/docs/NODE_SERVER.md), introduced
in 0.9.4, supports Fastify integration. Version 0.9.5 adds the compatibility fixes
used by the [Next.js custom-server example](https://github.com/currentspace/http3/blob/HEAD/examples/nextjs/README.md).

- `@currentspace/http3` - canonical API.
- `@currentspace/http3/parity` - http2-style aliases for migrations.
- `@currentspace/http3/h3` - HTTP/3-specific extension namespace.

The package root also exports `createNodeServerFactory` and
`createExpressHandler`. Subpath declarations use `typesVersions` for legacy
TypeScript `moduleResolution: "node"`; modern Node16/NodeNext/bundler resolution
continues to use `exports`. No ambient module declarations are required.

Since 0.9.7, native and WASM H3 request bodies are flow-controlled: a slow or
paused handler holds the client back. A 4 MiB upload can stream through a slow
handler with the default `maxBufferedRequestBytes: 1024 * 1024` (1 MiB).
This option caps unread bytes buffered in the transport stream, including data
already in flight when reading pauses; it is not a total upload-size limit.
Exceeding the cap resets the stream, but large streaming uploads do not normally
need a higher value. Framework body-size limits still apply. See
[request buffering](https://github.com/currentspace/http3/blob/HEAD/docs/NODE_SERVER.md#request-buffering-and-shutdown).

## Examples

- [Raw API example](https://github.com/currentspace/http3/blob/HEAD/examples/raw-api/README.md)
- [Express adapter example](https://github.com/currentspace/http3/blob/HEAD/examples/express-adapter/README.md)
- [Hono `serveFetch()` example](https://github.com/currentspace/http3/blob/HEAD/examples/hono/README.md)
- [Next.js custom server with HTTP/3 and Fast Refresh](https://github.com/currentspace/http3/blob/HEAD/examples/nextjs/README.md)
- [NestJS with Fastify over HTTP/3](https://github.com/currentspace/http3/blob/HEAD/examples/nestjs/README.md)
- [Next.js and NestJS with Express in one process](https://github.com/currentspace/http3/blob/HEAD/examples/nextjs-nestjs/README.md) (0.9.6+)

## Start Here

- [Quickstart](https://github.com/currentspace/http3/blob/HEAD/docs/QUICKSTART.md)
- [Runtime modes and deployment matrix](https://github.com/currentspace/http3/blob/HEAD/docs/RUNTIME_MODES.md)
- [WASM runtime guide](https://github.com/currentspace/http3/blob/HEAD/docs/WASM_RUNTIME.md)
- [Support matrix](https://github.com/currentspace/http3/blob/HEAD/docs/SUPPORT_MATRIX.md)
- [Configuration options reference](https://github.com/currentspace/http3/blob/HEAD/docs/CONFIGURATION_OPTIONS.md)
- [Error handling guide](https://github.com/currentspace/http3/blob/HEAD/docs/ERROR_HANDLING.md)
- [Changelog](https://github.com/currentspace/http3/blob/HEAD/CHANGELOG.md)

## Deployment and Operations

- [QUIC guide](https://github.com/currentspace/http3/blob/HEAD/docs/QUIC_GUIDE.md)
- [Production docs index](https://github.com/currentspace/http3/blob/HEAD/docs/README.md)
- [HTTP/2 parity matrix](https://github.com/currentspace/http3/blob/HEAD/docs/HTTP2_PARITY_MATRIX.md)
- [ECS/Fargate deployment](https://github.com/currentspace/http3/blob/HEAD/docs/ECS_FARGATE_DEPLOYMENT.md)
- [AWS NLB QUIC passthrough](https://github.com/currentspace/http3/blob/HEAD/docs/AWS_NLB_QUIC_PASSTHROUGH.md)
- [Session ticket keys across instances](https://github.com/currentspace/http3/blob/HEAD/docs/SESSION_TICKET_KEYS.md)

## Contributors and Maintainers

- [Test strategy](https://github.com/currentspace/http3/blob/HEAD/docs/TEST_STRATEGY.md)
- [Contributing guide](https://github.com/currentspace/http3/blob/HEAD/CONTRIBUTING.md)
- [Release evidence ledger](https://github.com/currentspace/http3/blob/HEAD/docs/RELEASE_EVIDENCE.md)
- [Release runbook](https://github.com/currentspace/http3/blob/HEAD/docs/RELEASE_RUNBOOK.md)

