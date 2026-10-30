import assert from 'node:assert/strict';
import { once } from 'node:events';
import { test } from 'node:test';
import { createSecureServer } from '../../lib/server.js';
import { Http3NodeServer } from '../../lib/node-server.js';
import { WasmH3ServerEventLoop } from '../../lib/wasm/h3-server-event-loop.js';
import { QuicServer } from '../../lib/quic-server.js';
import { connectQuic } from '../../lib/quic-client.js';
import { generateMutualTlsTestCerts, generateTestCerts } from '../support/generate-certs.js';
import { wasmSkipReason } from '../support/wasm-test-helpers.js';

for (const option of [{ reusePort: true }, { sessionTicketKeys: Buffer.alloc(48) }]) {
  test(`WASM rejects unsupported server option ${Object.keys(option)[0]}`, { skip: wasmSkipReason() }, async t => {
    const server = createSecureServer({ ...generateTestCerts(), runtimeMode: 'wasm', ...option });
    t.after(() => server.close());
    const failed = once(server, 'error');
    let selected = false;
    server.on('runtime', () => { selected = true; });
    server.listen(0, '127.0.0.1');
    const [error] = await failed;
    assert.equal(error.code, 'ERR_HTTP3_RUNTIME_UNSUPPORTED');
    assert.equal(error.reasonCode, 'wasm-option-unsupported');
    assert.equal(server.runtimeInfo?.selectedMode, null);
    assert.equal(selected, false);
  });
}

test('WASM close during deferred UDP bind never publishes a successful runtime', { skip: wasmSkipReason() }, async t => {
  let resume!: () => void;
  let started!: () => void;
  const pending = new Promise<void>(resolve => { resume = resolve; });
  const entered = new Promise<void>(resolve => { started = resolve; });
  // Original method is called with its receiver below.
  // eslint-disable-next-line @typescript-eslint/unbound-method
  const listen = WasmH3ServerEventLoop.prototype.listen;
  t.mock.method(WasmH3ServerEventLoop.prototype, 'listen', async function (this: WasmH3ServerEventLoop, port: number, host: string) {
    started();
    await pending;
    return listen.call(this, port, host);
  });
  const server = createSecureServer({ ...generateTestCerts(), runtimeMode: 'wasm' });
  t.after(() => server.close());
  server.on('runtime', () => assert.fail('cancelled startup selected a runtime'));
  server.on('listening', () => assert.fail('cancelled startup started listening'));
  server.listen(0, '127.0.0.1');
  await entered;
  const closed = server.close();
  resume();
  await closed;
  assert.equal(server.address(), null);
  assert.equal(server.runtimeInfo?.selectedMode, null);
});

test('Node facade exposes pending runtime and delivers selection exactly once', { skip: wasmSkipReason() }, async t => {
  let callbacks = 0;
  const server = new Http3NodeServer({ ...generateTestCerts(), runtimeMode: 'wasm', onRuntimeEvent: () => { callbacks++; } }, (_req, res) => res.end());
  t.after(() => new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())));
  assert.equal(server.runtimeInfo?.selectedMode, null);
  let events = 0;
  server.on('runtime', info => {
    events++;
    assert.equal(info, server.runtimeInfo);
  });
  const ready = once(server, 'listening');
  assert.equal(server.listen(0, '127.0.0.1'), server);
  await ready;
  assert.equal(server.runtimeInfo?.selectedMode, 'wasm');
  assert.equal(callbacks, 1);
  assert.equal(events, 1);
});

test('WASM raw QUIC keeps required client authentication after runtime selection', { skip: wasmSkipReason() }, async t => {
  const tls = generateMutualTlsTestCerts();
  const server = new QuicServer({ ...tls.server, ca: tls.ca.cert, clientAuth: 'require', runtimeMode: 'wasm', disableRetry: true });
  t.after(() => server.close());
  let sessions = 0;
  server.on('session', () => { sessions++; });
  const addr = await server.listen(0, '127.0.0.1');
  const client = connectQuic(`https://localhost:${addr.port}`, { ca: tls.ca.cert, servername: 'localhost', runtimeMode: 'wasm', connectTimeoutMs: 3000 });
  t.after(() => client.close());
  client.on('error', () => {});
  await new Promise<void>(resolve => client.once('close', resolve));
  assert.equal(sessions, 0, 'an anonymous client must not create an authenticated session');
});
