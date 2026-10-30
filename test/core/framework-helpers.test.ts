import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { get } from 'node:https';
import { connect as connectH2 } from 'node:http2';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Http3NodeServer } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import { generateDevCertificate } from '../../lib/dev-cert.js';
import { proxy } from '../../lib/proxy.js';
import { installShutdownHandlers } from '../../lib/shutdown.js';

let directory: string;
let tls: { key: Buffer; cert: Buffer };
let ca: Buffer;
before(() => {
  directory = mkdtempSync(join(tmpdir(), 'http3-helper-'));
  generateDevCertificate(directory);
  tls = { key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')) };
  ca = readFileSync(join(directory, 'ca.pem'));
});
after(() => rmSync(directory, { recursive: true, force: true }));

for (const altSvc of [undefined, { maxAge: 12345 }, { maxAge: 0 }, false] as const) {
  test(`Alt-Svc ${JSON.stringify(altSvc)} on all three protocols`, async t => {
    const server = new Http3NodeServer({ ...tls, altSvc, allowHTTP1: true, runtimeMode: 'portable' }, (_req, res) => res.end('ok'));
    t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
    server.listen(0);
    await once(server, 'listening');
    const origin = `https://localhost:${server.address()!.port}`;
    const expected = altSvc === false ? undefined : `h3=":${server.address()!.port}"; ma=${altSvc?.maxAge ?? 86400}`;
    await new Promise<void>((resolve, reject) => {
      get(origin, { ca, agent: false }, response => {
        assert.equal(response.headers['alt-svc'], expected);
        response.resume(); response.once('end', resolve);
      }).once('error', reject);
    });
    for (const protocol of ['h2', 'h3']) {
      const session = protocol === 'h2' ? connectH2(origin, { ca }) : await connectAsync(origin, { ca, runtimeMode: 'portable' });
      try {
        const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
        stream.once('response', headers => assert.equal(headers['alt-svc'], expected));
        for await (const _chunk of stream) { /* drain */ }
      } finally { await session.close(); }
    }
  });
}

test('Alt-Svc malformed options fail before binding', () => {
  for (const altSvc of [true, 1, [], null]) assert.throws(() => new Http3NodeServer({ ...tls, altSvc: altSvc as never }, () => {}), TypeError);
  for (const maxAge of [-1, 0.5, NaN, Infinity, 2147483648]) assert.throws(() => new Http3NodeServer({ ...tls, altSvc: { maxAge } }, () => {}), RangeError);
});

test('proxy streams bodies, cookies and forwarding metadata across h1/h2/h3', async t => {
  const upstream = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    res.setHeader('connection', 'x-hop');
    res.setHeader('x-hop', 'must disappear');
    res.setHeader('alt-svc', 'h3=":1"; ma=60');
    res.setHeader('set-cookie', ['a=1; Path=/', 'b=2; Path=/']);
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ path: req.url, headers: req.headers, body: Buffer.concat(chunks).toString() }));
  });
  upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
  t.after(async () => new Promise<void>(resolve => upstream.close(() => resolve())));
  const port = (upstream.address() as { port: number }).port;
  const server = new Http3NodeServer({ ...tls, allowHTTP1: true, runtimeMode: 'portable' }, proxy(`http://127.0.0.1:${port}`, { rewrite: path => '/bff' + path }));
  server.listen(0); await once(server, 'listening');
  t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
  const origin = `https://localhost:${server.address()!.port}`;
  for (const protocol of ['h1', 'h2', 'h3']) {
    const check = (text: string): void => {
      const result = JSON.parse(text) as { path: string; headers: Record<string, string>; body: string };
      assert.equal(result.path, '/bff/test?q=1');
      assert.equal(result.headers['x-forwarded-proto'], 'https');
      assert.equal(result.headers['x-forwarded-host'], new URL(origin).host);
      assert.notEqual(result.headers['x-forwarded-for'], 'spoofed');
      assert.equal(result.headers.forwarded, undefined);
      assert.equal(result.headers['x-remove'], undefined);
      assert.equal(result.body, protocol === 'h1' ? '' : 'streamed body');
    };
    if (protocol === 'h1') {
      await new Promise<void>((resolve, reject) => {
        get(origin + '/test?q=1', { ca, agent: false, headers: { connection: 'x-remove', 'x-remove': 'remove', 'x-forwarded-for': 'spoofed', forwarded: 'spoofed' } }, async response => {
          assert.equal(response.headers['x-hop'], undefined);
          assert.equal(response.headers['alt-svc'], `h3=":${server.address()!.port}"; ma=86400`);
          assert.equal(response.headers['set-cookie']?.length, 2);
          const chunks: Buffer[] = []; for await (const chunk of response) chunks.push(chunk as Buffer);
          check(Buffer.concat(chunks).toString()); resolve();
        }).once('error', reject);
      });
    } else {
      const session = protocol === 'h2' ? connectH2(origin, { ca }) : await connectAsync(origin, { ca, runtimeMode: 'portable' });
      try {
        const stream = session.request({ ':method': 'POST', ':path': '/test?q=1', ':authority': new URL(origin).host, 'x-forwarded-for': 'spoofed', forwarded: 'spoofed' });
        stream.once('response', headers => { assert.equal(headers['x-hop'], undefined); assert.equal(headers['alt-svc'], `h3=":${server.address()!.port}"; ma=86400`); assert.equal(headers['set-cookie']?.length, 2); });
        stream.write(Buffer.from('streamed body')); stream.end();
        const chunks: Buffer[] = []; for await (const chunk of stream) chunks.push(chunk as Buffer);
        check(Buffer.concat(chunks).toString());
      } finally { await session.close(); }
    }
  }
});

test('shutdown controller drains then cleans up once, removes listeners and enforces deadline', async () => {
  const previous = process.listenerCount('SIGTERM');
  const calls: string[] = [];
  const controller = installShutdownHandlers(async () => { calls.push('drain'); }, { exit: false, onClose: () => { calls.push('cleanup'); } });
  assert.equal(process.listenerCount('SIGTERM'), previous + 1);
  await Promise.all([controller.close(), controller.close()]);
  assert.deepEqual(calls, ['drain', 'cleanup']);
  assert.equal(process.listenerCount('SIGTERM'), previous);
  const hanging = installShutdownHandlers(async () => new Promise(() => {}), { timeoutMs: 10, signals: [], exit: false });
  await assert.rejects(hanging.close(), /Shutdown exceeded/);
});

for (const protocol of ['h1', 'h2', 'h3']) {
  test(`proxy flushes idle SSE headers and cancels upstream on ${protocol} client abort`, async t => {
    let cancelled!: () => void;
    const cancellation = new Promise<void>(resolve => { cancelled = resolve; });
    const upstream = createServer((_req, res) => {
      res.setHeader('content-type', 'text/event-stream');
      res.flushHeaders();
      res.once('close', cancelled);
    });
    upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
    t.after(async () => { upstream.closeAllConnections(); await new Promise<void>(resolve => upstream.close(() => resolve())); });
    const server = new Http3NodeServer({ ...tls, allowHTTP1: true, runtimeMode: 'portable' }, proxy(`http://127.0.0.1:${(upstream.address() as { port: number }).port}`, { timeoutMs: 0 }));
    server.listen(0); await once(server, 'listening');
    t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
    const origin = `https://localhost:${server.address()!.port}`;
    if (protocol === 'h1') {
      await new Promise<void>((resolve, reject) => {
        get(origin, { ca, agent: false }, response => {
          assert.equal(response.headers['content-type'], 'text/event-stream');
          response.destroy(); resolve();
        }).once('error', reject);
      });
    } else {
      const session = protocol === 'h2' ? connectH2(origin, { ca }) : await connectAsync(origin, { ca, runtimeMode: 'portable' });
      try {
        const stream = session.request({ ':method': 'GET', ':path': '/', ':authority': new URL(origin).host }, { endStream: true });
        const [headers] = await once(stream, 'response');
        assert.equal((headers as Record<string, string>)['content-type'], 'text/event-stream');
        stream.close();
        await cancellation;
      } finally { await session.close(); }
    }
    await cancellation;
  });
}

test('server shutdown waits for an accepted response before application cleanup', async t => {
  let release!: () => void;
  const accepted = new Promise<void>(resolve => { release = resolve; });
  let finish!: () => void;
  const server = new Http3NodeServer({ ...tls, runtimeMode: 'portable' }, (_req, res) => { finish = () => res.end('done'); release(); });
  server.listen(0); await once(server, 'listening');
  let cleaned = false;
  const shutdown = server.closeOnSignals({ exit: false, timeoutMs: 2000, onClose: () => { cleaned = true; } });
  t.after(shutdown.dispose);
  const session = await connectAsync(`https://localhost:${server.address()!.port}`, { ca, runtimeMode: 'portable' });
  t.after(async () => session.close());
  const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
  await accepted;
  const closing = shutdown.close();
  assert.equal(cleaned, false);
  finish();
  const chunks: Buffer[] = []; for await (const chunk of stream) chunks.push(chunk as Buffer);
  assert.equal(Buffer.concat(chunks).toString(), 'done');
  await closing;
  assert.equal(cleaned, true);
  assert.equal(server.listening, false);
});

test('proxy returns 502 on invalid rewritten paths and upstream inactivity', async t => {
  const upstream = createServer(() => {});
  upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
  t.after(async () => { upstream.closeAllConnections(); await new Promise<void>(resolve => upstream.close(() => resolve())); });
  const target = `http://127.0.0.1:${(upstream.address() as { port: number }).port}`;
  for (const rewrite of [() => '//another-host/', () => '/invalid space', undefined]) {
    const server = new Http3NodeServer({ ...tls, allowHTTP1: true, runtimeMode: 'portable' }, proxy(target, { rewrite, timeoutMs: 20 }));
    server.listen(0); await once(server, 'listening');
    try {
      await new Promise<void>((resolve, reject) => {
        get(`https://localhost:${server.address()!.port}`, { ca, agent: false }, response => {
          assert.equal(response.statusCode, 502); response.resume(); response.once('end', resolve);
        }).once('error', reject);
      });
    } finally { await new Promise<void>(resolve => server.close(() => resolve())); }
  }
});
