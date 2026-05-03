import { describe, it } from 'node:test';
import assert from 'node:assert';
import { ClientHttp3Stream, ServerHttp3Stream } from '../../lib/stream.js';
import { QuicStream } from '../../lib/quic-stream.js';

interface ServerLoopStub {
  streamSend: (connHandle: number, streamId: number, data: Buffer, fin: boolean) => number;
  streamClose: (connHandle: number, streamId: number, errorCode: number) => boolean;
}

function nextImmediate(): Promise<void> {
  return new Promise((resolve) => {
    setImmediate(resolve);
  });
}

function finish(stream: ServerHttp3Stream | ClientHttp3Stream, chunk: Buffer): Promise<void> {
  return new Promise((resolve, reject) => {
    stream.once('error', reject);
    stream.end(chunk, () => {
      stream.off('error', reject);
      resolve();
    });
  });
}

describe('_final native backpressure', () => {
  it('server stream sends final body chunk and FIN in one native call', async () => {
    const stream = new ServerHttp3Stream();
    const calls: Array<{ data: Buffer; fin: boolean }> = [];
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = {
      streamSend: (_c, _s, data, fin) => {
        calls.push({ data: Buffer.from(data), fin });
        return Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._connHandle = 0;
    stream._streamId = 0;

    const body = Buffer.from('final-response-body');
    await finish(stream, body);

    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0], { data: body, fin: true });
    stream.destroy();
  });

  it('client stream sends final body chunk and FIN in one native call', async () => {
    const stream = new ClientHttp3Stream();
    const calls: Array<{ data: Buffer; fin: boolean }> = [];
    interface ClientLoopStub {
      streamSend: (streamId: number, data: Buffer, fin: boolean) => number;
      streamClose: (streamId: number, errorCode: number) => boolean;
    }
    (stream as unknown as { _eventLoop: ClientLoopStub })._eventLoop = {
      streamSend: (_s, data, fin) => {
        calls.push({ data: Buffer.from(data), fin });
        return Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._streamId = 0;

    const body = Buffer.from('final-request-body');
    await finish(stream, body);

    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0], { data: body, fin: true });
    stream.destroy();
  });

  it('server stream waits for native write-ready and retries FIN', async () => {
    const stream = new ServerHttp3Stream();
    let attempts = 0;
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = {
      streamSend: (_c, _s, data, fin) => {
        attempts += 1;
        return attempts === 1 && fin && data.length === 0 ? 0 : Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._connHandle = 0;
    stream._streamId = 0;

    let settled = false;
    const done = new Promise<Error | null | undefined>((resolve) => {
      stream._final((err) => {
        settled = true;
        resolve(err);
      });
    });

    await nextImmediate();
    assert.equal(settled, false);
    stream._onNativeWriteReady();
    assert.equal(await done, undefined);
    assert.equal(attempts, 2);
    stream.destroy();
  });

  it('server stream write-ready does not bypass QUIC flow-control blocking', async () => {
    const stream = new ServerHttp3Stream();
    let attempts = 0;
    (stream as unknown as { _eventLoop: ServerLoopStub })._eventLoop = {
      streamSend: (_c, _s, data, fin) => {
        attempts += 1;
        return Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    stream._connHandle = 0;
    stream._streamId = 0;
    stream._blocked = true;

    let settled = false;
    const done = new Promise<Error | null | undefined>((resolve) => {
      stream._final((err) => {
        settled = true;
        resolve(err);
      });
    });

    stream._onNativeWriteReady();
    await nextImmediate();
    assert.equal(attempts, 0);
    assert.equal(settled, false);

    stream._onNativeDrain();
    assert.equal(await done, undefined);
    assert.equal(attempts, 1);
    stream.destroy();
  });

  it('quic stream waits for native write-ready and retries FIN', async () => {
    const stream = new QuicStream();
    interface ClientLoop {
      streamSend: (s: number, d: Buffer, f: boolean) => number;
      streamClose: () => boolean;
    }
    let attempts = 0;
    const loop: ClientLoop = {
      streamSend: (_s, data, fin) => {
        attempts += 1;
        return attempts === 1 && fin && data.length === 0 ? 0 : Math.max(data.length, fin ? 1 : 0);
      },
      streamClose: () => true,
    };
    (stream as unknown as { _clientLoop: ClientLoop })._clientLoop = loop;
    stream._streamId = 0;

    let settled = false;
    const done = new Promise<Error | null | undefined>((resolve) => {
      stream._final((err) => {
        settled = true;
        resolve(err);
      });
    });

    await nextImmediate();
    assert.equal(settled, false);
    stream._onNativeWriteReady();
    assert.equal(await done, undefined);
    assert.equal(attempts, 2);
    stream.destroy();
  });
});
