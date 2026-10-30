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

The [combined Next.js/NestJS example](../examples/nextjs-nestjs/README.md) uses
Nest's `ExpressAdapter` and Next in one process on the same listener. It requires
the `createExpressHandler` bridge and Cookie normalization changes introduced in
0.9.6; these are not part of 0.9.5.

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
for browser clients. The factory adds `Alt-Svc: h3=":PORT"; ma=86400`; a handler can
replace or remove it. This advertises the bound port, so deployments with port
translation must set the externally reachable port themselves.

## Listener and TypeScript compatibility (unreleased after 0.9.6)

Omitted hosts now bind dual-stack `::`, on all interfaces. Both stream and Node
servers accept object and positional forms. See [local development](./LOCAL_DEVELOPMENT.md)
for loopback-only bindings and locally trusted Chrome H3. The Node factory and
Express bridge are also exported at the root; legacy TypeScript `node` module
resolution can resolve the subpaths through `typesVersions`.

## Supported surface

- `listen({ port, host }, callback)` and `listen(port, host, callback)`, `address()`, `listening`, `close(callback)`,
  and `listening`, `error`, `close` events. Hostnames are resolved before binding.
- Readable request bodies with `method`, `url`, normalized `headers`, `rawHeaders`,
  trailers, protocol version, completion/abort state, and peer address metadata.
- Writable responses with streaming/backpressure, status, header accessors,
  cookies, `writeHead`, `flushHeaders`, `addTrailers`, and body suppression for
  HEAD, 204 and 304. HTTP/1 hop-by-hop response headers are removed on H2/H3.
- A stream-scoped `socket` facade exposes peer IP/port, encryption/ALPN, timeout
  and cancellation. `setKeepAlive()` and `setNoDelay()` return the facade as
  no-ops on H2/H3; they do not tune the shared connection. Readable/writable lifecycle flags reflect the backing stream.
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

## Express applications

Wrap Express with `createExpressHandler` from `@currentspace/http3/express` before
passing it to the Node server factory. Express replaces request/response
prototypes; the bridge preserves the facade's transport methods on each instance,
including when Express mounts a child app. Native HTTP/1 objects pass through.
The regression suite also exercises interleaved H1/H2/H3 traffic, redirects,
and streamed requests/responses through `http-proxy-middleware` 4.2.1.

```js
const express = require('express');
const { createExpressHandler } = require('@currentspace/http3/express');
const app = express();
app.use(express.json());
app.post('/echo', (req, res) => res.json(req.body));
const server = createNodeServerFactory({ ...tls, allowHTTP1: true })(createExpressHandler(app));
server.listen({ port: 8443 });
```

For H2/H3 requests with an unfinished body and no Content-Length, the bridge adds
a request-local `transfer-encoding: chunked` hint so Express's body parser reads
the stream. This hint is not sent on the wire and does not change `rawHeaders`.
Express's body-size limits still apply. When sharing with Next, route to Express
before invoking its middleware, so its parsers cannot consume Next's bodies.

The shared Node facade joins split Cookie fields with `; `, matching the string
expected by cookie-parser and Next's cookie APIs. `rawHeaders` retains the split
H3 fields, and response `Set-Cookie` values remain separate. The stream-level H3
API still exposes duplicate fields as arrays.

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


## Alt-Svc lifetime

The facade advertises `h3=":PORT"; ma=86400` on H1/H2/H3 responses by default.
Configure `altSvc: { maxAge: 604800 }` for a week, or `altSvc: false` to omit the
automatic header. Values are integer seconds from 0 through 2147483647.
A handler can override the header. Disabling advertisements does not erase an
already cached alternative; send `Alt-Svc: clear` when withdrawing one.
A longer lifetime avoids expiry after a minute idle, but does not guarantee a
browser will choose H3: trust, network changes and cached failures still matter.
See [RFC 7838](https://httpwg.org/specs/rfc7838.html#caching).

On H2/H3, `req.socket.setKeepAlive()`, `setNoDelay()` and `setTimeout()` are
chainable no-ops: the socket represents a multiplexed transport. Use
`req.setTimeout()`/`res.setTimeout()` for the existing stream timeout behavior.
Native H1 sockets keep their Node behavior.

## Nest with Express

```ts
import { listenNest } from '@currentspace/http3/nest';

const listener = await listenNest(app, {
  port: 3443,
  tls: { key, cert },
  closeOnSignals: true,
});
// Programmatic shutdown, including Nest lifecycle hooks:
await listener.close();
```

Create/configure the Nest Express application before calling the helper; do not
call `app.listen()`. The helper initializes it, installs the Express bridge and
binds H3/H2/H1 on one port. It has no Nest dependency. FastifyAdapter applications
continue using `createNodeServerFactory` through Fastify's `serverFactory`.

`listener.close()` drains transport then calls `app.close()`, once. Set
`closeApp: false` if another owner must close Nest. `closeOnSignals` defaults to
false; when enabled it owns SIGINT/SIGTERM, invokes Nest shutdown hooks and exits.
Do not also call `app.enableShutdownHooks()` in that mode. Calling `app.close()`
directly does not close the helper's external listener; use `listener.close()`.

## Separate Next and BFF processes

```ts
import { proxy } from '@currentspace/http3/proxy';
const bff = proxy('http://127.0.0.1:3001', {
  rewrite: path => path.replace(/^\/bff(?=\/|$)/, '') || '/',
  timeoutMs: 0, // allow quiet SSE streams
});
const server = createNodeServerFactory({ key, cert, allowHTTP1: true })((req, res) => {
  const path = (req.url ?? '/').split('?')[0];
  if (path === '/bff' || path.startsWith('/bff/')) bff(req, res);
  else nextHandler(req, res);
});
```

The fixed upstream is HTTP/1.1 or HTTPS, with certificate verification enabled
and optional `ca`. Requests/responses stream with backpressure. The handler strips
hop-by-hop and Connection-nominated headers and upstream Alt-Svc (the gateway
keeps its own advertisement), preserves separate Set-Cookie values,
replaces incoming forwarding metadata with this hop's X-Forwarded fields, flushes
SSE headers and cancels upstream work when the client disconnects. It does not
rewrite Location or cookie domains/paths, retry requests, impose body limits, or
forward WebSocket upgrades. Keep Next HMR's upgrade routing on the existing
custom server; this helper handles HTTP requests only. Run it before body parsers.
Only trust its forwarding metadata when the upstream is restricted to this proxy.
The default upstream inactivity timeout is 30 seconds; configure it for SSE.

## Signal shutdown

```ts
const shutdown = server.closeOnSignals({
  timeoutMs: 5000,
  onClose: async () => { await Promise.all([nest.close(), next.close()]); },
});
await shutdown.close(); // idempotent; manual calls never exit the process
```

Signal-triggered calls drain, run `onClose`, and exit (0 on success, 1 on failure
or deadline). Set `exit: false` to retain process ownership. `dispose()` removes
this helper's signal handlers. The deadline includes application cleanup; it is
best-effort transport draining, not a delivery acknowledgement. Register one
controller for a shared listener. The combined Next/Nest example uses this API.
