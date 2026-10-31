import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { setImmediate as nextTurn, setTimeout as delay } from 'node:timers/promises';
import { ServerHttp3Stream, ClientHttp3Stream } from '../../lib/stream.js';
import { QuicStream } from '../../lib/quic-stream.js';

const limit = 16 * 1024;
const payload = Buffer.alloc(limit * 5 + 17, 0x6b);
const hash = (data: Buffer): string => createHash('sha256').update(data).digest('hex');
const variants = ['h3-server', 'h3-client', 'quic-server', 'quic-client'] as const;

function makeStream(variant: typeof variants[number], send: (data: Buffer, fin: boolean) => number, bounded = true): ServerHttp3Stream | ClientHttp3Stream | QuicStream {
  const loop = {
    ...(bounded ? { maxStreamSendBytes: limit } : {}),
    streamSend: (...args: unknown[]) => send(args.at(-2) as Buffer, args.at(-1) as boolean),
    streamClose: () => true,
  };
  if (variant === 'h3-server') {
    const stream = new ServerHttp3Stream();
    stream._eventLoop = loop as unknown as NonNullable<typeof stream._eventLoop>;
    return stream;
  }
  if (variant === 'h3-client') {
    const stream = new ClientHttp3Stream();
    stream._eventLoop = loop as unknown as NonNullable<typeof stream._eventLoop>;
    return stream;
  }
  const stream = new QuicStream();
  if (variant === 'quic-client') stream._clientLoop = loop as unknown as NonNullable<typeof stream._clientLoop>;
  else stream._serverLoop = loop as unknown as NonNullable<typeof stream._serverLoop>;
  return stream;
}

for (const variant of variants) {
  test(`${variant}: bounded end waits for deferred BLOCKED and sends FIN only on the last slice`, async t => {
    const chunks: Buffer[] = [];
    const fins: boolean[] = [];
    const stream = makeStream(variant, (data, fin) => {
      chunks.push(data); fins.push(fin);
      if (chunks.length === 1) queueMicrotask(() => stream._onNativeBlocked());
      return Math.max(1, data.length);
    });
    t.after(() => stream.destroy());
    let callbacks = 0;
    const finished = new Promise<void>((resolve, reject) => stream.end(payload, (error?: Error | null) => {
      callbacks++;
      if (error) reject(error); else resolve();
    }));
    await delay(25);
    assert.equal(chunks.length, 1, 'deferred BLOCKED must stop the next slice');
    assert.equal(callbacks, 0);
    stream._onNativeDrain();
    await finished;
    assert.equal(callbacks, 1);
    assert.ok(chunks.every(chunk => chunk.length <= limit));
    assert.equal(hash(Buffer.concat(chunks)), hash(payload));
    assert.deepEqual(fins, [...Array<boolean>(5).fill(false), true]);
  });

  test(`${variant}: a large write preserves Node backpressure and ordering before end(data)`, async t => {
    const chunks: Buffer[] = [];
    const fins: boolean[] = [];
    const stream = makeStream(variant, (data, fin) => { chunks.push(data); fins.push(fin); return Math.max(1, data.length); });
    t.after(() => stream.destroy());
    let callbacks = 0;
    assert.equal(stream.write(payload, (error?: Error | null) => { callbacks++; assert.ifError(error); }), false);
    await new Promise<void>(resolve => stream.end('tail', resolve));
    assert.equal(callbacks, 1);
    assert.deepEqual(Buffer.concat(chunks), Buffer.concat([payload, Buffer.from('tail')]));
    assert.ok(fins.slice(0, -1).every(fin => !fin));
    assert.equal(fins.at(-1), true);
  });

  test(`${variant}: partial admission retries only unadmitted bytes before later slices`, async t => {
    const chunks: Buffer[] = [];
    let first = true;
    const stream = makeStream(variant, data => {
      if (first) {
        first = false;
        chunks.push(data.subarray(0, 37));
        setImmediate(() => stream._onNativeDrain());
        return 37;
      }
      chunks.push(data);
      return data.length;
    });
    t.after(() => stream.destroy());
    await new Promise<void>(resolve => stream.end(payload, resolve));
    assert.equal(hash(Buffer.concat(chunks)), hash(payload));
  });

  test(`${variant}: slice failure reaches the callback once and stops later slices`, async () => {
    let sends = 0;
    const error = new Error('allocation failure');
    const stream = makeStream(variant, data => {
      sends++;
      if (sends === 2) throw error;
      return data.length;
    });
    stream.on('error', () => {});
    let callbacks = 0;
    const failure = await new Promise<Error | null | undefined>(resolve => stream.end(payload, (err?: Error | null) => {
      callbacks++; resolve(err);
    }));
    assert.equal(failure, error);
    await nextTurn();
    assert.equal(callbacks, 1);
    assert.equal(sends, 2);
    assert.equal(stream.destroyed, true);
  });

  for (const stage of ['before-first-slice', 'scheduled', 'blocked'] as const) {
    test(`${variant}: destroy ${stage} rejects the write once and stops admission`, async () => {
      let sends = 0;
      const stream = makeStream(variant, data => {
        sends++;
        if (stage === 'blocked') queueMicrotask(() => stream._onNativeBlocked());
        return data.length;
      });
      stream.on('error', () => {});
      let callbacks = 0;
      const finished = new Promise<Error | null | undefined>(resolve => stream.end(payload, (error?: Error | null) => {
        callbacks++; resolve(error);
      }));
      if (stage !== 'before-first-slice') await nextTurn();
      if (stage === 'blocked') await delay(20);
      const before = sends;
      stream.destroy(new Error('cancel large write'));
      assert.match((await finished)?.message ?? '', /cancel large write/);
      await delay(20);
      assert.equal(callbacks, 1);
      assert.equal(sends, before);
      assert.equal(stream._bp?.drainCallbacks.length ?? 0, 0);
    });
  }

  test(`${variant}: transports without a bound keep one payload/FIN call`, async t => {
    const sends: Array<{ data: Buffer; fin: boolean }> = [];
    const stream = makeStream(variant, (data, fin) => { sends.push({ data, fin }); return data.length; }, false);
    t.after(() => stream.destroy());
    await new Promise<void>(resolve => stream.end(payload, resolve));
    assert.deepEqual(sends, [{ data: payload, fin: true }]);
  });

  test(`${variant}: empty final write preserves the FIN-only acceptance convention`, async t => {
    const fins: boolean[] = [];
    const stream = makeStream(variant, (data, fin) => { assert.equal(data.length, 0); fins.push(fin); return 1; });
    t.after(() => stream.destroy());
    await new Promise<void>(resolve => stream.end(resolve));
    assert.deepEqual(fins, [true]);
  });
}

test('H3 respondWithBody waits for blocked headers and bypasses the all-at-once shortcut', async t => {
  const stream = new ServerHttp3Stream();
  t.after(() => stream.destroy());
  const chunks: Buffer[] = [];
  let headers = 0;
  stream._eventLoop = {
    maxStreamSendBytes: limit,
    sendResponseHeaders() { headers++; queueMicrotask(() => stream._onNativeBlocked()); },
    sendResponse() { assert.fail('large response must not use the unbounded shortcut'); },
    streamSend(_conn: number, _id: number, data: Buffer) { chunks.push(data); return data.length; },
    streamClose() {},
  } as unknown as NonNullable<typeof stream._eventLoop>;
  const finished = new Promise<void>(resolve => stream.once('finish', resolve));
  stream.respondWithBody({}, payload);
  await delay(25);
  assert.equal(headers, 1);
  assert.equal(chunks.length, 0);
  stream._onNativeDrain();
  await finished;
  assert.equal(hash(Buffer.concat(chunks)), hash(payload));
});

for (const cancel of [false, true]) {
  test(`WASM trailers wait for the last body drain; cancel=${cancel}`, async t => {
    const stream = new ServerHttp3Stream();
    stream.on('error', () => {});
    t.after(() => stream.destroy());
    let sent = 0;
    let trailers = 0;
    let callbacks = 0;
    let allSent!: () => void;
    const bodyAdmitted = new Promise<void>(resolve => { allSent = resolve; });
    stream._eventLoop = {
      maxStreamSendBytes: limit,
      streamSend(_conn: number, _id: number, data: Buffer) {
        sent += data.length;
        if (sent === payload.length) { queueMicrotask(() => stream._onNativeBlocked()); allSent(); }
        return data.length;
      },
      streamClose() {},
      sendTrailers() { trailers++; assert.equal(sent, payload.length); },
    } as unknown as NonNullable<typeof stream._eventLoop>;
    stream.write(payload);
    stream.sendTrailers({ 'x-done': 'yes' });
    const finished = new Promise<Error | null | undefined>(resolve => stream.end((error?: Error | null) => { callbacks++; resolve(error); }));
    await bodyAdmitted; await nextTurn(); await nextTurn();
    assert.equal(trailers, 0);
    assert.equal(callbacks, 0);
    if (cancel) stream.destroy(new Error('cancel trailers'));
    else stream._onNativeDrain();
    const error = await finished;
    if (cancel) assert.match(error?.message ?? '', /cancel trailers/);
    else assert.ifError(error);
    stream._onNativeDrain(); await nextTurn();
    assert.equal(callbacks, 1);
    assert.equal(trailers, cancel ? 0 : 1);
  });
}
