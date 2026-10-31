import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import type { Readable } from 'node:stream';
import { setTimeout as delay } from 'node:timers/promises';
import { Http3NodeServer } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import type { Http3WasmCore } from '../../lib/wasm/core-loader.js';
import type { ServerHttp3Stream } from '../../lib/stream.js';
import { generateTestCerts } from '../support/generate-certs.js';
import { createWasmServerH3Pair, createWasmServerQuicPair, wasmSkipReason } from '../support/wasm-test-helpers.js';

const mib = 1024 * 1024;
function body(sizeMiB: number): Buffer {
  const pattern = Buffer.from(Array.from({ length: 251 }, (_, i) => i));
  return Buffer.alloc(sizeMiB * mib).fill(pattern);
}
function digest(data: Buffer): string { return createHash('sha256').update(data).digest('hex'); }
async function received(stream: Readable): Promise<{ bytes: number; hash: string }> {
  const hash = createHash('sha256');
  let bytes = 0;
  return new Promise((resolve, reject) => {
    stream.on('data', (chunk: Buffer) => { bytes += chunk.length; hash.update(chunk); });
    stream.once('error', reject);
    stream.once('end', () => resolve({ bytes, hash: hash.digest('hex') }));
    stream.resume();
  });
}
async function text(stream: Readable): Promise<string> {
  let value = '';
  for await (const chunk of stream) value += String(chunk);
  return value;
}

// Each case owns its own core(s), avoiding heap state left by allocation-failure
// probes. The larger bodies live in JS; each WASM crossing must remain bounded.
describe('WASM large Node stream writes', { skip: wasmSkipReason(), timeout: 120000 }, () => {
  for (const sizeMiB of [32, 64, 128]) {
    for (const method of ['respondWithBody', 'write', 'end'] as const) {
      it(`H3 server ${method}: ${sizeMiB} MiB matches bytes and hash with a native client`, { timeout: 20000 }, async t => {
        const pair = await createWasmServerH3Pair();
        t.after(async () => pair.cleanup());
        const payload = body(sizeMiB);
        let sent!: Promise<void>;
        let callbacks = 0;
        pair.server.on('stream', (stream, headers) => {
          if (headers[':path'] === '/probe') { stream.respondWithBody({}, 'ok'); return; }
          sent = new Promise<void>((resolve, reject) => {
            stream.once('error', reject);
            stream.once('finish', resolve);
          });
          if (method === 'respondWithBody') stream.respondWithBody({}, payload);
          else {
            stream.respond({});
            stream[method](payload, (error?: Error | null) => {
              callbacks++;
              assert.ifError(error);
              if (method === 'write') stream.end();
            });
          }
        });
        const result = await received(pair.client.request({ ':path': '/' }, { endStream: true }));
        await sent;
        assert.deepEqual(result, { bytes: payload.length, hash: digest(payload) });
        assert.equal(callbacks, method === 'respondWithBody' ? 0 : 1);
        assert.equal(await text(pair.client.request({ ':path': '/probe' }, { endStream: true })), 'ok');
      });
    }
  }

  for (const sizeMiB of [32, 64]) {
    it(`H3 client end: ${sizeMiB} MiB upload preserves payload and connection`, { timeout: 20000 }, async t => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      t.after(async () => pair.cleanup());
      const payload = body(sizeMiB);
      pair.server.on('stream', (stream, headers) => {
        if (headers[':path'] === '/probe') { stream.respondWithBody({}, 'ok'); return; }
        void received(stream).then(result => stream.respondWithBody({}, JSON.stringify(result)), error => stream.destroy(error as Error));
      });
      const request = pair.client.request({ ':method': 'POST', ':path': '/' });
      let callbacks = 0;
      request.end(payload, (error?: Error | null) => { callbacks++; assert.ifError(error); });
      assert.deepEqual(JSON.parse(await text(request)), { bytes: payload.length, hash: digest(payload) });
      assert.equal(callbacks, 1);
      assert.equal(await text(pair.client.request({ ':path': '/probe' }, { endStream: true })), 'ok');
    });
  }

  for (const side of ['client', 'server'] as const) {
    for (const sizeMiB of [32, 64]) {
      it(`QUIC ${side} end: ${sizeMiB} MiB preserves bytes, FIN, and a subsequent stream`, { timeout: 20000 }, async t => {
        const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
        t.after(async () => pair.cleanup());
        const payload = body(sizeMiB);
        const incoming = once(pair.serverSession, 'stream');
        const client = pair.client.openStream();
        // Raw QUIC creates the peer stream only when it receives stream data.
        client.write(Buffer.from('hello'));
        const [server] = await incoming;
        let callbacks = 0;
        if (side === 'client') {
          const reading = received(server);
          client.end(payload, (error?: Error | null) => { callbacks++; assert.ifError(error); });
          const result = await reading;
          assert.deepEqual(result, { bytes: payload.length + 5, hash: createHash('sha256').update('hello').update(payload).digest('hex') });
          server.end('ack');
          assert.equal(await text(client), 'ack');
        } else {
          server.resume(); client.end();
          server.end(payload, (error?: Error | null) => { callbacks++; assert.ifError(error); });
          assert.deepEqual(await received(client), { bytes: payload.length, hash: digest(payload) });
        }
        assert.equal(callbacks, 1);
        pair.serverSession.once('stream', stream => { stream.resume(); stream.end('ok'); });
        const probe = pair.client.openStream(); probe.end('probe');
        assert.equal(await text(probe), 'ok');
      });
    }
  }

  for (const side of ['client', 'server'] as const) {
    it(`QUIC ${side} destroy after partial admission resets the peer and preserves the session`, { timeout: 10000 }, async t => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
      t.after(async () => pair.cleanup());
      const incoming = once(pair.serverSession, 'stream');
      const client = pair.client.openStream(); client.write('hello');
      const [server] = await incoming;
      const sender = side === 'client' ? client : server;
      const peer = side === 'client' ? server : client;
      sender.on('error', () => {});
      const peerReset = once(peer, 'error');
      let bytes = 0;
      let cancelled = false;
      let callbacks = 0;
      peer.on('data', (chunk: Buffer) => {
        bytes += chunk.length;
        if (!cancelled && bytes > 64 * 1024) {
          cancelled = true;
          assert.equal(callbacks, 0, 'cancel before all application bytes are admitted');
          sender.destroy(new Error('cancel partial QUIC write'));
        }
      });
      peer.resume();
      const completed = new Promise<Error | null | undefined>(resolve => sender.end(body(32), (error?: Error | null) => { callbacks++; resolve(error); }));
      assert.match((await peerReset)[0].message, /stream ?reset/i);
      assert.ok(await completed);
      assert.equal(callbacks, 1);
      assert.ok(bytes < 32 * mib);
      pair.serverSession.once('stream', stream => { stream.resume(); stream.end('ok'); });
      const probe = pair.client.openStream(); probe.end('probe');
      assert.equal(await text(probe), 'ok');
    });
  }

  it('Node facade res.end accepts one 32 MiB response', { timeout: 20000 }, async t => {
    const tls = generateTestCerts();
    const payload = body(32);
    const server = new Http3NodeServer({ ...tls, runtimeMode: 'wasm' }, (_req, res) => res.end(payload));
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    const client = await connectAsync(`https://localhost:${server.address()!.port}`, { runtimeMode: 'portable', rejectUnauthorized: false });
    t.after(async () => {
      await client.close();
      await new Promise<void>(resolve => server.close(() => resolve()));
    });
    assert.deepEqual(await received(client.request({ ':path': '/' }, { endStream: true })), { bytes: payload.length, hash: digest(payload) });
  });

  it('trailing headers follow every byte of a large write', { timeout: 15000 }, async t => {
    const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm', initialMaxStreamDataBidiLocal: 64 * 1024 });
    t.after(async () => pair.cleanup());
    const payload = body(32);
    pair.server.on('stream', stream => {
      stream.respond({});
      stream.write(payload);
      stream.sendTrailers({ 'x-body-sha256': digest(payload) });
      stream.end();
    });
    const request = pair.client.request({ ':path': '/' }, { endStream: true });
    const trailers = once(request, 'trailers');
    assert.deepEqual(await received(request), { bytes: payload.length, hash: digest(payload) });
    assert.equal((await trailers)[0]['x-body-sha256'], digest(payload));
  });

  it('peer cancellation rejects a blocked large response once and leaves the session usable', { timeout: 10000 }, async t => {
    const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
    t.after(async () => pair.cleanup());
    let writer!: ServerHttp3Stream;
    let callbacks = 0;
    let complete!: (error: Error | null | undefined) => void;
    const completion = new Promise<Error | null | undefined>(resolve => { complete = resolve; });
    pair.server.on('stream', (stream, headers) => {
      if (headers[':path'] === '/probe') { stream.respondWithBody({}, 'ok'); return; }
      writer = stream;
      stream.on('error', () => {});
      stream.respond({});
      stream.end(body(32), (error?: Error | null) => { callbacks++; complete(error); });
    });
    const request = pair.client.request({ ':path': '/' }, { endStream: true });
    request.pause();
    await once(request, 'response');
    await delay(100);
    assert.equal(callbacks, 0);
    assert.equal(writer._blocked, true);
    request.destroy();
    assert.ok(await completion);
    await delay(25);
    assert.equal(callbacks, 1);
    assert.equal(writer.destroyed, true);
    assert.equal(await text(pair.client.request({ ':path': '/probe' }, { endStream: true })), 'ok');
  });

  it('concurrent 32 MiB responses stay backpressured while readers pause, then resume without corruption', { timeout: 25000 }, async t => {
    const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm', initialMaxData: 4 * mib, initialMaxStreamDataBidiLocal: 64 * 1024 });
    t.after(async () => pair.cleanup());
    const payload = body(32);
    const writers: ServerHttp3Stream[] = [];
    let finished = 0;
    let core: Http3WasmCore | undefined;
    let initialHeap = 0;
    let peakHeap = 0;
    pair.server.on('stream', (stream, headers) => {
      if (headers[':path'] === '/probe') { stream.respondWithBody({}, 'ok'); return; }
      writers.push(stream);
      if (!core) {
        core = (stream._eventLoop as unknown as { core: Http3WasmCore }).core;
        initialHeap = core.exports.memory.buffer.byteLength;
      }
      stream.once('finish', () => { finished++; });
      stream.respondWithBody({}, payload);
    });
    const sampling = setInterval(() => { peakHeap = Math.max(peakHeap, core?.exports.memory.buffer.byteLength ?? 0); }, 5);
    t.after(() => clearInterval(sampling));
    const readers = Array.from({ length: 3 }, () => pair.client.request({ ':path': '/' }, { endStream: true }));
    await Promise.all(readers.map(async stream => { stream.pause(); await once(stream, 'response'); }));
    await delay(150);
    assert.equal(finished, 0, 'paused readers must not admit whole bodies');
    assert.ok(writers.every(stream => stream._blocked), 'writers must observe transport backpressure');
    assert.equal(await text(pair.client.request({ ':path': '/probe' }, { endStream: true })), 'ok');
    const results = await Promise.all(readers.map(received));
    for (const result of results) assert.deepEqual(result, { bytes: payload.length, hash: digest(payload) });
    assert.equal(finished, 3);
    // A 96 MiB application workload must not become 96 MiB of retained WASM
    // writes. Allow protocol/connection scratch growth, not full-body copies.
    t.diagnostic(`WASM heap: initial ${initialHeap}, peak ${peakHeap} bytes for three 32 MiB responses`);
    assert.ok(peakHeap - initialHeap < 16 * mib, `WASM heap grew by ${peakHeap - initialHeap} bytes`);
  });
});
