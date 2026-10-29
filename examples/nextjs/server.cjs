const next = require('next');
const { readFileSync } = require('node:fs');
const { createNodeServerFactory } = require('../../dist/node-server.js');

const port = Number(process.env.PORT || 3443);
const hostname = 'localhost';
const server = createNodeServerFactory({
  key: readFileSync(process.env.TLS_KEY_FILE),
  cert: readFileSync(process.env.TLS_CERT_FILE),
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
// Providing httpServer lets Next attach its development upgrade handler.
const app = next({ dev: true, dir: __dirname, hostname, port, httpServer: server, turbopack: true });
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
    console.error(shutdownError); process.exit(1);
  });
}
server.on('error', fail);
app.prepare().then(() => server.listen({ port, host: '127.0.0.1' }, () => {
  console.log(`Ready: https://${hostname}:${port}`);
})).catch(fail);
for (const signal of ['SIGINT', 'SIGTERM']) {
  // Next's dev file watchers can remain referenced after app.close(). Exit only
  // after both cleanup promises finish; the adapter owns the drain deadline.
  process.on(signal, () => { void stop().then(() => process.exit(0), error => {
    console.error(error); process.exit(1);
  }); });
}
