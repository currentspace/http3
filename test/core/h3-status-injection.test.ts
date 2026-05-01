/**
 * Audit finding #17. The H3 server's `respond()` / `respondWithBody()`
 * must auto-inject `:status: 200` when the caller omits it. Mirrors the
 * H2 adapter behaviour (`toHttp2OutgoingHeaders` in lib/stream.ts already
 * defaults to 200 for the H2 path).
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { ServerHttp3Stream } from '../../lib/stream.js';

interface RecordedHeaders {
  call: 'sendResponseHeaders' | 'sendResponse';
  headers: Array<{ name: string; value: string }>;
}

function recordingLoop(): {
  loop: Record<string, unknown>;
  recorded: RecordedHeaders[];
} {
  const recorded: RecordedHeaders[] = [];
  const loop: Record<string, unknown> = {
    sendResponseHeaders: (_c: number, _s: number, headers: RecordedHeaders['headers']): void => {
      recorded.push({ call: 'sendResponseHeaders', headers });
    },
    sendResponse: (
      _c: number,
      _s: number,
      headers: RecordedHeaders['headers'],
      _data: Buffer,
    ): void => {
      recorded.push({ call: 'sendResponse', headers });
    },
    streamSend: (): number => 1,
    streamClose: (): boolean => true,
    sendTrailers: (): boolean => true,
    sendDatagram: (): boolean => true,
  };
  return { loop, recorded };
}

describe('H3 :status auto-injection', () => {
  it('respond() prepends :status: 200 when omitted', () => {
    const stream = new ServerHttp3Stream();
    const { loop, recorded } = recordingLoop();
    (stream as unknown as { _eventLoop: typeof loop })._eventLoop = loop;
    stream._connHandle = 0;
    stream._streamId = 0;

    stream.respond({ 'content-type': 'text/plain' });

    assert.equal(recorded.length, 1);
    assert.equal(recorded[0].headers[0].name, ':status');
    assert.equal(recorded[0].headers[0].value, '200');

    stream.destroy();
  });

  it('respond() preserves user-supplied :status', () => {
    const stream = new ServerHttp3Stream();
    const { loop, recorded } = recordingLoop();
    (stream as unknown as { _eventLoop: typeof loop })._eventLoop = loop;
    stream._connHandle = 0;
    stream._streamId = 0;

    stream.respond({ ':status': '418', 'content-type': 'text/plain' });

    assert.equal(recorded.length, 1);
    const statuses = recorded[0].headers.filter((h) => h.name === ':status');
    assert.equal(statuses.length, 1, 'only one :status header expected');
    assert.equal(statuses[0].value, '418');

    stream.destroy();
  });

  it('respondWithBody() prepends :status: 200 when omitted', () => {
    const stream = new ServerHttp3Stream();
    const { loop, recorded } = recordingLoop();
    (stream as unknown as { _eventLoop: typeof loop })._eventLoop = loop;
    stream._connHandle = 0;
    stream._streamId = 0;

    stream.respondWithBody({ 'content-type': 'text/plain' }, 'hello');

    assert.equal(recorded.length, 1);
    assert.equal(recorded[0].headers[0].name, ':status');
    assert.equal(recorded[0].headers[0].value, '200');

    stream.destroy();
  });
});
