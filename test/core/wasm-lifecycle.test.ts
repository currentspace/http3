import { it } from 'node:test';
import assert from 'node:assert/strict';
import { DeadlineTimer } from '../../lib/wasm/lifecycle.js';
import { WasmH3ClientEventLoop } from '../../lib/wasm/h3-client-event-loop.js';
import { WasmQuicClientEventLoop } from '../../lib/wasm/quic-client-event-loop.js';
import { WasmH3ServerEventLoop } from '../../lib/wasm/h3-server-event-loop.js';
import { WasmQuicServerEventLoop } from '../../lib/wasm/quic-server-event-loop.js';
import { connectNodeUdp } from '../../lib/wasm/node-udp-adapter.js';
import dgram from 'node:dgram';
import { EventEmitter } from 'node:events';

it('a fired WASM timer can rearm the same deadline, then cancel', t => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 0 });
  const timer = new DeadlineTimer();
  let ticks = 0;
  timer.arm(10, () => {
    ticks++;
    timer.arm(0, () => { ticks++; });
  });
  t.mock.timers.tick(10);
  t.mock.timers.tick(1);
  assert.equal(ticks, 2);
  timer.arm(20, () => { ticks++; });
  timer.cancel();
  t.mock.timers.tick(100);
  assert.equal(ticks, 2);
});

for (const Client of [WasmH3ClientEventLoop, WasmQuicClientEventLoop]) {
  it(`${Client.name} aborts a pending UDP startup`, async t => {
    let closed = 0;
    const socket = Object.assign(new EventEmitter(), {
      connect() {}, close() { closed++; },
    });
    t.mock.method(dgram, 'createSocket', () => socket);
    const loop = new Client({ core: {} as any, transportFactory: connectNodeUdp }, () => {});
    const connecting = loop.connect('127.0.0.1:443', 'localhost');
    const rejected = assert.rejects(connecting, /abort/i);
    await loop.close();
    await rejected;
    assert.equal(closed, 1);
    assert.equal(socket.listenerCount('error'), 1, 'only post-connect error guard remains');
  });

  it(`${Client.name} closes the transport when core construction fails`, async () => {
    let closed = 0;
    let allocationsFreed = 0;
    const loop = new Client({
      core: {
        writeUtf8: () => ({ ptr: 8, len: 1 }),
        free() { allocationsFreed++; },
        exports: new Proxy({}, { get: () => () => { throw new Error('invalid core options'); } }),
      } as any,
      transportFactory: async () => ({
        localAddress: () => ({ address: '127.0.0.1', family: 'IPv4', port: 443 }),
        onMessage() {}, send() {}, async close() { closed++; },
      }),
    }, () => {});
    await assert.rejects(loop.connect('127.0.0.1:443', 'localhost'), /invalid core options/);
    await loop.close();
    assert.equal(closed, 1);
    assert.equal(allocationsFreed, 1);
  });
}

for (const Server of [WasmH3ServerEventLoop, WasmQuicServerEventLoop]) {
  it(`${Server.name} closes a transport that finishes binding after shutdown`, async () => {
    let bound!: (transport: any) => void;
    let closed = 0;
    const loop = new Server({ core: {} as any, key: new Uint8Array(), cert: new Uint8Array(),
      transportFactory: async () => new Promise(resolve => { bound = resolve; }) }, () => {});
    const listening = loop.listen(443, '127.0.0.1');
    const rejected = assert.rejects(listening, /closed during startup/);
    await loop.close();
    bound({ async close() { closed++; } });
    await rejected;
    assert.equal(closed, 1);
  });
}

for (const Loop of [WasmH3ClientEventLoop, WasmQuicClientEventLoop, WasmH3ServerEventLoop, WasmQuicServerEventLoop]) {
  for (const fail of [false, true]) {
    it(`${Loop.name} shares close completion and frees resources${fail ? ' after pump failure' : ''}`, async () => {
      let release!: () => void;
      const closed = new Promise<void>(resolve => { release = resolve; });
      let closes = 0;
      let freed = 0;
      let stopping = false;
      const core = {
        writeUtf8: () => ({ ptr: 8, len: 1 }), free() {}, allocOutPtrCell: () => 16,
        exports: new Proxy({}, {
          get(_target, key: string) {
            return () => {
              if (key.endsWith('_new')) return 1;
              if (key.endsWith('_close') || key.endsWith('_shutdown')) stopping = true;
              if (key.endsWith('_free')) freed++;
              if (key.endsWith('_is_done')) return 1;
              if (key.endsWith('_timeout_ms')) return -1;
              if (fail && stopping && key.endsWith('_next_send')) throw new Error('pump failed');
              return 0;
            };
          },
        }),
      };
      const transport = {
        localAddress: () => ({ address: '127.0.0.1', family: 'IPv4', port: 443 }),
        send() {}, onMessage() {},
        async close() { closes++; await closed; },
      };
      const loop = new Loop({ core: core as any, key: new Uint8Array(), cert: new Uint8Array(),
        transportFactory: async () => transport }, () => {});
      if ('connect' in loop) await loop.connect('127.0.0.1:443', 'localhost');
      else await loop.listen(443, '127.0.0.1');
      let completed = 0;
      const first = loop.close();
      const second = loop.close();
      const observed = Promise.allSettled([first, second]).then(results => { completed++; return results; });
      await new Promise(resolve => setImmediate(resolve));
      assert.equal(completed, 0, 'every close caller must await transport release');
      release();
      const results = await observed;
      assert.ok(results.every(result => result.status === (fail ? 'rejected' : 'fulfilled')));
      assert.equal(freed, 1);
      assert.equal(closes, 1);
    });
  }
}
