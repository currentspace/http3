import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  completeNativeWrite,
  createNativeWriteWindow,
  rejectNativeWriteWindow,
} from '../../lib/stream-backpressure.js';
import { QuicStream } from '../../lib/quic-stream.js';

function nextImmediate(): Promise<void> {
  return new Promise((resolve) => {
    setImmediate(resolve);
  });
}

describe('native write admission window', () => {
  it('completes one highWaterMark worth of writes synchronously', async () => {
    const window = createNativeWriteWindow(8);
    const calls: string[] = [];

    completeNativeWrite(window, 4, () => { calls.push('a'); });
    completeNativeWrite(window, 4, () => { calls.push('b'); });
    completeNativeWrite(window, 1, () => { calls.push('c'); });

    assert.deepEqual(calls, ['a', 'b']);
    await nextImmediate();
    assert.deepEqual(calls, ['a', 'b', 'c']);
  });

  it('treats FIN-only writes as one byte of local window pressure', async () => {
    const window = createNativeWriteWindow(1);
    const calls: string[] = [];

    completeNativeWrite(window, 0, () => { calls.push('fin-a'); });
    completeNativeWrite(window, 0, () => { calls.push('fin-b'); });

    assert.deepEqual(calls, ['fin-a']);
    await nextImmediate();
    assert.deepEqual(calls, ['fin-a', 'fin-b']);
  });

  it('rejects callbacks delayed by the local window', () => {
    const window = createNativeWriteWindow(4);
    let observed: Error | null | undefined;

    completeNativeWrite(window, 8, (err) => {
      observed = err;
    });

    const err = new Error('stream destroyed');
    rejectNativeWriteWindow(window, err);
    assert.equal(observed, err);
  });

  it('keeps a fast Writable producer bounded by highWaterMark', async () => {
    const stream = new QuicStream({ highWaterMark: 8 });
    let nativeSends = 0;

    (stream as unknown as {
      _clientLoop: {
        streamSend: (streamId: number, data: Buffer, fin: boolean) => number;
        streamClose: () => boolean;
      };
    })._clientLoop = {
      streamSend: (_streamId, data, fin) => {
        nativeSends += 1;
        return Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._streamId = 0;

    const returns = Array.from({ length: 5 }, () => stream.write(Buffer.alloc(4)));

    assert.deepEqual(returns, [true, true, true, false, false]);
    assert.equal(nativeSends, 3, 'only one local window should enter native synchronously');
    assert.equal(stream.writableNeedDrain, true);

    await nextImmediate();

    assert.equal(nativeSends, 5);
    assert.equal(stream.writableNeedDrain, false);
    stream.destroy();
  });

  it('write-ready does not clear QUIC flow-control blocking', () => {
    const stream = new QuicStream({ highWaterMark: 8 });
    let nativeSends = 0;
    let completed = false;

    (stream as unknown as {
      _clientLoop: {
        streamSend: (streamId: number, data: Buffer, fin: boolean) => number;
        streamClose: () => boolean;
      };
    })._clientLoop = {
      streamSend: (_streamId, data, fin) => {
        nativeSends += 1;
        return Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._streamId = 0;
    stream._blocked = true;

    stream.write(Buffer.alloc(4), (err) => {
      assert.ifError(err);
      completed = true;
    });

    stream._onNativeWriteReady();
    assert.equal(nativeSends, 0);
    assert.equal(completed, false);

    stream._onNativeDrain();
    assert.equal(nativeSends, 1);
    assert.equal(completed, true);
    stream.destroy();
  });
});
