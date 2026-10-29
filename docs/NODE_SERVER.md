# Node-style HTTP/3 server factory

The experimental `@currentspace/http3/node` export connects Node-style request
handlers to this project's HTTP/3 transport. It also accepts HTTP/2 over TLS on
the same port and optional HTTP/1.1 fallback. No proxy is involved.

The export was introduced in 0.9.4. The Next.js compatibility, HTTP/1 upgrade,
and packet-limit fixes described here are included in 0.9.5. See the
[Next.js custom-server example](../examples/nextjs/README.md) for local development,
Fast Refresh, and browser protocol verification.

The [NestJS example](../examples/nestjs/README.md) uses Nest's `FastifyAdapter`
with the same factory for controllers, JSON parsing, file streaming, and SSE.
It runs on published 0.9.5 without a NestJS patch.

```js
const fs = require('node:fs');
const Fastify = require('fastify');
const { createNodeServerFactory } = require('@currentspace/http3/node');
const tls = {
  key: fs.readFileSync(process.env.TLS_KEY_FILE),
  cert: fs.readFileSync(process.env.TLS_CERT_FILE),
};
const app = Fastify({
  https: tls,
  serverFactory: createNodeServerFactory({
    ...tls,
    runtimeMode: 'portable',
    allowHTTP1: true,
  }),
});
app.get('/', request => ({ httpVersion: request.raw.httpVersion }));
app.listen({ port: 8443, host: '127.0.0.1' }).catch(console.error);
```

Requires Node 24+. Native prebuilds support macOS arm64 and Linux glibc x64/arm64.
See [the support matrix](./SUPPORT_MATRIX.md) before using another platform.
Permit both TCP and UDP on the selected port. Use a browser-trusted certificate
for browser clients. The factory adds `Alt-Svc: h3=":PORT"; ma=60`; a handler can
replace or remove it. This advertises the bound port, so deployments with port
translation must set the externally reachable port themselves.

## Supported surface

- `listen({ port, host }, callback)`, `address()`, `listening`, `close(callback)`,
  and `listening`, `error`, `close` events. Hostnames are resolved before binding.
- Readable request bodies with `method`, `url`, normalized `headers`, `rawHeaders`,
  trailers, protocol version, completion/abort state, and peer address metadata.
- Writable responses with streaming/backpressure, status, header accessors,
  cookies, `writeHead`, `flushHeaders`, `addTrailers`, and body suppression for
  HEAD, 204 and 304. HTTP/1 hop-by-hop response headers are removed on H2/H3.
- A stream-scoped `socket` facade exposes peer IP/port, encryption/ALPN, timeout
  and cancellation. Readable/writable lifecycle flags reflect the backing stream.
  Cancelling it resets the request stream, not other requests sharing its QUIC/H2
  connection.

The H3/H2 objects are facades, not instances of Node's `IncomingMessage`,
`ServerResponse` or `net.Socket`. The handler signature accepts familiar Node
handler types for interoperability; that does not promise the entire Node HTTP
API. Fastify's TypeScript `RawServer` union currently requires a separate type
integration; the accompanying example is JavaScript.

With `allowHTTP1: true`, the public `upgrade(request, socket, head)` event forwards
HTTP/1.1 upgrades from the TLS listener. The application owns the handshake;
without an upgrade listener the socket is closed. Upgraded sockets remain subject
to the shutdown deadline. HTTP/2 and HTTP/3 streams do not emit this event.

HTTP/3 WebSockets/WebTransport, CONNECT tunnels, raw QUIC socket writes,
informational responses (`writeEarlyHints`/`writeContinue`), Unix sockets, server
re-listening and the complete Node server API are not implemented. Plugins that
require those APIs or `instanceof` checks need separate qualification. Fastify's
built-in server construction options (including connection/request/keep-alive
and HTTP/2 session timeouts) are not automatically applied by a custom factory.
Configure transport timeouts through this package's `ServerOptions` and use
framework hooks for application deadlines.

## Request buffering and shutdown

`maxBufferedRequestBytes` defaults to 1 MiB and bounds unread H3 bytes buffered in
the transport stream. Exceeding it resets that stream; it is not a replacement
for Fastify's `bodyLimit`. H2 uses Node's stream backpressure. The wrapper itself
also uses Node readable/writable high-water marks.

`shutdownTimeoutMs` defaults to 30,000. `close()` rejects new requests, waits for
active response writes, invokes session shutdown, and then stops the transport.
At the deadline, it forces shutdown, so unfinished responses may be truncated.
The existing native H3 session close uses GOAWAY followed by a short deferred
CONNECTION_CLOSE; it does not expose per-response acknowledgement completion.
Consequently this is bounded, best-effort draining, not a delivery guarantee on
lossy or high-latency paths. Applications needing stronger guarantees must
coordinate shutdown with their clients/load balancer before calling `close()`.
