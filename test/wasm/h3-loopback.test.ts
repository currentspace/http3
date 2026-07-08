/**
 * C3 (docs/WASM_CLIENT_PLAN.md §7): H3 loopback tests, wasm client <->
 * native server — mirrors test/interop/h3-loopback.test.ts's scenarios,
 * driven through the wasm runtime via the full public API
 * (`connectAsync(..., { runtimeMode: 'wasm' })`).
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips
 * cleanly (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import type { ClientHttp3Stream } from '../../lib/stream.js';
import { createWasmH3Pair, wasmSkipReason } from '../support/wasm-test-helpers.js';
import type { WasmH3Pair } from '../support/wasm-test-helpers.js';

const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;

function waitForResponse(stream: ClientHttp3Stream, timeoutMs = 5000): Promise<{ status: string; headers: Record<string, string>; body: Buffer }> {
  return new Promise((resolve, reject) => {
    let status = '';
    let headers: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('waitForResponse timed out')), timeoutMs);
    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      headers = h;
    });
    stream.on('data', (chunk: Buffer) => chunks.push(chunk));
    stream.on('end', () => {
      clearTimeout(timer);
      resolve({ status, headers, body: Buffer.concat(chunks) });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

/**
 * Accumulate a server-side request body from EVENT_DATA/EVENT_HEADERS
 * until EVENT_FINISHED (or an event with `fin: true`) for a given
 * streamId. Polls `serverEvents.allEvents` (rather than swapping out
 * `serverEvents.callback`) because the native binding captures the
 * callback *reference* passed to its constructor once — reassigning the
 * `EventCollector`'s `.callback` property afterward would never actually
 * be invoked. Mirrors the polling style of
 * test/support/native-test-helpers.ts's `waitForH3BodyData`.
 */
function collectServerBody(pair: WasmH3Pair, streamId: number, timeoutMs = 10000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const chunks: Buffer[] = [];
    let processed = 0;

    const check = (): void => {
      const events = pair.serverEvents.allEvents as Array<{ eventType: number; streamId: number; data?: Buffer; fin?: boolean }>;
      for (; processed < events.length; processed++) {
        const evt = events[processed];
        if (!evt || evt.streamId !== streamId) continue;
        if ((evt.eventType === EVENT_DATA || evt.eventType === EVENT_HEADERS) && evt.data) {
          chunks.push(Buffer.from(evt.data));
        }
        if (evt.eventType === EVENT_FINISHED || evt.fin) {
          resolve(Buffer.concat(chunks));
          return;
        }
      }
      if (Date.now() > deadline) {
        reject(new Error('collectServerBody timed out'));
        return;
      }
      setTimeout(check, 10);
    };
    check();
  });
}

describe('wasm H3 loopback (C3)', { skip: wasmSkipReason() }, () => {
  it('handshake completes via connectAsync with runtimeMode wasm', async () => {
    const pair = await createWasmH3Pair();
    try {
      assert.equal(pair.client.handshakeComplete, true);
      assert.equal(pair.client.runtimeInfo?.selectedMode, 'wasm');
      assert.equal(pair.client.runtimeInfo?.driver, 'wasm');
    } finally {
      await pair.cleanup();
    }
  });

  it('GET request receives a response with a body', async () => {
    const pair = await createWasmH3Pair();
    try {
      const stream = pair.client.request({
        ':method': 'GET',
        ':path': '/hello',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      const headersEvent = await pair.serverEvents.waitForEvent(EVENT_HEADERS);
      pair.server.sendResponseHeaders(headersEvent.connHandle, headersEvent.streamId, [
        { name: ':status', value: '200' },
        { name: 'x-server', value: 'wasm-h3-loopback' },
      ], false);
      pair.server.streamSend(headersEvent.connHandle, headersEvent.streamId, Buffer.from('hello from wasm test'), true);

      const res = await waitForResponse(stream);
      assert.equal(res.status, '200');
      assert.equal(res.headers['x-server'], 'wasm-h3-loopback');
      assert.equal(res.body.toString('utf8'), 'hello from wasm test');
    } finally {
      await pair.cleanup();
    }
  });

  it('request body upload exercises backpressure (STREAM_BLOCKED -> DRAIN)', { timeout: 15000 }, async () => {
    const pair = await createWasmH3Pair({ initialMaxData: 32 * 1024, initialMaxStreamDataBidiLocal: 16 * 1024 });
    try {
      const stream = pair.client.request({
        ':method': 'POST',
        ':path': '/upload',
        ':authority': 'localhost',
        ':scheme': 'https',
      });

      const headersEvent = await pair.serverEvents.waitForEvent(EVENT_HEADERS);
      const bodyPromise = collectServerBody(pair, headersEvent.streamId);

      const payload = Buffer.alloc(256 * 1024, 'U');
      let sawBackpressure = false;
      const chunkSize = 8192;
      for (let offset = 0; offset < payload.length; offset += chunkSize) {
        const chunk = payload.subarray(offset, Math.min(offset + chunkSize, payload.length));
        const ok = stream.write(chunk);
        if (!ok) {
          sawBackpressure = true;
          await new Promise<void>((resolve) => stream.once('drain', resolve));
        }
      }
      stream.end();

      const received = await bodyPromise;
      assert.equal(received.length, payload.length);
      assert.equal(Buffer.compare(received, payload), 0);
      assert.equal(sawBackpressure, true, 'expected at least one backpressured write (STREAM_BLOCKED -> DRAIN) for a 256KB body over a 32KB connection window');

      pair.server.sendResponseHeaders(headersEvent.connHandle, headersEvent.streamId, [{ name: ':status', value: '200' }], false);
      pair.server.streamSend(headersEvent.connHandle, headersEvent.streamId, Buffer.from('ok'), true);
      const res = await waitForResponse(stream);
      assert.equal(res.status, '200');
    } finally {
      await pair.cleanup();
    }
  });

  it('trailers arrive as a "trailers" event after the response body', async () => {
    const pair = await createWasmH3Pair();
    try {
      const stream = pair.client.request({
        ':method': 'GET',
        ':path': '/trailers',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      const trailersPromise = new Promise<Record<string, string>>((resolve) => {
        stream.once('trailers', (t: Record<string, string>) => resolve(t));
      });

      const headersEvent = await pair.serverEvents.waitForEvent(EVENT_HEADERS);
      pair.server.sendResponseHeaders(headersEvent.connHandle, headersEvent.streamId, [{ name: ':status', value: '200' }], false);
      pair.server.streamSend(headersEvent.connHandle, headersEvent.streamId, Buffer.from('body'), false);
      pair.server.sendTrailers(headersEvent.connHandle, headersEvent.streamId, [{ name: 'x-trailer', value: 'trailer-value' }]);

      const res = await waitForResponse(stream);
      assert.equal(res.body.toString('utf8'), 'body');
      const trailers = await trailersPromise;
      assert.equal(trailers['x-trailer'], 'trailer-value');
    } finally {
      await pair.cleanup();
    }
  });

  it('GOAWAY from the server is observed as a "goaway" session event', async () => {
    const pair = await createWasmH3Pair();
    try {
      const stream = pair.client.request({
        ':method': 'GET',
        ':path': '/keep-open',
        ':authority': 'localhost',
        ':scheme': 'https',
      });

      const headersEvent = await pair.serverEvents.waitForEvent(EVENT_HEADERS);
      const goawayPromise = new Promise<{ lastStreamId: number }>((resolve) => {
        pair.client.once('goaway', (info: { lastStreamId: number }) => resolve(info));
      });

      pair.server.closeSession(headersEvent.connHandle, 0, 'graceful shutdown');

      const goawayInfo = await goawayPromise;
      assert.equal(typeof goawayInfo.lastStreamId, 'number');
      stream.destroy();
    } finally {
      await pair.cleanup();
    }
  });

  it('RESET from the server destroys the client stream with an error', async () => {
    const pair = await createWasmH3Pair();
    try {
      const stream = pair.client.request({
        ':method': 'GET',
        ':path': '/reset-me',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      const headersEvent = await pair.serverEvents.waitForEvent(EVENT_HEADERS);

      const abortedPromise = new Promise<void>((resolve) => {
        stream.once('aborted', () => resolve());
      });
      const errorPromise = new Promise<Error>((resolve) => {
        stream.once('error', (err: Error) => resolve(err));
      });

      pair.server.streamClose(headersEvent.connHandle, headersEvent.streamId, 1 /* arbitrary H3 app error code */);

      await abortedPromise;
      const err = await errorPromise;
      assert.match(err.message, /reset/i);
    } finally {
      await pair.cleanup();
    }
  });

  it('datagram round-trip (client -> server -> client)', async () => {
    const pair = await createWasmH3Pair({ enableDatagrams: true });
    try {
      const clientPayload = Buffer.from('hello from wasm client datagram');
      const serverDatagramPromise = pair.serverEvents.waitForEvent(14 /* EVENT_DATAGRAM */);
      assert.equal(pair.client.sendDatagram(clientPayload), true);

      const serverEvt = await serverDatagramPromise;
      assert.ok(serverEvt.data);
      assert.equal(Buffer.compare(Buffer.from(serverEvt.data), clientPayload), 0);

      const echoPayload = Buffer.from('echo from native server');
      const clientDatagramPromise = new Promise<Buffer>((resolve) => {
        pair.client.once('datagram', (data: Buffer) => resolve(data));
      });
      assert.equal(pair.server.sendDatagram(serverEvt.connHandle, echoPayload), true);

      const echoed = await clientDatagramPromise;
      assert.equal(Buffer.compare(echoed, echoPayload), 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('ping resolves with a duration', async () => {
    const pair = await createWasmH3Pair();
    try {
      const duration = await new Promise<number>((resolve, reject) => {
        pair.client.ping((err, d) => {
          if (err) reject(err);
          else resolve(d);
        });
      });
      assert.equal(typeof duration, 'number');
      assert.ok(duration >= 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('close() resolves promptly via the shutdown sentinel, not the 5s fallback', async () => {
    const pair = await createWasmH3Pair();
    try {
      const start = Date.now();
      await pair.client.close();
      const elapsed = Date.now() - start;
      assert.ok(elapsed < 2500, `close() took ${elapsed}ms — expected well under the 5s SHUTDOWN_TIMEOUT_MS fallback`);
      assert.equal(pair.client.closed, true);
    } finally {
      // pair.cleanup() calls client.close() again — idempotent, a no-op
      // resolving immediately per WasmH3ClientEventLoop's close() contract.
      await pair.cleanup();
    }
  });
});
