# NestJS over HTTP/3

Run a NestJS application through `FastifyAdapter` and the experimental
`@currentspace/http3/node` server factory. HTTP/3 uses UDP; HTTP/2 and HTTP/1.1
use TLS/TCP on the same port. No reverse proxy or NestJS patch is required.

This example uses NestJS 12.3.2, Fastify 5.12.7, and the published
`@currentspace/http3@0.9.5`. It requires Node 24+ and a supported native platform:
macOS arm64 or Linux glibc x64/arm64.

## Run

From this directory:

```sh
npm install --ignore-scripts --no-audit --no-fund
npm run certs
npm run build
npm start
```

Open `https://localhost:3443`. `PORT`, `TLS_KEY_FILE`, and `TLS_CERT_FILE` can
override the defaults. The listener binds to `127.0.0.1` for local use.

The certificate command creates a self-signed certificate without changing
system trust. CLI checks can trust it explicitly:

```sh
curl --cacert .certs/cert.pem https://localhost:3443/
curl --http3-only --cacert .certs/cert.pem https://localhost:3443/
```

The second command requires a curl build with HTTP/3 support. The response
reports `httpVersion`, so a successful HTTPS response alone is not evidence of
H3. Browser H3 discovery uses the server's `Alt-Svc` header and requires a
browser-trusted certificate; clicking through a certificate warning is not
sufficient evidence of H3. Permit both TCP and UDP when exposing the listener.

## Use in your application

The integration is in [src/bootstrap.ts](./src/bootstrap.ts):

```ts
const factory = createNodeServerFactory({
  ...tls,
  runtimeMode: 'portable',
  allowHTTP1: true,
  shutdownTimeoutMs: 5000,
});
const adapter = new FastifyAdapter<Server>({
  https: tls,
  serverFactory: factory as unknown as FastifyServerOptions<Server>['serverFactory'],
});
const app = await NestFactory.create<NestFastifyApplication>(AppModule, adapter);
await app.listen(3443, '127.0.0.1');
```

`Server` is imported as a type from `node:https`. The type assertion is confined
to `serverFactory`: Fastify's server types only cover Node's built-in servers.
It does not make the facade a complete `https.Server`. Keep `https: tls` so
Fastify knows the listener is secure; leave `http2: true` unset, since Fastify
would otherwise expect Node's native HTTP/2 request and response objects.

Controllers, dependency injection, pipes, middleware, and exception handling
remain in Nest. This example uses `rawBody: true` for signature input on
`POST /echo`; that route buffers JSON and enforces a 1 MiB parser limit.
`GET /download` demonstrates `StreamableFile`, and `GET /events` demonstrates
Nest's `@Sse()` with a finite observable. SIGINT/SIGTERM use Nest shutdown hooks.

## Verify

```sh
npm test
```

The tests use actual H3, H2, and HTTP/1.1 connections with certificate validation
enabled. They check routing and DI, middleware, protocol metadata, JSON/raw
body handling, parameter validation, error responses, declared body limits,
CORS, file bytes/HEAD, SSE before EOF, stream cancellation, and shutdown.
The H3 client explicitly selects `portable`, matching the server.

These are API tests, not browser negotiation tests or a qualification of all
Nest plugins. CI runs the example against the published package on Node 24/26.

## Limitations

- This integration uses `@nestjs/platform-fastify`. Existing Express middleware
  and Express-specific Nest packages require migration or separate testing.
  Nest's `ExpressAdapter` and `NodeAdapter` are not covered by this example.
- Nest 12.3.2's `app.getUrl()` tests `instanceof tls.Server` and reports an
  `http://` URL for this facade. Use the explicit HTTPS origin as in `main.ts`.
  This affects the reported URL, not encryption or request protocol metadata.
- HTTP/3 WebSockets, WebTransport, multipart uploads, GraphQL, and arbitrary
  Fastify plugins have not been qualified here. `allowHTTP1` permits ordinary
  TLS fallback but does not establish compatibility with Nest gateway plugins.
- Fastify server timeouts and Nest's socket-tracking options are not transport
  configuration. Use the library's transport options; see the
  [Node server contract](../../docs/NODE_SERVER.md) for request-buffer limits,
  unsupported APIs, and bounded best-effort shutdown.

This example adds no new library export or runtime dependency to the library.
