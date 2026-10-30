import { test } from 'node:test';
import assert from 'node:assert/strict';
import { setImmediate as nextTurn } from 'node:timers/promises';
import { ServerHttp3Stream, ClientHttp3Stream } from '../../lib/stream.js';
import type { ClientEventLoopLike, ServerEventLoopLike } from '../../lib/event-loop.js';

for (const Stream of [ServerHttp3Stream, ClientHttp3Stream]) {
  function fixture(rejectedAttempts = 0) {
    const stream = new Stream();
    const credits: number[] = [];
    stream._eventLoop = (stream instanceof ServerHttp3Stream ? {
      streamReadCredit: (_conn: number, _id: number, bytes: number) => { credits.push(bytes); return credits.length > rejectedAttempts; },
      streamClose() {},
    } : {
      streamReadCredit: (_id: number, bytes: number) => { credits.push(bytes); return credits.length > rejectedAttempts; },
      streamClose() { return true; },
    }) as unknown as ServerEventLoopLike & ClientEventLoopLike;
    return { stream, credits };
  }

  test(`${Stream.name}: receive credit coalesces a burst without losing bytes`, async () => {
    const { stream, credits } = fixture();
    stream.resume();
    for (let i = 0; i < 16; i++) stream._pushData(Buffer.alloc(1024));
    assert.deepEqual(credits, []);
    await nextTurn();
    assert.deepEqual(credits, [16 * 1024]);
    stream._pushData(Buffer.alloc(3));
    await nextTurn();
    assert.deepEqual(credits, [16 * 1024, 3]);
    stream.destroy();
  });

  test(`${Stream.name}: deferred credit respects intervening backpressure`, async () => {
    const { stream, credits } = fixture();
    stream._pushData(Buffer.alloc(1));
    stream._pushData(Buffer.alloc(stream.readableHighWaterMark));
    await nextTurn();
    assert.deepEqual(credits, []);
    stream.resume();
    await nextTurn(); await nextTurn();
    assert.equal(credits.reduce((sum, bytes) => sum + bytes, 0), 1 + stream.readableHighWaterMark);
    stream.destroy();
  });

  test(`${Stream.name}: destruction cancels a queued credit grant`, async () => {
    const { stream, credits } = fixture();
    stream._pushData(Buffer.alloc(10));
    stream.destroy();
    await nextTurn();
    assert.deepEqual(credits, []);
  });

  test(`${Stream.name}: retry includes bytes consumed while the command queue is full`, async () => {
    const { stream, credits } = fixture(1);
    stream._pushData(Buffer.alloc(10));
    await nextTurn();
    assert.deepEqual(credits, [10]);
    stream._pushData(Buffer.alloc(20));
    await nextTurn(); await nextTurn();
    assert.deepEqual(credits, [10, 30]);
    stream.destroy();
  });
}
