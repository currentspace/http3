import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import type { IncomingMessage } from 'node:http';
import { request as requestHttps } from 'node:https';
import { connect as connectH2 } from 'node:http2';
import type { Writable, Readable } from 'node:stream';
import express from 'express';
import { createProxyMiddleware } from 'http-proxy-middleware';
import { createExpressHandler } from '../../lib/express-adapter.js';
import { Http3NodeServer } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import { generateMutualTlsTestCerts } from '../support/generate-certs.js';

test('one Express app handles mixed protocols, redirects and streaming proxy requests', async t => {
  const certs = generateMutualTlsTestCerts();
  const received = new Map<string, { bytes: number; cookie: unknown }>();
  const upstream = createServer((req, res) => {
    res.setHeader('content-type', 'application/octet-stream');
    res.setHeader('set-cookie', ['upstream=one; Path=/', 'second=two; Path=/']);
    const progress = { bytes: 0, cookie: req.headers.cookie };
    received.set(req.url!, progress);
    req.on('data', (chunk: Buffer) => { progress.bytes += chunk.length; });
    req.pipe(res);
  });
  upstream.listen(0, '127.0.0.1');
  await once(upstream, 'listening');
  t.after(async () => {
    upstream.closeAllConnections();
    await new Promise<void>(resolve => upstream.close(() => resolve()));
  });
  const address = upstream.address();
  assert.ok(address && typeof address !== 'string');
  const app = express();
  const child = express();
  app.use('/proxy', createProxyMiddleware({ target: `http://127.0.0.1:${address.port}` }));
  child.get('/json', (req, res) => {
    // Finish asynchronously, after requests using other protocols can enter.
    setTimeout(() => {
      res.cookie('session', 'demo');
      assert.ok(res.get('set-cookie'));
      res.json({ version: req.httpVersion, cookie: req.headers.cookie });
    }, 5);
  });
  child.get('/redirect', (_req, res) => { res.redirect(307, '/child/json'); });
  app.use('/child', child);
  const prototypes = [app.request, app.response, child.request, child.response];
  const server = new Http3NodeServer({ ...certs.server, allowHTTP1: true, runtimeMode: 'portable' }, createExpressHandler(app));
  server.listen({ port: 0, host: '127.0.0.1' });
  await once(server, 'listening');
  t.after(() => new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())));
  const origin = `https://127.0.0.1:${server.address()!.port}`;
  const tls = { ca: certs.ca.cert, servername: 'localhost' };
  const h3 = await connectAsync(origin, { ...tls, runtimeMode: 'portable' });
  const h2 = connectH2(origin, tls);
  await once(h2, 'connect');
  t.after(async () => { h2.destroy(); await h3.close(); });

  function open(protocol: string, path: string, method = 'GET') {
    const chunks: Buffer[] = [];
    let receivedFirst!: () => void;
    const first = new Promise<void>(resolve => { receivedFirst = resolve; });
    const read = async (stream: Readable) => {
      stream.on('data', (chunk: Buffer) => { chunks.push(Buffer.from(chunk)); receivedFirst(); });
      await once(stream, 'end');
      return Buffer.concat(chunks);
    };
    let writable: Writable;
    let response: Promise<{ status: number; headers: Record<string, unknown> }>;
    let body: Promise<Buffer>;
    if (protocol === 'h1') {
      const req = requestHttps(origin + path, { ...tls, method, agent: false,
        headers: { cookie: ['a=1', 'b=2'], 'content-type': 'application/octet-stream' },
      });
      writable = req;
      const incoming = once(req, 'response') as Promise<[IncomingMessage]>;
      response = incoming.then(([res]) => ({ status: res.statusCode!, headers: res.headers }));
      body = incoming.then(([res]) => read(res));
    } else {
      const session = protocol === 'h3' ? h3 : h2;
      const stream = session.request({ ':method': method, ':path': path, ':authority': 'localhost',
        ':scheme': 'https', cookie: ['a=1', 'b=2'], 'content-type': 'application/octet-stream',
      }, { endStream: false });
      writable = stream;
      const incoming = once(stream, 'response') as Promise<[Record<string, unknown>]>;
      response = incoming.then(([headers]) => ({ status: Number(headers[':status']), headers }));
      body = read(stream);
    }
    // Observe failures while a test may be waiting for the streamed prefix.
    void body.catch(() => {});
    void response.catch(() => {});
    return { writable, response, body, first };
  }

  // Every round interleaves all protocols through the same mounted app.
  for (let round = 0; round < 3; round++) {
    await Promise.all(['h3', 'h2', 'h1'].map(async protocol => {
      const request = open(protocol, '/child/json');
      request.writable.end();
      assert.equal((await request.response).status, 200);
      assert.deepEqual(JSON.parse((await request.body).toString()), {
        version: protocol === 'h1' ? '1.1' : protocol === 'h2' ? '2.0' : '3.0', cookie: 'a=1; b=2',
      });
    }));
  }
  for (const protocol of ['h3', 'h2', 'h1']) {
    const redirect = open(protocol, '/child/redirect');
    redirect.writable.end();
    const response = await redirect.response;
    assert.equal(response.status, 307);
    assert.equal(response.headers.location, '/child/json');
    await redirect.body;

    const request = open(protocol, `/proxy/echo/${protocol}`, 'POST');
    const prefix = Buffer.alloc(65536, 17);
    const suffix = Buffer.alloc(256 * 1024, 42);
    request.writable.write(prefix);
    // The proxy and upstream must deliver bytes before the producer sends EOF.
    await request.first;
    const progress = received.get(`/echo/${protocol}`)!;
    assert.ok(progress.bytes > 0 && progress.bytes <= prefix.length);
    assert.equal(progress.cookie, 'a=1; b=2');
    request.writable.end(suffix);
    assert.deepEqual(await request.body, Buffer.concat([prefix, suffix]));
    assert.equal((await request.response).status, 200);
    assert.deepEqual((await request.response).headers['set-cookie'], ['upstream=one; Path=/', 'second=two; Path=/']);
  }
  const current = [app.request, app.response, child.request, child.response];
  prototypes.forEach((prototype, index) => assert.equal(current[index], prototype));
});
