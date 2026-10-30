# Next.js and NestJS with Express on one HTTP/3 listener

One Node process runs Next.js, Nest's `ExpressAdapter`, and an Express sub-app.
The shared listener serves HTTP/3 over UDP and HTTP/2 or HTTP/1.1 over TCP at
`https://localhost:3443`. It does not start a second backend listener or proxy
process. Nest allocates its usual internal server object, but never binds it.

This example requires **`@currentspace/http3` 0.9.6 or later**. Version 0.9.5 does
not contain `createExpressHandler` or the split Cookie fix. Nest and Next
themselves are unmodified.

## Run with the released package

Copy this example directory to a project outside the library's pnpm workspace:

```sh
npm install --ignore-scripts --no-audit --no-fund
npm run certs
npm run dev
```

## Run a source candidate from this repository

Use Node 24+ on a supported native platform. From the repository root, install
dependencies and build the library, then install its candidate tarball in the
example. The local tarball override below tests this checkout, including before
the next version is on npm. A tarball also avoids a package
symlink outside Turbopack's example root.

```sh
pnpm install --frozen-lockfile
pnpm run build
npm pack --ignore-scripts
cd examples/nextjs-nestjs
npm install --no-save --package-lock=false --ignore-scripts --no-audit --no-fund ../../currentspace-http3-0.9.6.tgz
npm run certs
npm run dev
```

The certificate helper only creates local files; it does not change system
trust. For manual browser use, supply a locally trusted certificate with
`TLS_KEY_FILE` and `TLS_CERT_FILE`. `TLS_CA_FILE` sets the CA trusted by the
example's internal H3 client (defaults to the certificate). The hostname is
`localhost`; configure it consistently if adapting this example.

For a built application:

```sh
npm run build
npm start
```

`PORT` defaults to 3443. Both UDP and TCP must be available. Ordinary browsers
first use TCP, learn the `Alt-Svc` advertisement, and can then switch to H3.
Next's development HMR WebSocket uses the HTTP/1.1 upgrade path on the same TLS
listener. It is not an H3 WebSocket.

## Request ownership

The entry point routes `/bff` and `/bff/*` to Nest/Express, and `/express` and
`/express/*` to the Express sub-app. All other paths go directly to Next,
including `/_next`, Route Handlers, RSC navigation, and Server Actions. Dispatch
happens before Express's parsers or prototype decoration, preserving Next's
request bodies. A missing BFF route stays a Nest 404.

Nest uses its normal controllers, dependency injection, guards, pipes, exception
filters, JSON parsing, raw-body access, `StreamableFile`, and SSE. The upload route
reads an octet-stream incrementally; it does not buffer the complete upload.
The guard and cookies are demonstrations, not a complete authentication system.

The homepage and `/api/bff` explicitly use this library's H3 client to call
`/bff/identity` through the same listener, forwarding the incoming Cookie string.
This demonstrates SSR and a Next-to-BFF network call without another process.
Node's built-in `fetch` does not automatically become an H3 client. In a real
application, direct calls to shared services can avoid this network round trip;
the example opens a session per call to keep ownership and cleanup explicit.

Next and Nest see the same secure, HttpOnly session cookie. Chromium may split
Cookie into several H3 fields. The Node facade combines them with `; ` before
either framework sees them; `rawHeaders` retains the original fields.
Separate `Set-Cookie` response fields remain separate.

One bootstrap owns shutdown: stop accepting requests and drain the listener,
then close both frameworks. All responses carry the process ID for verification.

## Verify

```sh
npm run build:server
npx playwright install chromium
npm run test:protocols
npm run test:browser
HTTP3_TEST_PROTOCOL=h2 npm run test:browser
npm run build
NODE_ENV=production npm run test:protocols
NODE_ENV=production npm run test:browser
NODE_ENV=production HTTP3_TEST_PROTOCOL=h2 npm run test:browser
```

The API suite checks H3, H2, and HTTP/1.1. The browser suite checks Alt-Svc
negotiation, shared PID, hydration, Server Actions, split cookies, SSR and BFF
forwarding, an 8 MiB upload with server progress before producer EOF, SSE,
repeated RSC navigation, shutdown, and development Fast Refresh preserving state.
`HTTP3_TEST_REPORT` writes the protocol evidence as JSON. Tests use port 3457
unless `HTTP3_TEST_PORT` is set, and restore the source edited for Fast Refresh.

The browser test pins the generated certificate for its own Chromium process.
Its unused `localhost:1` QUIC flag enables Chromium's local certificate exception;
the application port is learned through Alt-Svc, not forced to QUIC. These tests
do not establish public certificate trust or qualify other browsers. The pinned
Chromium 157 test build also avoids the older Chromium RSC issue documented in
the [standalone Next example](../nextjs/README.md).

Local qualification on macOS arm64, 2026-10-29: Node 24.22.1 and 26.13.0 passed
the development and production browser suites with Chromium 157.0.8103.15 over
both H3 and H2 (eight combinations). H3 runs observed two raw Cookie fields and
verified the joined values through Nest, Next, SSR, and the Next-to-BFF call.
The API suite passed across H3/H2/HTTP1, and the library's 240 core tests passed
on both Node versions. This includes mixed-protocol Express requests, redirects,
and streamed proxy uploads using `http-proxy-middleware` 4.2.1. The Linux CI
workflow is provided but has not been run as
part of this local qualification.

This uses Next's custom-server mode, not its generated standalone server. The
Node facade supports a subset of Node's HTTP APIs; plugins needing raw sockets,
informational responses, or HTTP/3 WebSockets need separate support. See
[the facade contract](../../docs/NODE_SERVER.md) and
[Next's custom-server guidance](https://nextjs.org/docs/app/guides/custom-server).
