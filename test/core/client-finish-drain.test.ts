/**
 * Regression: H3 client `_onFinished` (response FIN) must not delete the
 * stream from `_streams`, because the writable side may still have pending
 * drain callbacks that need to fire when the peer eventually grants flow
 * control. Audit finding #2.
 *
 * The server side (`Http3SecureServer._onFinished` in lib/server.ts) gets
 * this right: it defers the map removal until 'close'. The client side
 * historically did `this._streams.delete(...)` inline, which made any
 * subsequent EVENT_DRAIN no-op, hanging Node's writable callbacks.
 *
 * This is a unit-level test that drives the dispatcher directly with a
 * stub event loop, rather than spinning up a real worker.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { Http3ClientSession } from '../../lib/client.js';

// Mirror the constants from lib/client.ts (intentional duplication so a
// rename on either side trips this test).
const EVENT_FINISHED = 5;
const EVENT_DRAIN = 8;

interface ClientLoopStub {
  sendRequest: (h: Array<{ name: string; value: string }>, fin: boolean) => number;
  streamSend: (streamId: number, data: Buffer, fin: boolean) => number;
  streamClose: (streamId: number, errorCode: number) => boolean;
  sendDatagram: (data: Buffer) => boolean;
  getSessionMetrics: () => Record<string, number>;
  getRemoteSettings: () => Array<{ id: number; value: number }>;
  ping: () => boolean;
  getQlogPath: () => string | null;
}

function makeStubLoop(): ClientLoopStub {
  return {
    sendRequest: () => 0,
    streamSend: (_streamId, data, fin) => Math.max(data.length, fin ? 1 : 0),
    streamClose: () => true,
    sendDatagram: () => true,
    getSessionMetrics: () => ({
      packetsIn: 0, packetsOut: 0, bytesIn: 0, bytesOut: 0,
      handshakeTimeMs: 0, rttMs: 0, cwnd: 0, datagramQueueDepth: 0,
    }),
    getRemoteSettings: () => [],
    ping: () => true,
    getQlogPath: () => null,
  };
}

describe('client EVENT_FINISHED', () => {
  it('does not remove stream from internal map (drain after FIN still routes)', () => {
    const session = new Http3ClientSession('example.test:443');
    const sessionAny = session as unknown as {
      _eventLoop: ClientLoopStub | null;
      _handshakeComplete: boolean;
      _dispatchEvents: (events: Array<Record<string, unknown>>) => void;
      _streams: Map<number, unknown>;
    };
    sessionAny._eventLoop = makeStubLoop();
    sessionAny._handshakeComplete = true;

    const stream = session.request({ ':method': 'GET', ':path': '/' });
    const streamId = (stream as unknown as { _streamId: number })._streamId;

    let drainFired = 0;
    (stream as unknown as { _onNativeDrain: () => void })._onNativeDrain = () => {
      drainFired += 1;
    };

    sessionAny._dispatchEvents([{ eventType: EVENT_FINISHED, streamId }]);
    sessionAny._dispatchEvents([{ eventType: EVENT_DRAIN, streamId }]);

    assert.equal(drainFired, 1, 'EVENT_DRAIN after EVENT_FINISHED must reach the stream');
    assert.ok(sessionAny._streams.has(streamId), 'stream must remain in map until close');

    stream.destroy();
  });
});
