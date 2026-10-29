# Next.js development over HTTP/3

This opt-in custom server runs Next.js on HTTP/3 with HTTP/2 and HTTP/1.1 TLS
fallback on the same port. Fast Refresh uses an HTTP/1.1 WebSocket on TCP;
pages, assets, API routes and RSC navigation can use HTTP/3 on UDP.

Requires Node 24+ and the native build of this 0.9.5 candidate checkout. **Published `@currentspace/http3@0.9.4` does not contain these fixes.**
This example imports `../../dist/node-server.js` to test the local build.
Next.js 16.4.3 and React 19.3.1 are pinned for reproducibility.

From the repository root:

```sh
pnpm install --frozen-lockfile
pnpm build
TLS_KEY_FILE=/absolute/path/localhost-key.pem \
TLS_CERT_FILE=/absolute/path/localhost.pem \
  pnpm --dir examples/nextjs dev
```

Open `https://localhost:3443`. Set `PORT` to change the port; allow both TCP and
UDP. For normal browser use, supply a certificate trusted by your browser with
`localhost` in its subject alternative names. A certificate warning bypass is
not sufficient evidence that QUIC will work. Certificate creation and system
trust installation are separate from this example.

The response advertises `Alt-Svc`. In Chromium DevTools enable the Network
**Protocol** column and confirm `h3` on document/assets requests. The initial
connection may be `h2` until the browser learns the alternative service.
Successful page loading alone does not prove HTTP/3.

## Automated checks

```sh
pnpm --dir examples/nextjs test:adapter
TLS_KEY_FILE=/absolute/path/localhost-key.pem \
TLS_CERT_FILE=/absolute/path/localhost.pem \
  pnpm --dir examples/nextjs test:browser
```

The browser check requires the root Playwright dependency and its Chromium
installation (`pnpm exec playwright install chromium`). It launches an isolated
Chromium process, pins only the supplied certificate's public key, and forces
QUIC for `localhost:PORT`. It does not modify system certificate trust. This
verifies HTTP/3 operation, not automatic Alt-Svc discovery or other browsers.
Use `HTTP3_TEST_PROTOCOL=h2` for the HTTP/2 control. Optional
`HTTP3_TEST_REPORT=/absolute/path/report.json` saves protocol evidence and logs.

The check owns the selected port, starts/stops its server, edits `app/counter.jsx`
temporarily and restores it in `finally`. Run one browser check at a time. It
asserts hydration, gzip responses, state-preserving Fast Refresh, a POST body,
complete RSC body reads, client navigation without a document reload, and shutdown with the development WebSocket connected.
The check waits for Fast Refresh traffic to settle before testing RSC reads and
navigation. Speculative link prefetch is disabled in the fixture. The adapter checks separately exercise Next's
static-file middleware and gzip streaming, including `flushHeaders()` and HEAD.

## Integration with your app

Use the `server.cjs` pattern and set Next's `dir` to your application directory.
Pass the facade as Next's `httpServer` so Next can register its upgrade handler;
`allowHTTP1: true` is needed for that WebSocket. No private server fields or
per-request compatibility patches are needed. Handle handler promise rejections
and close both the facade and the Next application on shutdown.

The standalone example exits after both close operations finish because Next's
dev watchers can remain referenced after `app.close()`. The adapter independently
bounds socket draining with `shutdownTimeoutMs`; it does not call `process.exit`.

This is a development example, not a `next dev --http3` flag or a change to
Next.js itself. Production custom-server deployment, standalone output, edge
runtimes, WebTransport and WebSockets over HTTP/3 are outside this example's
qualification. See [the Node adapter contract](../../docs/NODE_SERVER.md).
