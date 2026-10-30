// Copied beside a fresh package.json by platform-smoke.mjs. Failures and
// timeouts exit nonzero; all protocol assertions use the installed public API.
const assert = require('node:assert/strict');
const { once } = require('node:events');
const http2 = require('node:http2');
const { join } = require('node:path');
const { setImmediate: turn } = require('node:timers/promises');
const { createNodeServerFactory } = require('@currentspace/http3/node');
const { connectAsync, createQuicServer, connectQuicAsync, Http3Error } = require('@currentspace/http3');
const { createDevTls } = require('@currentspace/http3/dev');

const expected = process.argv[2];
assert.ok(['native', 'wasm'].includes(expected));
const warnings = [];
let nativeFastPathUnavailable = false;
process.on('warning', warning => {
  if (warning.code === 'WARN_HTTP3_RUNTIME_FALLBACK') warnings.push(warning);
});
const timeout = setTimeout(() => {
  console.error('Installed platform consumer timed out');
  process.exit(1);
}, 45000);
const tls = createDevTls({ directory: join(__dirname, 'certs/leaf'), caDirectory: join(__dirname, 'certs/ca') });
function runtime(info, explicit = false) {
  assert.ok(info, 'public runtimeInfo must be available');
  assert.equal(info.requestedMode, explicit ? 'wasm' : 'auto');
  if (explicit || expected === 'wasm') {
    assert.equal(info.selectedMode, 'wasm');
    assert.equal(info.driver, 'wasm');
    assert.equal(info.fallbackOccurred, !explicit);
    assert.equal(info.reasonCode, explicit ? 'requested-wasm' : 'native-binding-unavailable');
  } else {
    assert.ok(['fast', 'portable'].includes(info.selectedMode));
    assert.notEqual(info.reasonCode, 'native-binding-unavailable');
    if (info.fallbackOccurred) assert.equal(info.reasonCode, 'fast-path-unavailable');
    if (info.reasonCode === 'fast-path-unavailable') nativeFastPathUnavailable = true;
  }
}
async function collect(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString();
}
async function h3(mode) {
  let callbacks = 0;
  const events = [];
  const server = createNodeServerFactory({ key: tls.key, cert: tls.cert, runtimeMode: mode, onRuntimeEvent() { callbacks++; } })((req, res) => res.end(`ok ${req.httpVersion}`));
  server.on('runtime', info => events.push(info));
  let session;
  let h2;
  try {
    const listening = once(server, 'listening');
    assert.equal(server.listen(0, '127.0.0.1'), server);
    await listening;
    runtime(server.runtimeInfo, mode === 'wasm');
    assert.equal(events.length, 1, 'facade runtime event must be forwarded once');
    assert.equal(callbacks, 1, 'onRuntimeEvent must run once');
    assert.deepEqual(events[0], server.runtimeInfo);
    const port = server.address().port;
    h2 = http2.connect(`https://localhost:${port}`, { ca: tls.ca });
    await once(h2, 'connect');
    const h2Request = h2.request({ ':path': '/' });
    const h2Body = collect(h2Request);
    const [[h2Headers], h2Text] = await Promise.all([once(h2Request, 'response'), h2Body]);
    assert.equal(h2Headers[':status'], 200);
    assert.equal(h2Text, 'ok 2.0');
    const h2Closed = once(h2, 'close');
    h2.close();
    await h2Closed;
    h2 = undefined;
    session = await connectAsync(`https://localhost:${port}`, { ca: tls.ca, runtimeMode: mode });
    runtime(session.runtimeInfo, mode === 'wasm');
    const request = session.request({ ':method': 'GET', ':path': '/', ':scheme': 'https', ':authority': `localhost:${port}` }, { endStream: true });
    const body = collect(request);
    const [[headers], text] = await Promise.all([once(request, 'response'), body]);
    assert.equal(headers[':status'], '200');
    assert.equal(text, 'ok 3.0');
    console.log(`H2/H3 ${mode}: ${JSON.stringify(server.runtimeInfo)}`);
  } finally {
    h2?.destroy();
    if (session) await session.close();
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
}
async function quic() {
  const server = createQuicServer({ key: tls.key, cert: tls.cert, alpn: ['platform-smoke'], runtimeMode: 'auto' });
  server.on('session', session => session.on('stream', stream => stream.pipe(stream)));
  let client;
  try {
    const address = await server.listen(0, '127.0.0.1');
    runtime(server.runtimeInfo);
    client = await connectQuicAsync(`https://localhost:${address.port}`, { ca: tls.ca, alpn: ['platform-smoke'], runtimeMode: 'auto' });
    runtime(client.runtimeInfo);
    const stream = client.openStream();
    const body = collect(stream);
    stream.end('raw QUIC echo');
    assert.equal(await body, 'raw QUIC echo');
    console.log(`QUIC auto: ${JSON.stringify(client.runtimeInfo)}`);
  } finally {
    if (client) await client.close();
    await server.close();
  }
}
async function nativeRequestRate() {
  const warningCount = warnings.length;
  const server = createNodeServerFactory({ key: tls.key, cert: tls.cert, runtimeMode: 'portable' })((_req, res) => res.end('ok'));
  let client;
  try {
    const listening = once(server, 'listening');
    server.listen(0, '127.0.0.1');
    await listening;
    assert.equal(server.runtimeInfo.selectedMode, 'portable');
    const port = server.address().port;
    client = await connectAsync(`https://localhost:${port}`, { ca: tls.ca, runtimeMode: 'portable' });
    assert.equal(client.runtimeInfo.selectedMode, 'portable');
    let next = 0;
    let completed = 0;
    const start = process.hrtime.bigint();
    await Promise.all(Array.from({ length: 50 }, async () => {
      while (next++ < 1000) {
        const request = client.request({ ':method': 'GET', ':path': '/', ':scheme': 'https', ':authority': `localhost:${port}` }, { endStream: true });
        const [[headers], body] = await Promise.all([once(request, 'response'), collect(request)]);
        assert.equal(headers[':status'], '200');
        assert.equal(body, 'ok');
        completed++;
      }
    }));
    assert.equal(completed, 1000);
    const seconds = Number(process.hrtime.bigint() - start) / 1e9;
    console.log(`Native portable: ${completed} GETs, concurrency 50, ${Math.round(completed / seconds)} req/s (informational)`);
  } finally {
    if (client) await client.close();
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
  await turn();
  assert.equal(warnings.length, warningCount, 'explicit portable must not emit fallback warnings');
}
function unsupported(error) {
  assert.ok(error instanceof Http3Error);
  assert.equal(error.code, 'ERR_HTTP3_RUNTIME_UNSUPPORTED');
  assert.equal(error.reasonCode, 'native-binding-unavailable');
  assert.ok(error.message.includes(process.platform));
  assert.ok(error.message.includes(process.arch));
  if (process.platform === 'linux') assert.match(error.message, /musl|glibc/);
  assert.ok(error.cause);
  return true;
}
async function strict() {
  for (const options of [{ runtimeMode: 'auto', fallbackPolicy: 'error' }, { runtimeMode: 'fast' }, { runtimeMode: 'portable' }]) {
    const server = createNodeServerFactory({ key: tls.key, cert: tls.cert, ...options })(() => {});
    const error = once(server, 'error');
    server.listen(0, '127.0.0.1');
    unsupported((await error)[0]);
    await new Promise(resolve => server.close(resolve));
    await assert.rejects(connectAsync('https://127.0.0.1:9', { ca: tls.ca, ...options }), unsupported);
    const raw = createQuicServer({ key: tls.key, cert: tls.cert, alpn: ['platform-smoke'], ...options });
    try { await assert.rejects(raw.listen(0, '127.0.0.1'), unsupported); }
    finally { await raw.close(); }
    await assert.rejects(connectQuicAsync('https://127.0.0.1:9', { ca: tls.ca, alpn: ['platform-smoke'], ...options }), unsupported);
  }
}
// This branch runs only after a real installed native auto attempt reports
// that its fast path is unavailable. Capable native hosts keep their own path.
async function strictNative() {
  await turn();
  const warningCount = warnings.length;
  const unavailable = error => {
    assert.ok(error instanceof Http3Error);
    assert.equal(error.code, 'ERR_HTTP3_FAST_PATH_UNAVAILABLE');
    assert.equal(error.reasonCode, 'fast-path-unavailable');
    assert.equal(error.driver, 'io_uring');
    assert.equal(typeof error.errno, 'number');
    assert.match(error.syscall, /^io_uring_/);
    assert.equal(error.runtimeInfo.selectedMode, null);
    assert.equal(error.runtimeInfo.fallbackOccurred, false);
    return true;
  };
  for (const options of [{ runtimeMode: 'fast' }, { runtimeMode: 'auto', fallbackPolicy: 'error' }]) {
    const server = createNodeServerFactory({ key: tls.key, cert: tls.cert, ...options })(() => {});
    try {
      await assert.rejects(new Promise((resolve, reject) => {
        server.once('listening', resolve);
        server.once('error', reject);
        server.listen(0, '127.0.0.1');
      }), unavailable);
    } finally {
      await new Promise(resolve => server.close(resolve));
    }
    await assert.rejects(connectAsync('https://127.0.0.1:9', {
      ca: tls.ca, connectTimeoutMs: 1000, ...options,
    }), unavailable);
    const raw = createQuicServer({ key: tls.key, cert: tls.cert, alpn: ['platform-smoke'], ...options });
    try { await assert.rejects(raw.listen(0, '127.0.0.1'), unavailable); }
    finally { await raw.close(); }
    await assert.rejects(connectQuicAsync('https://127.0.0.1:9', {
      ca: tls.ca, alpn: ['platform-smoke'], connectTimeoutMs: 1000, ...options,
    }), unavailable);
  }
  await turn();
  assert.equal(warnings.length, warningCount, 'strict native failures must not emit fallback warnings');
  console.log('Native strict: fast and auto/error rejected across H3 and raw QUIC clients/servers');
}

async function main() {
  await h3('wasm');
  await turn();
  assert.equal(warnings.length, 0, 'explicit WASM must not warn');
  if (expected === 'wasm') await strict();
  await turn();
  assert.equal(warnings.length, 0, 'strict failures must not emit fallback warnings');
  await h3('auto');
  await quic();
  if (expected === 'native') await nativeRequestRate();
  await turn();
  if (expected === 'wasm') {
    assert.equal(warnings.length, 1, 'same missing binding must warn once across servers and clients');
    assert.match(warnings[0].message, /wasm/);
    assert.ok(warnings[0].message.includes(process.platform));
    assert.ok(warnings[0].message.includes(process.arch));
  } else {
    assert.equal(warnings.length, nativeFastPathUnavailable ? 1 : 0, 'native auto must warn exactly once when its fast path is unavailable');
    if (nativeFastPathUnavailable) await strictNative();
    assert.ok(warnings.every(warning => !warning.message.includes('wasm')), 'native install must not fall back to WASM');
  }
  console.log(`PASS ${process.platform}/${process.arch} ${process.version}: ${expected} installed consumer`);
}
main().then(() => clearTimeout(timeout), error => {
  console.error(error);
  process.exitCode = 1;
  // Keep the timeout armed in case a failed implementation leaked a handle.
  timeout.unref();
});
