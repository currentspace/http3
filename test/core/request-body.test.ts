import { it } from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { once } from 'node:events';
import { createRequestBody, RequestBodyTooLargeError } from '../../lib/request-body.js';
import { ServerHttp3Stream } from '../../lib/stream.js';
import { createFetchHandler } from '../../lib/fetch-adapter.js';

it('request body pauses a fast source and streams all bytes when consumed', async () => {
  let produced = 0;
  const source = new Readable({
    highWaterMark: 16 * 1024,
    read() {
      if (produced === 4 * 1024 * 1024) { this.push(null); return; }
      produced += 16 * 1024;
      this.push(Buffer.alloc(16 * 1024, 7));
    },
  });
  const { body, dispose } = createRequestBody(source, new AbortController().signal, 4 * 1024 * 1024);
  try {
    await new Promise(resolve => setImmediate(resolve));
    assert.ok(produced <= 80 * 1024, `buffered ${produced} bytes without a consumer`);
    assert.equal(source.isPaused(), true);
    const result = new Uint8Array(await new Response(body).arrayBuffer());
    assert.equal(result.length, 4 * 1024 * 1024);
    assert.ok(result.every(byte => byte === 7));
    assert.equal(source.listenerCount('data'), 0);
  } finally { dispose(); source.destroy(); }
});

it('request body rejects oversized input without content-length', async () => {
  const source = Readable.from([Buffer.alloc(8), Buffer.alloc(8)]);
  const { body } = createRequestBody(source, new AbortController().signal, 10);
  await assert.rejects(new Response(body).arrayBuffer(), RequestBodyTooLargeError);
  assert.equal(source.listenerCount('data'), 0);
  source.destroy();
});

for (const action of ['abort', 'cancel'] as const) {
  it(`request body ${action} releases listeners and pauses upstream`, async () => {
    const source = new Readable({ read() {} });
    const controller = new AbortController();
    const { body } = createRequestBody(source, controller.signal, 100);
    const reader = body.getReader();
    const pending = reader.read();
    if (action === 'abort') {
      controller.abort(new Error('disconnected'));
      await assert.rejects(pending, /disconnected/);
    } else {
      await reader.cancel();
      assert.equal((await pending).done, true);
    }
    assert.equal(source.listenerCount('data'), 0);
    assert.equal(source.listenerCount('close'), 0);
    assert.equal(source.isPaused(), true);
    reader.releaseLock();
    source.destroy();
  });
}

it('native receive spill is bounded and resets only the overflowing stream', async () => {
  const stream = new ServerHttp3Stream();
  stream._maxBufferedReadBytes = 1024 * 1024;
  let resets = 0;
  stream._eventLoop = { streamClose: () => { resets++; return true; } } as any;
  const error = once(stream, 'error');
  for (let i = 0; i < 100; i++) stream._pushData(Buffer.alloc(64 * 1024));
  assert.equal(stream.destroyed, true);
  assert.equal((await error)[0].code, 'ERR_HTTP3_RECEIVE_BUFFER_LIMIT');
  assert.equal(resets, 1);
  assert.equal(stream._bp?.pendingReads.length, 0);
});

it('Fetch response cancellation settles a pending read on disconnect', async () => {
  const stream = new ServerHttp3Stream();
  let cancelled = 0;
  const handler = createFetchHandler(() => new Response(new ReadableStream({
    pull() { /* keep read pending until cancellation */ },
    cancel() { cancelled++; },
  })));
  handler(stream, { ':method': 'GET', ':path': '/', ':authority': 'localhost' }, { endStream: true });
  await new Promise(resolve => setImmediate(resolve));
  stream.destroy();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(cancelled, 1);
  assert.equal(stream.listenerCount('aborted'), 0);
});
