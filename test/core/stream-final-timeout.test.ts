/**
 * Audit finding #9. `_final` must not wait forever for a drain that
 * never arrives. With the current `streamSend` contract (always returns
 * full acceptance), the wait branch is dead code, but the timeout
 * watchdog is a safety net for a future contract redesign and for
 * pathological worker states (wedged, crashed mid-send, etc.).
 *
 * Use a tiny private `STREAM_FINISH_TIMEOUT_MS` override via an exposed
 * test path: we drive `_final` with a stub event loop that returns 0
 * for the FIN-only send, then never fire the drain callback. The
 * `_final` callback must eventually be invoked with an Error rather
 * than hang.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { ServerHttp3Stream } from '../../lib/stream.js';
import { QuicStream } from '../../lib/quic-stream.js';

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

function blockingServerLoop(): ServerLoopStub {
  return {
    // Returns 0 for FIN-only sends to force `_final` into the wait
    // branch where the timeout watchdog applies.
    streamSend: (_c, _s, data, fin) => (fin && data.length === 0 ? 0 : data.length),
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

describe('_final timeout watchdog', () => {
  it('server stream _final fires callback on timeout when drain never arrives', async () => {
    // Override the timeout to something fast for the test.
    // The stream module captured the constant at import time; we test
    // the *behavior* by inducing the timeout path with the production
    // 30 s constant via a custom faster runner. Easiest path: invoke
    // _final directly and stub setTimeout/clearTimeout temporarily.
    const stream = new ServerHttp3Stream();
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = blockingServerLoop();
    stream._connHandle = 0;
    stream._streamId = 0;

    const realSetTimeout = global.setTimeout;
    type Callback = () => void;
    let captured: Callback | undefined;
    (global as { setTimeout: typeof setTimeout }).setTimeout = ((fn: Callback) => {
      captured = fn;
      return realSetTimeout(() => undefined, 0) as unknown as ReturnType<typeof setTimeout>;
    }) as unknown as typeof setTimeout;

    try {
      const callbackResult = await new Promise<Error | null | undefined>((resolve) => {
        (stream as unknown as { _final: (cb: (err?: Error | null) => void) => void })._final(
          (err) => resolve(err),
        );
        // Fire the captured timeout fn to simulate the watchdog firing.
        if (captured) captured();
      });
      assert.ok(callbackResult instanceof Error, 'expected timeout error');
      assert.match(callbackResult.message, /finish timed out/i);
    } finally {
      (global as { setTimeout: typeof setTimeout }).setTimeout = realSetTimeout;
    }
    stream.destroy();
  });

  it('server stream _final does not fire timeout when drain arrives first', async () => {
    const stream = new ServerHttp3Stream();
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = blockingServerLoop();
    stream._connHandle = 0;
    stream._streamId = 0;

    const callbackResult = await new Promise<Error | null | undefined>((resolve) => {
      (stream as unknown as { _final: (cb: (err?: Error | null) => void) => void })._final(
        (err) => resolve(err),
      );
      // Fire the queued drain callback synchronously (worker accepted the FIN).
      const bp = (stream as unknown as { _bp: { drainCallbacks: Array<(err?: Error) => void> } })._bp;
      bp.drainCallbacks.shift()?.();
    });
    assert.equal(callbackResult, undefined, 'no error expected when drain wins');
    stream.destroy();
  });

  it('quic stream _final timeout wraps _writeFinalChunk', async () => {
    const stream = new QuicStream();
    interface ClientLoop { streamSend: (s: number, d: Buffer, f: boolean) => number; streamClose: () => boolean }
    const loop: ClientLoop = {
      streamSend: (_s, data, fin) => (fin && data.length === 0 ? 0 : data.length),
      streamClose: () => true,
    };
    (stream as unknown as { _clientLoop: ClientLoop })._clientLoop = loop;
    stream._streamId = 0;

    const realSetTimeout = global.setTimeout;
    type Callback = () => void;
    let captured: Callback | undefined;
    (global as { setTimeout: typeof setTimeout }).setTimeout = ((fn: Callback) => {
      captured = fn;
      return realSetTimeout(() => undefined, 0) as unknown as ReturnType<typeof setTimeout>;
    }) as unknown as typeof setTimeout;

    try {
      const callbackResult = await new Promise<Error | null | undefined>((resolve) => {
        (stream as unknown as { _final: (cb: (err?: Error | null) => void) => void })._final(
          (err) => resolve(err),
        );
        if (captured) captured();
      });
      assert.ok(callbackResult instanceof Error, 'expected timeout error');
      assert.match(callbackResult.message, /finish timed out/i);
    } finally {
      (global as { setTimeout: typeof setTimeout }).setTimeout = realSetTimeout;
    }
    stream.destroy();
  });
});
