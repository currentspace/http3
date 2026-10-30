import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { connect as connectH2 } from 'node:http2';
import { request as requestHttps } from 'node:https';
import { Readable } from 'node:stream';
import express from 'express';
import { createExpressHandler, createExpressAdapter } from '../../lib/express-adapter.js';
import { Http3NodeServer, Http3IncomingMessage, Http3ServerResponse } from '../../lib/node-server.js';
import { ServerHttp3Stream } from '../../lib/stream.js';
import { connectAsync } from '../../lib/client.js';
import { createSecureServer } from '../../lib/server.js';
import { generateMutualTlsTestCerts } from '../support/generate-certs.js';

test('Express preparation preserves descriptors, symbols, and existing own overrides across prototype replacement', () => {
  const stream = new ServerHttp3Stream();
  const req = new Http3IncomingMessage(stream, {}, { endStream: true });
  const res = new Http3ServerResponse(req);
  const originalWrite = res.write.bind(res);
  Object.defineProperty(res, 'write', { value: originalWrite, configurable: false });
  const expected = new Map<object, Map<PropertyKey, PropertyDescriptor>>();
  for (const instance of [req, res]) {
    const descriptors = new Map<PropertyKey, PropertyDescriptor>();
    for (let proto: object | null = Object.getPrototypeOf(instance) as object; proto && proto !== Object.prototype; proto = Object.getPrototypeOf(proto) as object | null) {
      for (const key of Reflect.ownKeys(proto)) {
        if (key === 'constructor' || descriptors.has(key) || Object.hasOwn(instance, key)) continue;
        descriptors.set(key, { ...Object.getOwnPropertyDescriptor(proto, key), configurable: true });
      }
    }
    expected.set(instance, descriptors);
  }
  const handle = createExpressHandler((request, response) => {
    for (const instance of [request, response]) {
      Object.setPrototypeOf(instance, Object.prototype);
      for (const [key, descriptor] of expected.get(instance)!) {
        assert.deepEqual(Object.getOwnPropertyDescriptor(instance, key), descriptor);
      }
    }
  });
  handle(req as unknown as Parameters<typeof handle>[0], res as unknown as Parameters<typeof handle>[1]);
  assert.equal(Object.getOwnPropertyDescriptor(res, 'write')?.value, originalWrite);
  res.setHeader('x-test', 'preserved');
  assert.equal(res.getHeader('x-test'), 'preserved');
  req.destroy(); res.destroy(); stream.destroy();
});

for (const protocol of ['h3', 'h2', 'h1'] as const) {
  test(`real Express: ${protocol} helpers, parsers, mounted apps and streaming`, async t => {
    const certs = generateMutualTlsTestCerts();
    const app = express();
    const child = express();
    app.use(express.json());
    child.post('/echo', (req, res) => {
      res.cookie('session', 'demo', { httpOnly: true, secure: true });
      res.status(201).json({ body: req.body as unknown, version: req.httpVersion, secure: req.secure, path: req.originalUrl });
    });
    app.use('/child', child);
    app.get('/stream', (_req, res) => {
      res.type('text/plain');
      Readable.from(['one', 'two', 'three']).pipe(res);
    });
    app.get('/error', () => { throw new Error('example'); });
    app.use((_err: Error, _req: express.Request, res: express.Response, _next: express.NextFunction) => {
      res.status(418).json({ handled: true });
    });
    const server = new Http3NodeServer({ ...certs.server, allowHTTP1: true, runtimeMode: 'portable' }, createExpressHandler(app));
    server.listen({ port: 0, host: '127.0.0.1' });
    await once(server, 'listening');
    const origin = `https://127.0.0.1:${server.address()!.port}`;
    const tls = { ca: certs.ca.cert, servername: 'localhost' };
    const session = protocol === 'h3' ? await connectAsync(origin, { ...tls, runtimeMode: 'portable' })
      : protocol === 'h2' ? connectH2(origin, tls) : undefined;
    if (protocol === 'h2') await once(session!, 'connect');
    t.after(async () => {
      await session?.close();
      await new Promise<void>((resolve, reject) => server.close(err => err ? reject(err) : resolve()));
    });
    async function request(path: string, method = 'GET', body?: string) {
      const headers: Record<string, string> = body ? { 'content-type': 'application/json' } : {};
      if (!session) {
        const req = requestHttps(origin + path, { ...tls, method, headers, agent: false });
        const response = once(req, 'response');
        req.end(body);
        const [res] = await response as [import('node:http').IncomingMessage];
        const chunks: Buffer[] = [];
        for await (const chunk of res) chunks.push(chunk as Buffer);
        return { status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString() };
      }
      const stream = session.request({ ':method': method, ':path': path, ':scheme': 'https', ':authority': 'localhost', ...headers }, { endStream: body === undefined });
      const response = once(stream, 'response');
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => chunks.push(chunk));
      const end = once(stream, 'end');
      const writable: import('node:stream').Writable = stream;
      if (body) writable.end(body);
      await end;
      const [responseHeaders] = await response as [Record<string, string>];
      return { status: Number(responseHeaders[':status']), headers: responseHeaders, body: Buffer.concat(chunks).toString() };
    }
    const echo = await request('/child/echo', 'POST', '{"hello":"world"}');
    assert.equal(echo.status, 201);
    assert.deepEqual(JSON.parse(echo.body), { body: { hello: 'world' }, version: protocol === 'h1' ? '1.1' : protocol === 'h2' ? '2.0' : '3.0', secure: true, path: '/child/echo' });
    assert.match(String(echo.headers['set-cookie']), /session=demo/);
    assert.equal((await request('/stream')).body, 'onetwothree');
    assert.equal((await request('/stream', 'HEAD')).body, '');
    assert.equal((await request('/error')).status, 418);
    assert.equal((await request('/missing')).status, 404);
  });
}

test('stream-level Express adapter supports real res.json()', async t => {
  const certs = generateMutualTlsTestCerts();
  const app = express();
  app.get('/', (_req, res) => { res.json({ realExpress: true }); });
  const server = createSecureServer({ ...certs.server, runtimeMode: 'portable' }, createExpressAdapter(app as unknown as Parameters<typeof createExpressAdapter>[0]));
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const session = await connectAsync(`https://127.0.0.1:${server.address()!.port}`, {
    ca: certs.ca.cert, servername: 'localhost', runtimeMode: 'portable',
  });
  t.after(async () => { await session.close(); await server.close(); });
  const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
  const chunks: Buffer[] = [];
  for await (const chunk of stream) chunks.push(chunk as Buffer);
  assert.deepEqual(JSON.parse(Buffer.concat(chunks).toString()), { realExpress: true });
});
