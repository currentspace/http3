import assert from 'node:assert/strict';
import dgram from 'node:dgram';
import { once } from 'node:events';
import { test } from 'node:test';
import { bindNodeUdpServer } from '../../lib/wasm/node-udp-server-adapter.js';

for (const mode of ['occupied', 'synchronous-failure'] as const) {
  test(`WASM UDP server releases its socket before rejecting ${mode} bind`, async t => {
    const blocker = dgram.createSocket('udp4');
    t.after(() => new Promise<void>(resolve => blocker.close(() => resolve())));
    blocker.bind(0, '127.0.0.1');
    await once(blocker, 'listening');
    const create = dgram.createSocket.bind(dgram);
    const failure = Object.assign(new Error('synchronous bind failure'), { code: 'EINVAL' });
    let closed = 0;
    let socket: dgram.Socket | undefined;
    t.mock.method(dgram, 'createSocket', (options: dgram.SocketOptions) => {
      socket = create(options);
      socket.once('close', () => { closed++; });
      if (mode === 'synchronous-failure') t.mock.method(socket, 'bind', () => { throw failure; });
      return socket;
    });
    // Cleanup remains bounded even if the regression reintroduces the leak.
    t.after(() => { if (closed === 0) { try { socket?.close(); } catch { /* already closed */ } } });
    await assert.rejects(bindNodeUdpServer(blocker.address().port, '127.0.0.1'), (error: unknown) => {
      assert.ok(error instanceof Error && 'code' in error);
      if (mode === 'synchronous-failure') assert.equal(error, failure);
      assert.equal(error.code, mode === 'occupied' ? 'EADDRINUSE' : 'EINVAL');
      return true;
    });
    assert.equal(closed, 1);
  });
}
