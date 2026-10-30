import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { Readable, Writable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { connect as connectH2 } from 'node:http2';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createDevTls } from '../../lib/dev-cert.js';
import { Http3NodeServer, Http3IncomingMessage } from '../../lib/node-server.js';
import { ClientHttp3Stream } from '../../lib/stream.js';
import { connectAsync } from '../../lib/client.js';

let directory: string;
let tls: ReturnType<typeof createDevTls>;
before(() => { directory = mkdtempSync(join(tmpdir(), 'http3-download-')); tls = createDevTls({ directory }); });
after(() => rmSync(directory, { recursive: true, force: true }));
const modes = process.env.HTTP3_WASM === '1' ? ['portable', 'wasm'] as const : ['portable'] as const;
const delay = (ms: number): Promise<void> => new Promise(resolve => setTimeout(resolve, ms));
const payload = Buffer.alloc(64 * 1024, 0x61);
function source(): Readable {
  return Readable.from((function* () { for (let i = 0; i < 320; i++) yield payload; })());
}

for (const runtimeMode of modes) {
  test(`${runtimeMode}: peer cancellation after response FIN reaches an unread client`, { timeout: 10000 }, async t => {
    let cancelPeer!: () => void;
    const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode }, (req, res) => {
      assert.ok(req instanceof Http3IncomingMessage);
      cancelPeer = () => req.stream.close(0x010c);
      res.end('unread response');
    });
    server.listen(0); await once(server, 'listening');
    t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
    const session = await connectAsync(`https://localhost:${server.address()!.port}`, { ca: tls.ca, runtimeMode });
    t.after(async () => session.close());
    // Keep the request writable while the response finishes.
    const stream = session.request({ ':method': 'POST', ':path': '/' });
    stream.pause();
    let sawFin!: () => void;
    const fin = new Promise<void>(resolve => { sawFin = resolve; });
    const push = stream._pushData.bind(stream);
    stream._pushData = chunk => { push(chunk); if (chunk === null) sawFin(); };
    await fin;
    assert.equal(stream.readableEnded, false);
    const reset = once(stream, 'error');
    const closed = new Promise<void>(resolve => stream.once('close', resolve));
    cancelPeer();
    const [error] = await reset;
    assert.equal(error.code, 'ERR_HTTP3_STREAM_ERROR');
    await closed;
    assert.equal((session as unknown as { _streams: Map<number, unknown> })._streams.size, 0);
  });

  for (const protocol of ['h2', 'h3'] as const) {
    test(`${runtimeMode} ${protocol}: paused download stays bounded and resumes into a slow reader`, { timeout: 15000 }, async t => {
      let finished = false;
      const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode }, (req, res) => {
        if (req.url === '/probe') { res.end('ok'); return; }
        void pipeline(source(), res).then(() => { finished = true; }, () => {});
      });
      server.listen(0); await once(server, 'listening');
      t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
      const origin = `https://localhost:${server.address()!.port}`;
      const session = protocol === 'h2' ? connectH2(origin, { ca: tls.ca }) : await connectAsync(origin, { ca: tls.ca, runtimeMode });
      t.after(async () => { await session.close(); });
      const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
      stream.pause(); await once(stream, 'response');
      await delay(200);
      const buffered = stream.readableLength + (stream instanceof ClientHttp3Stream ? stream._bp?.pendingReadBytes ?? 0 : 0);
      assert.ok(buffered <= 256 * 1024, `paused reader buffered ${buffered} bytes`);
      assert.equal(finished, false, 'sender must remain backpressured');
      const probe = session.request({ ':path': '/probe' }, { endStream: true });
      let text = ''; for await (const chunk of probe) text += String(chunk);
      assert.equal(text, 'ok');
      const hash = createHash('sha256'); let bytes = 0;
      await pipeline(stream, new Writable({ highWaterMark: 16384, write(chunk: Buffer, _encoding, callback) {
        bytes += chunk.length; hash.update(chunk); setTimeout(callback, 1);
      } }));
      const expected = createHash('sha256'); for (let i = 0; i < 320; i++) expected.update(payload);
      assert.equal(bytes, 20 * 1024 * 1024);
      assert.equal(hash.digest('hex'), expected.digest('hex'));
    });
  }

  for (const responseKind of ['idle-sse', 'blocked-download'] as const) {
    test(`${runtimeMode}: client destroy cancels ${responseKind} and releases the producer`, { timeout: 10000 }, async t => {
      let completed!: (error?: Error) => void;
      const remoteClosed = new Promise<Error | undefined>(resolve => { completed = resolve; });
      const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode }, (req, res) => {
        if (req.url === '/probe') { res.end('ok'); return; }
        if (responseKind === 'idle-sse') {
          res.writeHead(200, { 'content-type': 'text/event-stream' }); res.write('data: ready\n\n');
          res.once('close', () => completed());
        } else {
          void pipeline(source(), res).then(() => completed(), error => completed(error as Error));
        }
      });
      server.listen(0); await once(server, 'listening');
      t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
      const session = await connectAsync(`https://localhost:${server.address()!.port}`, { ca: tls.ca, runtimeMode });
      t.after(async () => session.close());
      const stream = session.request({ ':path': '/' }, { endStream: true });
      stream.pause(); await once(stream, 'response'); await delay(200);
      const closed = once(stream, 'close'); stream.destroy(); await closed;
      const error = await remoteClosed;
      if (responseKind === 'blocked-download') assert.ok(error, 'blocked producer must reject on cancellation');
      assert.equal((session as unknown as { _streams: Map<number, unknown> })._streams.size, 0);
      const probe = session.request({ ':path': '/probe' }, { endStream: true });
      let text = ''; for await (const chunk of probe) text += String(chunk);
      assert.equal(text, 'ok');
    });
  }

  test(`${runtimeMode}: client destroy rejects a flow-controlled upload and cancels the peer`, { timeout: 10000 }, async t => {
    let cancelled!: () => void;
    const remoteClosed = new Promise<void>(resolve => { cancelled = resolve; });
    const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode }, (_req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream' }); res.flushHeaders();
      res.once('close', cancelled);
    });
    server.listen(0); await once(server, 'listening');
    t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
    const session = await connectAsync(`https://localhost:${server.address()!.port}`, { ca: tls.ca, runtimeMode });
    t.after(async () => session.close());
    const stream = session.request({ ':method': 'POST', ':path': '/' });
    const headers = once(stream, 'response');
    let settled = false;
    const sending = pipeline(source(), stream).then(() => { settled = true; return undefined; }, error => { settled = true; return error as Error; });
    await headers; await delay(200); assert.equal(settled, false);
    stream.destroy();
    assert.ok(await sending, 'pending producer must reject');
    await remoteClosed;
  });
}
