/**
 * Regression: `stream.close()` (and `_destroy`) must invoke any queued
 * drain callbacks with an Error, not silently drop them. Otherwise a
 * `stream.write(chunk, cb)` whose `cb` was waiting on an EVENT_DRAIN that
 * never arrives (because the stream is now closed) hangs forever — and so
 * does any caller using `stream/promises.pipeline` or
 * `await once(stream, 'drain')` semantics.
 *
 * Audit finding #13.
 *
 * The drain-callback path isn't naturally reachable in production today
 * because `WorkerEventLoop.streamSend` always reports full acceptance
 * (Step 3.1 will fix that). To exercise the close-vs-pending-write logic
 * independently of that fix, we manually populate `_bp.drainCallbacks`
 * with the same closure shape that `_writeChunk` would produce.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { ServerHttp3Stream, ClientHttp3Stream } from '../../lib/stream.js';
import { QuicStream } from '../../lib/quic-stream.js';
import { ensureBackpressureState } from '../../lib/stream-backpressure.js';

interface ServerLoopStub {
  streamSend: (connHandle: number, streamId: number, data: Buffer, fin: boolean) => number;
  streamClose: (connHandle: number, streamId: number, errorCode: number) => boolean;
  sendResponseHeaders: () => boolean;
  sendResponse: () => boolean;
  sendTrailers: () => boolean;
  sendDatagram: () => boolean;
  getSessionMetrics: () => Record<string, number>;
  pingSession: () => boolean;
  getRemoteSettings: () => Array<{ id: number; value: number }>;
  getQlogPath: () => string | null;
}

function serverLoopStub(): ServerLoopStub {
  return {
    streamSend: (_c, _s, data, fin) => Math.max(data.length, fin ? 1 : 0),
    streamClose: () => true,
    sendResponseHeaders: () => true,
    sendResponse: () => true,
    sendTrailers: () => true,
    sendDatagram: () => true,
    getSessionMetrics: () => ({}),
    pingSession: () => true,
    getRemoteSettings: () => [],
    getQlogPath: () => null,
  };
}

function clientLoopStub(): { streamSend: (s: number, d: Buffer, f: boolean) => number; streamClose: () => boolean } {
  return {
    streamSend: (_s, data, fin) => Math.max(data.length, fin ? 1 : 0),
    streamClose: () => true,
  };
}

describe('stream.close() with pending drain callbacks', () => {
  it('server stream: pending write callbacks fire with error', () => {
    const stream = new ServerHttp3Stream();
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = serverLoopStub();
    stream._connHandle = 0;
    stream._streamId = 0;

    let cbErr: unknown = null;
    let cbCalled = 0;

    stream._bp = ensureBackpressureState(stream._bp);
    stream._bp.drainCallbacks.push((err) => {
      cbCalled += 1;
      if (err) cbErr = err;
    });

    stream.close();

    assert.equal(cbCalled, 1, 'pending callback must fire exactly once on close');
    assert.ok(cbErr instanceof Error, 'close should reject pending writes with an Error');
  });

  it('client stream: pending write callbacks fire with error', () => {
    const stream = new ClientHttp3Stream();
    const loop = clientLoopStub();
    (stream as unknown as { _eventLoop: typeof loop })._eventLoop = loop;
    stream._streamId = 0;

    let cbErr: unknown = null;
    let cbCalled = 0;

    stream._bp = ensureBackpressureState(stream._bp);
    stream._bp.drainCallbacks.push((err) => {
      cbCalled += 1;
      if (err) cbErr = err;
    });

    stream.close();

    assert.equal(cbCalled, 1);
    assert.ok(cbErr instanceof Error);
  });

  it('quic stream: pending write callbacks fire with error', () => {
    const stream = new QuicStream();
    const loop = clientLoopStub();
    (stream as unknown as { _clientLoop: typeof loop })._clientLoop = loop;
    stream._streamId = 0;

    let cbErr: unknown = null;
    let cbCalled = 0;

    stream._bp = ensureBackpressureState(stream._bp);
    stream._bp.drainCallbacks.push((err) => {
      cbCalled += 1;
      if (err) cbErr = err;
    });

    stream.close();

    assert.equal(cbCalled, 1);
    assert.ok(cbErr instanceof Error);
  });

  it('server stream: _destroy also fires pending callbacks with error', () => {
    const stream = new ServerHttp3Stream();
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = serverLoopStub();
    stream._connHandle = 0;
    stream._streamId = 0;

    let cbErr: unknown = null;
    let cbCalled = 0;

    stream._bp = ensureBackpressureState(stream._bp);
    stream._bp.drainCallbacks.push((err) => {
      cbCalled += 1;
      if (err) cbErr = err;
    });

    // Catch the error event so Node doesn't escalate to uncaughtException.
    stream.on('error', () => { /* swallow; we're testing the drain reject */ });
    stream.destroy(new Error('forced'));

    assert.equal(cbCalled, 1);
    assert.ok(cbErr instanceof Error);
    assert.equal((cbErr as Error).message, 'forced');
  });
});
