# Next.js over HTTP/3

A local Next.js App Router application using the experimental
`@currentspace/http3/node` custom-server adapter. It serves HTTP/3 over UDP and
HTTP/2 or HTTP/1.1 over TLS/TCP on the same port. Next's Fast Refresh WebSocket
uses the HTTP/1.1 upgrade bridge; page, asset, API, and RSC traffic can use HTTP/3.

This example uses the published `@currentspace/http3@0.9.5`. No patch to Next.js
is required. Next and React are dependencies of this example only.

## Run locally

Use Node.js 24+ on a supported native platform: macOS arm64 or Linux x64/arm64
with glibc. OpenSSL must be available for the certificate helper. See the
[support matrix](../../docs/SUPPORT_MATRIX.md) for other environments.

From this directory (or a copy of it outside the repository):

```sh
npm install
npm run certs
npm run dev
```

Open `https://localhost:3443`. The generated certificate is self-signed and valid
for localhost for 30 days. The helper does not install a CA or alter system
trust. Existing certificate files are never overwritten. To supply your own:

```sh
TLS_KEY_FILE=/absolute/path/key.pem TLS_CERT_FILE=/absolute/path/cert.pem npm run dev
```

Click the counter to verify hydration, edit its label in `app/counter.jsx` to
exercise Fast Refresh, and click **About** to exercise RSC client navigation.
The `POST /api/echo` route echoes a small text body as JSON. It uses
`request.text()` and buffers that body; use the separate upload example below
to exercise streaming.

To run the built application:

```sh
npm run build
npm start
```

`PORT=4443 npm run dev` changes the port. The example binds only to IPv4 loopback
(`127.0.0.1`); use the hostname `localhost` in the URL. The `portable` runtime
avoids requiring Linux io_uring capabilities for this local workflow.

## Streaming file uploads

Choose a file in the **Streaming file upload** form and click **Upload file**.
The browser sends `file.stream()` with `duplex: 'half'`. The `/api/upload` Node
route reads `request.body` chunk by chunk and returns the received byte count,
chunk count, and SHA-256 digest. It retains neither the complete body nor a file
on disk. The chunk count depends on transport delivery, not the original file's
chunk boundaries.

The browser request follows this pattern:

```js
const response = await fetch('/api/upload', {
  method: 'POST',
  headers: {
    'Content-Type': 'application/octet-stream',
    'X-File-Size': String(file.size),
  },
  body: file.stream(),
  duplex: 'half',
});
const { received, sha256 } = await response.json();
```

`duplex: 'half'` means the response is read after the upload finishes. The route
still consumes request chunks while the browser is sending the body. This is a
transport demonstration; adapt the route for storage and your application's
upload limits before using it as an upload service.

### `ERR_ALPN_NEGOTIATION_FAILED`

HTTPS alone is insufficient for Chromium's streaming fetch requests. In our
Next 16.4.3 reproduction, `next dev --experimental-https` negotiated HTTP/1.1:
`file.stream()` failed with `ERR_ALPN_NEGOTIATION_FAILED`, while sending the same
file as a non-stream body succeeded. Start this custom server with `npm run dev`
and verify `h2` or `h3` in DevTools. It provides these protocols directly without
Nginx. This addresses the scenarios in Next.js discussions
[#19653](https://github.com/vercel/next.js/discussions/19653) and
[#85001](https://github.com/vercel/next.js/discussions/85001), as tested on Node
24/26 with Chromium 157.0.8103.15; it does not imply identical behavior in every
browser or historical Next release.

## Verify the protocol

A working HTTPS page alone does **not** prove HTTP/3. Enable the **Protocol**
column in browser DevTools: `h3` is HTTP/3, `h2` is the TCP fallback. The server
advertises `Alt-Svc` on HTTPS responses so a compatible browser can discover H3
on subsequent requests. Both TCP and UDP must be reachable on the chosen port.

Chromium may reject QUIC with a locally issued certificate even when the HTTPS
certificate warning has been bypassed. The automated smoke test below uses an
isolated browser and explicit localhost certificate exceptions; it does not
claim that an unmodified browser automatically accepts self-signed QUIC.

## Browser smoke tests

Stop any manually running example server first. The test starts its own server,
temporarily edits and restores `app/counter.jsx` to check Fast Refresh, and
closes the server with the HMR connection still active. Do not run two smoke
tests against the same example directory concurrently.

```sh
npx playwright install chromium
npm run smoke
HTTP3_TEST_PROTOCOL=h2 npm run smoke
npm run build
NODE_ENV=production npm run smoke
NODE_ENV=production HTTP3_TEST_PROTOCOL=h2 npm run smoke
```

The test verifies initial HTTP/2, the advertised Alt-Svc port, subsequent HTTP/3
negotiation, compressed responses, hydration, ten client navigations, RSC body
consumption, POST data, streaming uploads, and shutdown. Development mode also
checks Fast Refresh and its HTTP/1.1 WebSocket handshake. All request failures and browser errors
remain test failures. The H2 control disables QUIC and requires H2 throughout.

Upload checks cover 64 KiB and 8 MiB `File.stream()` bodies, an 8 MiB controlled
stream, and the file-picker UI. Byte counts and SHA-256 digests must match. The
controlled stream withholds all data after the first 64 KiB until the server
confirms consuming a chunk, proving that it does not wait for upload completion
to read the body. The smoke harness enables `HTTP3_SMOKE_UPLOAD_PROGRESS=1` for
that observation; ordinary example runs do not emit those test markers. These
checks run in the example CI's H2/H3 and development/built-app matrix.

For the local H3 test, Chromium receives a certificate public-key pin exception
and `--origin-to-force-quic-on=localhost:1`. Port **1 is unused**: that flag permits
a local root for the hostname while only forcing transport for port 1. The app
port remains unforced; the test requires initial H2 and then H3 after discovery.
These browser flags are diagnostic settings, not server or deployment settings.

Optional settings:

```sh
HTTP3_TEST_NAVIGATION_ROUNDS=30 HTTP3_TEST_REPORT=/tmp/next-http3.json npm run smoke
CHROMIUM_EXECUTABLE=/absolute/path/to/chromium npm run smoke
```

The test dependency is pinned to Playwright `1.65.0-alpha-2026-10-29`, whose
Chromium `157.0.8103.15` passed this fixture. The previously tested Chromium
`156.0.8078.4` can report `ERR_ABORTED` after a streamed `no-store` response has
already reached EOF. That issue also reproduces with plain Node HTTP, without
Next or this library. See the [Chromium fix](https://chromium-review.googlesource.com/c/chromium/src/+/8508456)
and [Playwright issue](https://github.com/microsoft/playwright/issues/42742).
Use a browser containing the fix; do not discard request failures or weaken
Next's cache policy to make the tests pass. The prerelease dependency is for
this example's tests, not the server runtime.

## Use with an existing Next.js app

1. Install `@currentspace/http3@^0.9.5` in your app.
2. Copy `server.cjs` to the app root and supply TLS certificate paths.
3. Use `node server.cjs` for development and
   `NODE_ENV=production node server.cjs` after `next build` for the built app.
4. Retain `httpServer: server` and `allowHTTP1: true` for development HMR.
   Next owns the upgrade handler; do not attach a second HMR proxy.

This is a [Next.js custom server](https://nextjs.org/docs/app/guides/custom-server).
`next dev` and `next start` launch Next's own listener instead of this adapter.
Next's `output: 'standalone'` server cannot be combined with this custom server.
This example is for local testing and self-hosted experimentation, not a Vercel
deployment configuration or a claim that Next.js natively supports HTTP/3.

Validation covers this small App Router fixture. It does not establish support
for every Next feature, every browser, or every deployment environment.
