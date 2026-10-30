const next = require('next');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { createNodeServerFactory } = require('@currentspace/http3/node');

const port = Number(process.env.PORT || 3443);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('PORT must be between 1 and 65535');
const hostname = 'localhost';
const server = createNodeServerFactory({
  key: readFileSync(process.env.TLS_KEY_FILE || path.join(__dirname, '.certs/key.pem')),
  cert: readFileSync(process.env.TLS_CERT_FILE || path.join(__dirname, '.certs/cert.pem')),
  runtimeMode: 'portable',
  allowHTTP1: true, // Next's development WebSocket uses the TCP/TLS fallback.
  shutdownTimeoutMs: 1500,
})((req, res) => {
  app.getRequestHandler()(req, res).catch(error => {
    console.error(error);
    if (!res.headersSent) { res.statusCode = 500; res.end(); }
    else res.destroy(error);
  });
});
// Pass the server before prepare() so Next can attach its HMR upgrade handler.
const app = next({
  dev: process.env.NODE_ENV !== 'production',
  dir: __dirname,
  hostname,
  port,
  httpServer: server,
  turbopack: true,
});

let stopPromise;
function stop() {
  return stopPromise ??= Promise.all([
    new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())),
    app.close(),
  ]);
}
function fail(error) {
  console.error(error);
  void stop().then(() => process.exit(1), shutdownError => {
    console.error(shutdownError);
    process.exit(1);
  });
}
server.on('error', fail);
app.prepare().then(() => server.listen({ port, host: process.env.HTTP3_HOST ?? '::' }, () => {
  console.log(`Ready: https://${hostname}:${port}`);
})).catch(fail);
for (const signal of ['SIGINT', 'SIGTERM']) {
  // Next's dev watchers may remain referenced after app.close(). Exit after
  // both cleanup promises settle, including the adapter's bounded drain.
  process.on(signal, () => { void stop().then(() => process.exit(0), fail); });
}
