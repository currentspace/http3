import { test, before, type TestContext } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { setImmediate as nextTurn, setTimeout as delay } from 'node:timers/promises';
import { connect as connectH2 } from 'node:http2';
import { createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { Http3SecureServer } from '../../lib/server.js';
import { ServerHttp3Stream } from '../../lib/stream.js';
import { connectAsync } from '../../lib/client.js';
import type { NativeEvent, ServerEventLoopLike } from '../../lib/event-loop.js';
import { generateTestCerts } from '../support/generate-certs.js';

let tls: ReturnType<typeof generateTestCerts>;
before(() => { tls = generateTestCerts(); });

function fakeLoop() {
  const closes: Array<[number, number, number]> = [];
  const writes: Array<{ data: Buffer; fin: boolean }> = [];
  const loop = {
    streamSend: (_conn: number, _id: number, data: Buffer, fin: boolean) => {
      writes.push({ data, fin }); return Math.max(data.length, fin ? 1 : 0);
    },
    streamClose: (conn: number, id: number, code: number) => { closes.push([conn, id, code]); },
    streamReadCredit: () => true,
  } as unknown as ServerEventLoopLike;
  return { loop, closes, writes };
}

function fakeStream() {
  const fixture = fakeLoop();
  const stream = new ServerHttp3Stream();
  stream._connHandle = 1; stream._streamId = 4; stream._eventLoop = fixture.loop;
  return { ...fixture, stream };
}

function fakeServer() {
  const fixture = fakeLoop();
  const server = new Http3SecureServer(tls);
  const internal = server as unknown as {
    _eventLoop: ServerEventLoopLike;
    _streams: Map<string, ServerHttp3Stream>;
    _dispatchEvents: (events: NativeEvent[]) => void;
  };
  internal._eventLoop = fixture.loop;
  let stream!: ServerHttp3Stream;
  let requests = 0;
  server.on('stream', value => { stream = value; requests++; stream.on('error', () => {}); });
  const dispatch = (eventType: number, rest: Partial<NativeEvent> = {}) =>
    internal._dispatchEvents([{ eventType, connHandle: 1, streamId: 4, ...rest }]);
  dispatch(3, { headers: [{ name: ':method', value: 'POST' }, { name: ':path', value: '/' }] });
  return { ...fixture, stream, dispatch, streams: internal._streams, requests: () => requests };
}

test('server automatic duplex destruction preserves admitted writes', async () => {
  const { stream, closes, writes } = fakeStream();
  const closed = once(stream, 'close');
  stream.resume(); stream._pushData(null); stream.end('response');
  await closed;
  assert.equal(stream.readableEnded, true);
  assert.equal(stream.writableFinished, true);
  assert.deepEqual(writes.map(write => [write.data.toString(), write.fin]), [['response', true]]);
  assert.deepEqual(closes, []);
  stream.close();
  assert.deepEqual(closes, [], 'close after successful destruction must not discard admitted writes');
});

test('server close preserves its explicit code through repeated close/destroy', async () => {
  const { stream, closes } = fakeStream();
  const closed = once(stream, 'close');
  stream.close(0x010b); stream.close(); stream.destroy();
  await closed;
  assert.deepEqual(closes, [[1, 4, 0x010b]]);
});

test('server destroy rejects a blocked final write exactly once despite late drain', async () => {
  const { stream, closes, writes } = fakeStream();
  stream._blocked = true;
  stream.on('error', () => {});
  const closed = new Promise<void>(resolve => stream.once('close', resolve));
  const error = new Error('cancel response');
  let calls = 0;
  let failure: Error | null | undefined;
  stream.end('pending', (err?: Error | null) => { calls++; failure = err; });
  stream.destroy(error);
  await closed;
  stream._onNativeDrain(); await nextTurn();
  assert.equal(calls, 1); assert.equal(failure, error);
  assert.deepEqual(closes, [[1, 4, 0x010c]]);
  assert.deepEqual(writes, []);
});

test('server destroy rejects an admitted write callback once before its scheduled release', async () => {
  const { stream, closes } = fakeStream();
  stream.on('error', () => {});
  const closed = new Promise<void>(resolve => stream.once('close', resolve));
  const error = new Error('cancel admitted write');
  let calls = 0;
  let failure: Error | null | undefined;
  stream.write(Buffer.alloc(stream.writableHighWaterMark * 2), err => { calls++; failure = err; });
  assert.equal(calls, 0);
  stream.destroy(error);
  await closed; await nextTurn();
  assert.equal(calls, 1); assert.equal(failure, error);
  assert.deepEqual(closes, [[1, 4, 0x010c]]);
});

for (const action of ['destroy', 'close'] as const) {
  test(`server ${action} before request FIN removes tracking and ignores late trailers`, async () => {
    const { stream, streams, dispatch, requests } = fakeServer();
    const closed = once(stream, 'close');
    stream[action](); await closed;
    assert.equal(streams.size, 0);
    dispatch(5);
    dispatch(3, { headers: [{ name: 'x-trailer', value: 'late' }] });
    dispatch(8);
    assert.equal(requests(), 1);
    assert.equal(streams.size, 0);
  });
}

test('server request FIN retains pending response drain routing until full completion', async () => {
  const { stream, streams, dispatch, closes } = fakeServer();
  stream._blocked = true;
  stream.end('response');
  dispatch(5);
  await nextTurn();
  assert.equal(stream.readableEnded, true);
  assert.equal(stream.writableFinished, false);
  assert.equal(streams.size, 1);
  const closed = once(stream, 'close');
  dispatch(8);
  await closed;
  assert.equal(streams.size, 0);
  assert.deepEqual(closes, []);
});

test('server peer reset preserves the code even when an abort handler destroys the stream', async () => {
  const { stream, streams, dispatch, closes } = fakeServer();
  stream.once('aborted', () => stream.destroy());
  const closed = once(stream, 'close');
  dispatch(6, { meta: { errorCode: 0x010b } });
  await closed;
  dispatch(6, { meta: { errorCode: 0x010b } });
  assert.equal(streams.size, 0);
  assert.deepEqual(closes, [[1, 4, 0x010b]]);
});

test('server session teardown does not send stream resets to a closed transport', async () => {
  const { stream, dispatch, closes, streams } = fakeServer();
  const closed = new Promise<void>(resolve => stream.once('close', resolve));
  dispatch(7);
  await closed;
  assert.equal(streams.size, 0);
  assert.deepEqual(closes, []);
});

const variants = process.env.HTTP3_WASM === '1'
  ? ['h2', 'native-h3', 'wasm-h3'] as const : ['h2', 'native-h3'] as const;
type Variant = 'h2' | 'native-h3' | 'wasm-h3';

async function fixture(t: TestContext, variant: Variant, handler: (stream: ServerHttp3Stream) => void) {
  const runtimeMode = variant === 'wasm-h3' ? 'wasm' : 'portable';
  const server = new Http3SecureServer({ ...tls, runtimeMode });
  server.on('stream', (stream, headers) => {
    stream.on('error', () => {});
    if (headers[':path'] === '/probe') { stream.respond({ ':status': '200' }); stream.end('ok'); return; }
    handler(stream);
  });
  const listening = once(server, 'listening'); server.listen(0, '::'); await listening;
  const origin = `https://localhost:${server.address()!.port}`;
  const session = variant === 'h2' ? connectH2(origin, { rejectUnauthorized: false })
    : await connectAsync(origin, { rejectUnauthorized: false, runtimeMode });
  t.after(async () => {
    if (variant === 'h2') (session as ReturnType<typeof connectH2>).destroy();
    else await session.close();
    await server.close();
  });
  const tracked = () => {
    const state = server as unknown as { _streams: Map<string, unknown>; _h2SessionStreams: Map<unknown, Set<unknown>> };
    return state._streams.size + [...state._h2SessionStreams.values()].reduce((n, streams) => n + streams.size, 0);
  };
  const probe = async () => {
    const request = session.request({ ':path': '/probe' }, { endStream: true });
    let body = ''; for await (const data of request) body += String(data);
    assert.equal(body, 'ok', 'cancellation must leave the connection usable');
  };
  return { session, tracked, probe };
}

for (const variant of variants) {
  for (const action of ['destroy', 'destroy-error', 'close'] as const) {
    test(`${variant}: raw server ${action} notifies the peer and releases tracking before request FIN`, { timeout: 10000 }, async t => {
      let raw!: ServerHttp3Stream;
      const { session, tracked, probe } = await fixture(t, variant, stream => {
        raw = stream; stream.respond({ ':status': '200' }); stream.write('ready');
      });
      const request = session.request({ ':method': 'POST', ':path': '/' });
      let peerError: (Error & { h3Code?: number }) | undefined;
      request.on('error', error => { peerError = error; });
      request.resume(); await once(request, 'response');
      const peerClosed = new Promise<void>(resolve => request.once('close', resolve));
      const serverClosed = new Promise<void>(resolve => raw.once('close', resolve));
      if (action === 'destroy-error') raw.destroy(new Error('failed response'));
      else if (action === 'close') { raw.close(variant === 'h2' ? 8 : 0x0102); raw.close(); raw.destroy(); }
      else raw.destroy();
      await Promise.all([peerClosed, serverClosed]);
      assert.equal(tracked(), 0);
      if (action === 'close') {
        if (variant === 'h2') assert.equal((request as { rstCode?: number }).rstCode, 8);
        else assert.equal(peerError?.h3Code, 0x0102);
      }
      await probe();
    });
  }

  test(`${variant}: response FIN preserves the open request half`, { timeout: 10000 }, async t => {
    let raw!: ServerHttp3Stream;
    let upload = '';
    const { session, tracked } = await fixture(t, variant, stream => {
      raw = stream; stream.on('data', chunk => { upload += String(chunk); });
      stream.respond({ ':status': '200' }); stream.end('early response');
    });
    const request = session.request({ ':method': 'POST', ':path': '/' });
    let response = ''; request.on('data', chunk => { response += String(chunk); });
    await once(request, 'end');
    assert.equal(response, 'early response');
    assert.equal(raw.destroyed, false); assert.equal(tracked(), 1);
    const closed = once(raw, 'close');
    request.write(Buffer.from('later request body')); request.end(); await closed;
    assert.equal(upload, 'later request body'); assert.equal(tracked(), 0);
  });

  test(`${variant}: request FIN and automatic destruction preserve a queued 20 MiB response`, { timeout: 15000 }, async t => {
    const payload = Buffer.alloc(64 * 1024, 0x61);
    const expected = createHash('sha256');
    for (let i = 0; i < 320; i++) expected.update(payload);
    let raw!: ServerHttp3Stream;
    let sending!: Promise<void>;
    const { session, tracked } = await fixture(t, variant, stream => {
      raw = stream; stream.resume(); stream.respond({ ':status': '200' });
      sending = pipeline(Readable.from((function* () { for (let i = 0; i < 320; i++) yield payload; })()), stream);
    });
    const request = session.request({ ':path': '/' }, { endStream: true });
    request.pause(); await once(request, 'response'); await delay(100);
    assert.equal(raw.readableEnded, true); assert.equal(tracked(), 1);
    const hash = createHash('sha256'); let bytes = 0;
    const closed = once(raw, 'close');
    for await (const data of request) { bytes += (data as Buffer).length; hash.update(data); }
    await sending; await closed;
    assert.equal(bytes, 20 * 1024 * 1024);
    assert.equal(hash.digest('hex'), expected.digest('hex'));
    assert.equal(tracked(), 0);
  });

  if (variant !== 'h2') {
    test(`${variant}: respondWithBody shortcut finishes locally without cancelling queued transport bytes`, { timeout: 15000 }, async t => {
      // WASM now streams bodies above its 64 KiB admission bound. Exercise the
      // shortcut here; large-writes.test.ts covers its backpressured path.
      const payload = Buffer.alloc(variant === 'wasm-h3' ? 64 * 1024 : 20 * 1024 * 1024, 0x62);
      let serverClosed!: Promise<unknown[]>;
      const { session, tracked } = await fixture(t, variant, stream => {
        serverClosed = once(stream, 'close');
        stream.respondWithBody({ ':status': '200' }, payload);
      });
      const request = session.request({ ':path': '/' }, { endStream: true });
      request.pause(); await once(request, 'response');
      await serverClosed;
      assert.equal(tracked(), 0);
      assert.equal(request.readableEnded, false, 'peer is still paused after local completion');
      const hash = createHash('sha256'); let bytes = 0;
      for await (const data of request) { bytes += (data as Buffer).length; hash.update(data); }
      assert.equal(bytes, payload.length);
      assert.equal(hash.digest('hex'), createHash('sha256').update(payload).digest('hex'));
    });
  }

  test(`${variant}: cancellation releases a blocked response producer`, { timeout: 10000 }, async t => {
    let raw!: ServerHttp3Stream;
    let result!: Promise<Error | undefined>;
    let settled = false;
    const { session, tracked, probe } = await fixture(t, variant, stream => {
      raw = stream; stream.respond({ ':status': '200' });
      result = pipeline(Readable.from((function* () {
        for (let i = 0; i < 512; i++) yield Buffer.alloc(64 * 1024);
      })()), stream).then(() => undefined, error => error as Error).finally(() => { settled = true; });
    });
    const request = session.request({ ':method': 'POST', ':path': '/' });
    request.on('error', () => {});
    request.pause(); await once(request, 'response'); await delay(150);
    assert.equal(settled, false);
    const closed = new Promise<void>(resolve => request.once('close', resolve));
    raw.destroy(); request.resume();
    assert.ok(await result, 'blocked pipeline must reject'); await closed;
    assert.equal(tracked(), 0);
    await probe();
  });
}
