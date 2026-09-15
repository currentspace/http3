import { it, mock } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import dgram from 'node:dgram';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';
import { Writable } from 'node:stream';
import { DetachedTasks } from '../../lib/run-detached.js';
import { ServerSentEventStream } from '../../lib/sse.js';
import type { ServerHttp3Stream } from '../../lib/stream.js';
import { connectNodeUdp } from '../../lib/wasm/node-udp-adapter.js';

async function bounded<T>(promise: Promise<T>): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('operation did not settle')), 200);
    })]);
  } finally { clearTimeout(timer); }
}

it('detached error reporters cannot strand tasks or create unhandled rejections', () => {
  const path = resolve(__dirname, '../../lib/run-detached.js');
  const child = spawnSync(process.execPath, ['--unhandled-rejections=strict', '-e', `
    const { DetachedTasks, runDetached } = require(${JSON.stringify(path)});
    const assert = require('node:assert/strict');
    const fail = () => { throw new Error('reporter failed'); };
    const tasks = new DetachedTasks();
    runDetached(Promise.reject(new Error('operation failed')), fail);
    tasks.run(Promise.reject(new Error('operation failed')), fail);
    tasks.drain().then(() => assert.equal(tasks.size, 0));
  `], { encoding: 'utf8', timeout: 2000 });
  assert.equal(child.status, 0, child.stderr);
  assert.match(child.stderr, /reporter failed/);
});

it('drain includes tasks registered by settling tasks', async () => {
  const tasks = new DetachedTasks();
  let completed = false;
  tasks.run(Promise.resolve().then(() => {
    tasks.run(new Promise<void>(resolve => setImmediate(() => { completed = true; resolve(); })), assert.fail);
  }), assert.fail);
  await tasks.drain();
  assert.equal(completed, true);
  assert.equal(tasks.size, 0);
});

for (const event of ['close', 'error'] as const) {
  it(`SSE blocked send settles on ${event}`, async () => {
    const stream = Object.assign(new EventEmitter(), {
      destroyed: false, writableEnded: false,
      respond() {}, write() { return false; }, end() {},
    });
    const sse = new ServerSentEventStream(stream as unknown as ServerHttp3Stream);
    const sent = assert.rejects(bounded(sse.send('hello')), /closed|disconnected|aborted/);
    stream.emit(event, new Error('disconnected'));
    await sent;
    assert.equal(stream.listenerCount('drain'), 0);
  });
}

it('SSE heartbeats stay bounded while the client is backpressured', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  let releaseWrite: (() => void) | undefined;
  let writes = 0;
  const stream = Object.assign(new Writable({
    highWaterMark: 1,
    write(_chunk, _encoding, callback) {
      writes++;
      releaseWrite = callback;
    },
  }), { respond() {} });
  const sse = new ServerSentEventStream(stream as unknown as ServerHttp3Stream, {
    heartbeatIntervalMs: 10,
  });
  try {
    t.mock.timers.tick(10);
    const firstFrameBytes = stream.writableLength;
    assert.ok(firstFrameBytes > 0);
    t.mock.timers.tick(1000);
    assert.equal(stream.writableLength, firstFrameBytes, 'blocked heartbeats must not queue more frames');
    assert.equal(stream.listenerCount('drain'), 1);
    releaseWrite?.();
    await new Promise<void>(resolve => setImmediate(resolve));
    assert.equal(stream.writableLength, 0);
    t.mock.timers.tick(10);
    assert.equal(writes, 2, 'heartbeats resume once the client drains');
  } finally {
    sse.close();
    stream.destroy();
  }
  await Promise.resolve();
  assert.equal(stream.listenerCount('drain'), 0);
});

it('SSE heartbeats stop when the underlying writable finishes', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const stream = Object.assign(new EventEmitter(), {
    destroyed: false, writableEnded: false,
    respond() {}, write: t.mock.fn(() => true), end() {},
  });
  const sse = new ServerSentEventStream(stream as unknown as ServerHttp3Stream, { heartbeatIntervalMs: 10 });
  try {
    stream.writableEnded = true;
    stream.emit('finish');
    t.mock.timers.tick(100);
    sse.heartbeat(10);
    t.mock.timers.tick(100);
    assert.equal(stream.write.mock.callCount(), 0);
  } finally { sse.close(); }
});

it('WASM UDP startup rejects socket errors and closes the socket', async () => {
  let closed = 0;
  const socket = Object.assign(new EventEmitter(), {
    connect() { queueMicrotask(() => socket.emit('error', new Error('bind failed'))); },
    close(callback?: () => void) { closed++; callback?.(); },
  });
  const stub = mock.method(dgram, 'createSocket', () => socket);
  try {
    await assert.rejects(bounded(connectNodeUdp('127.0.0.1', 443)), /bind failed/);
    assert.equal(closed, 1);
  } finally { stub.mock.restore(); }
});
