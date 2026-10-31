import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { setImmediate as nextTurn } from 'node:timers/promises';
import { QuicStream } from '../../lib/quic-stream.js';
import { QuicClientSession } from '../../lib/quic-client.js';
import { QuicServerSession } from '../../lib/quic-server.js';

for (const side of ['client', 'server'] as const) {
  for (const bounded of [false, true]) {
    function fixture() {
      const closes: number[] = [];
      const loop = {
        ...(bounded ? { maxStreamSendBytes: 16 * 1024 } : {}),
        streamClose: (...args: number[]) => { closes.push(args.at(-1)!); return true; },
        streamSend: (...args: unknown[]) => Math.max(1, (args.at(-2) as Buffer).length),
        async close() {},
      };
      const stream = new QuicStream();
      if (side === 'client') stream._clientLoop = loop as unknown as NonNullable<typeof stream._clientLoop>;
      else stream._serverLoop = loop as unknown as NonNullable<typeof stream._serverLoop>;
      return { stream, loop, closes };
    }

    test(`QUIC ${side} bounded=${bounded}: explicit close preserves its code through destroy`, async () => {
      const { stream, closes } = fixture();
      const closed = once(stream, 'close');
      stream.close(42); stream.close(43); stream.destroy();
      await closed;
      assert.deepEqual(closes, [42]);
    });

    test(`QUIC ${side} bounded=${bounded}: automatic completion preserves queued bytes and FIN`, async () => {
      const { stream, closes } = fixture();
      const closed = once(stream, 'close');
      stream.resume(); stream._pushData(null); stream.end(Buffer.alloc(128 * 1024));
      await closed;
      assert.equal(stream.readableEnded, true);
      assert.equal(stream.writableFinished, true);
      assert.deepEqual(closes, []);
    });

    test(`QUIC ${side} bounded=${bounded}: session teardown destroys streams without transport resets`, async () => {
      const { loop, closes } = fixture();
      let stream: QuicStream;
      if (side === 'server') {
        const session = new QuicServerSession(1, '127.0.0.1', 1234, loop as unknown as ConstructorParameters<typeof QuicServerSession>[3]);
        stream = session.openStream();
        stream._onNativeBlocked();
        stream.end(Buffer.alloc(128 * 1024));
        session._cleanup();
      } else {
        const session = new QuicClientSession();
        session._setEventLoop(loop as unknown as Parameters<typeof session._setEventLoop>[0]);
        session.once('stream', value => { stream = value; });
        session._dispatchEvents([{ eventType: 2, connHandle: 0, streamId: 1 }]);
        stream!._onNativeBlocked();
        stream!.end(Buffer.alloc(128 * 1024));
        session._dispatchEvents([{ eventType: 7, connHandle: 0, streamId: 0 }]);
      }
      await nextTurn();
      assert.equal(stream!.destroyed, true);
      assert.deepEqual(closes, []);
    });
  }
}
