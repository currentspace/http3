import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { setImmediate as nextTurn } from 'node:timers/promises';
import { ClientHttp3Stream } from '../../lib/stream.js';
import type { ClientEventLoopLike } from '../../lib/event-loop.js';

function client() {
  const closes: Array<[number, number]> = [];
  const stream = new ClientHttp3Stream();
  stream._streamId = 4;
  const loop = {
    streamSend: (_id: number, data: Buffer, fin: boolean) => Math.max(data.length, fin ? 1 : 0),
    streamClose: (id: number, code: number) => { closes.push([id, code]); return true; },
    streamReadCredit: (_id: number, _bytes: number) => true,
  };
  stream._eventLoop = loop as unknown as ClientEventLoopLike;
  return { stream, loop, closes };
}

test('successful client duplex completion does not reset the peer', async () => {
  const { stream, closes } = client();
  const closed = once(stream, 'close');
  stream.resume();
  stream.end();
  stream._pushData(Buffer.from('done'));
  stream._pushData(null);
  await closed;
  assert.equal(stream.readableEnded, true);
  assert.equal(stream.writableFinished, true);
  assert.deepEqual(closes, []);
});

test('explicit close preserves its code and sends only one reset', async () => {
  const { stream, closes } = client();
  const closed = once(stream, 'close');
  stream.close(0x010b);
  stream.close();
  stream.destroy();
  await closed;
  assert.deepEqual(closes, [[4, 0x010b]]);
});

test('destroy cancels an unread response even after response FIN', async () => {
  const { stream, closes } = client();
  stream.end();
  await once(stream, 'finish');
  stream._pushData(Buffer.from('unread'));
  stream._pushData(null);
  const closed = once(stream, 'close');
  stream.destroy();
  await closed;
  assert.deepEqual(closes, [[4, 0x010c]]);
});

test('receive credit retries queue saturation without losing bytes', async () => {
  const { stream, loop } = client();
  const attempts: number[] = [];
  loop.streamReadCredit = (_id, bytes) => { attempts.push(bytes); return attempts.length >= 3; };
  stream._pushData(Buffer.alloc(10));
  stream._pushData(Buffer.alloc(20));
  await nextTurn();
  await nextTurn();
  assert.deepEqual(attempts, [10, 30, 30]);
  stream.destroy();
});

test('destroy stops retrying receive credit on a saturated queue', async () => {
  const { stream, loop } = client();
  let attempts = 0;
  loop.streamReadCredit = () => { attempts++; return false; };
  stream._pushData(Buffer.alloc(10));
  stream.destroy();
  await nextTurn();
  assert.equal(attempts, 1);
});

test('destroy settles a blocked client final write exactly once', async () => {
  const { stream, closes } = client();
  stream._blocked = true;
  let calls = 0;
  let failure: Error | null | undefined;
  stream.on('error', () => {});
  stream.end('pending', (error?: Error | null) => { calls++; failure = error; });
  stream.destroy(new Error('cancel upload'));
  await nextTurn();
  stream._onNativeDrain();
  await nextTurn();
  assert.equal(calls, 1);
  assert.equal(failure?.message, 'cancel upload');
  assert.deepEqual(closes, [[4, 0x010c]]);
});
