import { describe, it } from 'node:test';
import assert from 'node:assert';
import { Http3ClientSession, ERR_HTTP3_STREAM_BLOCKED, Http3Error } from '../../lib/index.js';

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

function makeStubLoop(sendRequest: ClientLoopStub['sendRequest']): ClientLoopStub {
  return {
    sendRequest,
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

function makeReadySession(loop: ClientLoopStub): Http3ClientSession {
  const session = new Http3ClientSession('example.test:443');
  const sessionAny = session as unknown as {
    _eventLoop: ClientLoopStub | null;
    _handshakeComplete: boolean;
  };
  sessionAny._eventLoop = loop;
  sessionAny._handshakeComplete = true;
  return session;
}

describe('client request backpressure', () => {
  it('wraps native StreamBlocked request creation errors with a stable code', () => {
    const session = makeReadySession(makeStubLoop(() => {
      throw new Error('send_request failed: HTTP/3 error: StreamBlocked InvalidArg');
    }));

    assert.throws(
      () => session.request({ ':method': 'GET', ':path': '/' }),
      (err: unknown) => err instanceof Http3Error && err.code === ERR_HTTP3_STREAM_BLOCKED,
    );
  });

  it('requestAsync retries when an unknown-stream drain signals request capacity', async () => {
    let attempts = 0;
    const session = makeReadySession(makeStubLoop(() => {
      attempts += 1;
      if (attempts === 1) {
        throw new Error('send_request failed: HTTP/3 error: StreamBlocked InvalidArg');
      }
      return 4;
    }));
    const sessionAny = session as unknown as {
      _dispatchEvents: (events: Array<Record<string, unknown>>) => void;
    };

    const pending = session.requestAsync({ ':method': 'GET', ':path': '/' }, { timeoutMs: 1000 });
    await Promise.resolve();
    sessionAny._dispatchEvents([{ eventType: EVENT_DRAIN, streamId: 4 }]);

    const stream = await pending;
    assert.equal(stream.id, 4);
    assert.equal(attempts, 2);
    stream.destroy();
  });

  it('requestAsync times out while waiting for request stream capacity', async () => {
    const session = makeReadySession(makeStubLoop(() => {
      throw new Error('send_request failed: HTTP/3 error: StreamBlocked InvalidArg');
    }));

    await assert.rejects(
      session.requestAsync({ ':method': 'GET', ':path': '/' }, { timeoutMs: 1 }),
      (err: unknown) => err instanceof Http3Error && err.code === ERR_HTTP3_STREAM_BLOCKED,
    );
  });
});
