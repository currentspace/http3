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

test('raw QUIC close waits for deferred WASM startup cleanup without announcing selection', { skip: wasmSkipReason() }, async t => {
  const { WasmQuicServerEventLoop } = await import('../../lib/wasm/quic-server-event-loop.js');
  const { createSocket } = await import('node:dgram');
  let resume!: () => void;
  let entered!: () => void;
  let port = 0;
  let loopCloses = 0;
  const gate = new Promise<void>(resolve => { resume = resolve; });
  const bound = new Promise<void>(resolve => { entered = resolve; });
  // Original prototype methods are invoked with their receiver below.
  // eslint-disable-next-line @typescript-eslint/unbound-method
  const listen = WasmQuicServerEventLoop.prototype.listen;
  // eslint-disable-next-line @typescript-eslint/unbound-method
  const close = WasmQuicServerEventLoop.prototype.close;
  t.mock.method(WasmQuicServerEventLoop.prototype, 'listen', async function(this: InstanceType<typeof WasmQuicServerEventLoop>, targetPort: number, host: string) {
    const address = await listen.call(this, targetPort, host);
    port = address.port;
    entered();
    await gate;
    return address;
  });
  t.mock.method(WasmQuicServerEventLoop.prototype, 'close', async function(this: InstanceType<typeof WasmQuicServerEventLoop>) {
    loopCloses++;
    return close.call(this);
  });
  const server = new QuicServer({ ...generateTestCerts(), runtimeMode: 'wasm' });
  t.after(() => server.close());
  let closes = 0;
  server.on('close', () => { closes++; });
  server.on('runtime', () => assert.fail('cancelled startup selected a runtime'));
  server.on('listening', () => assert.fail('cancelled startup published a listener'));
  const opening = server.listen(0, '127.0.0.1');
  const rejected = assert.rejects(opening, { code: 'ERR_HTTP3_INVALID_STATE', reasonCode: 'startup-cancelled' });
  await bound;
  const closing = server.close();
  const alsoClosing = server.close();
  assert.equal(closes, 0, 'close must wait for local loop cleanup');
  resume();
  await Promise.all([rejected, closing, alsoClosing]);
  assert.equal(closes, 1);
  assert.equal(loopCloses, 1);
  assert.equal(server.address(), null);
  assert.equal(server.runtimeInfo?.selectedMode, null);
  const probe = createSocket('udp4');
  t.after(() => new Promise<void>(resolve => probe.close(() => resolve())));
  probe.bind(port, '127.0.0.1');
  await once(probe, 'listening');
});

for (const runtimeMode of ['wasm', 'portable'] as const) {
  test(`raw QUIC ${runtimeMode} can restart from its close event and close the new listener`, {
    skip: runtimeMode === 'wasm' ? wasmSkipReason() : false,
  }, async t => {
    const server = new QuicServer({ ...generateTestCerts(), runtimeMode });
    t.after(() => server.close());
    let selections = 0;
    let closes = 0;
    server.on('runtime', () => { selections++; });
    server.on('close', () => { closes++; });
    await server.listen(0, '127.0.0.1');
    let restarting: ReturnType<QuicServer['listen']> | undefined;
    server.once('close', () => {
      assert.equal(server.address(), null);
      restarting = server.listen(0, '127.0.0.1');
    });
    await server.close();
    assert.ok(restarting, 'close event should start the next listener');
    const address = await restarting;
    assert.deepEqual(server.address(), address);
    assert.equal(server.runtimeInfo?.selectedMode, runtimeMode);
    assert.equal(selections, 2);
    await Promise.all([server.close(), server.close()]);
    assert.equal(server.address(), null);
    assert.equal(closes, 2, 'each listener must emit close exactly once');
  });

  test(`raw QUIC ${runtimeMode} can cancel a restart from its close event`, {
    skip: runtimeMode === 'wasm' ? wasmSkipReason() : false,
  }, async t => {
    const server = new QuicServer({ ...generateTestCerts(), runtimeMode });
    t.after(() => server.close());
    let selections = 0;
    let closes = 0;
    server.on('runtime', () => { selections++; });
    server.on('close', () => { closes++; });
    await server.listen(0, '127.0.0.1');
    let restartedAndClosed: Promise<void> | undefined;
    server.once('close', () => {
      const restarted = assert.rejects(server.listen(0, '127.0.0.1'), {
        code: 'ERR_HTTP3_INVALID_STATE', reasonCode: 'startup-cancelled',
      });
      restartedAndClosed = Promise.all([restarted, server.close()]).then(() => {});
    });
    await server.close();
    assert.ok(restartedAndClosed, 'close event should start the next lifecycle');
    await restartedAndClosed;
    assert.equal(server.address(), null);
    assert.equal(server.runtimeInfo?.selectedMode, null);
    assert.equal(selections, 1, 'cancelled restart must not publish a new selection');
    assert.equal(closes, 2);
  });
}
