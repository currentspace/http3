import assert from 'node:assert/strict';
import { test } from 'node:test';
import { once } from 'node:events';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { request as httpsRequest } from 'node:https';
import { connect as connectH2 } from 'node:http2';
import { startServer, port, certFile } from './harness.mjs';
import { connectAsync } from '@currentspace/http3';

const ca = readFileSync(certFile);
const hash = body => createHash('sha256').update(body).digest('hex');
async function collect(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
}

test('combined Next/Nest/Express protocols', { timeout: 60000 }, async t => {
  const server = await startServer();
  t.after(() => server.kill());
  for (const [protocol, version] of [['h3', '3.0'], ['h2', '2.0'], ['h1', '1.1']]) {
    await t.test(`${protocol}: Nest controller lifecycle over TLS (Node ${process.version})`, async t => {
      let session;
      t.after(async () => {
        if (protocol === 'h2') session?.destroy();
        else await session?.close();
      });
      const clientTls = { ca, servername: 'localhost' };
      if (protocol === 'h3') {
        session = await connectAsync(`https://127.0.0.1:${port}`, { ...clientTls, runtimeMode: 'portable' });
      } else if (protocol === 'h2') {
        session = connectH2(`https://127.0.0.1:${port}`, clientTls);
        await once(session, 'connect');
        assert.equal(session.alpnProtocol, 'h2');
      }

      async function open(path, method = 'GET', headers = {}, body) {
        if (protocol === 'h1') {
          const req = httpsRequest(`https://127.0.0.1:${port}` + path, { ...clientTls, method, headers, agent: false });
          const response = once(req, 'response');
          req.end(body);
          const [stream] = await response;
          return { stream, headers: stream.headers, status: stream.statusCode };
        }
        const stream = session.request({ ':method': method, ':path': path,
          ':scheme': 'https', ':authority': `localhost:${port}`, ...headers }, { endStream: body === undefined });
        // Retain body bytes while awaiting headers, including responses that fit
        // entirely in the same transport event batch as their headers and FIN.
        stream.pause();
        const response = once(stream, 'response');
        if (body !== undefined) stream.end(body);
        const [responseHeaders] = await response;
        return { stream, headers: responseHeaders, status: Number(responseHeaders[':status']) };
      }
      async function request(...args) {
        const response = await open(...args);
        return { ...response, body: await collect(response.stream) };
      }

      await t.test('routing, dependency injection, middleware and protocol metadata', async () => {
        const response = await request('/bff/identity');
        assert.equal(response.status, 200);
        const body = JSON.parse(response.body);
        assert.equal(body.framework, 'nestjs-express');
        assert.equal(body.httpVersion, version);
        assert.equal(body.pid, server.pid);
        assert.equal(response.headers['x-app-pid'], String(server.pid));
        assert.equal(JSON.parse((await request('/express/info')).body).pid, server.pid);
        assert.equal(JSON.parse((await request('/api/echo', 'POST', {}, 'Next body')).body).body, 'Next body');
        assert.equal(response.headers['alt-svc'], `h3=":${port}"; ma=86400`);
      });
      await t.test('JSON parser and raw-body signature input', async () => {
        const body = Buffer.from('{ "hello": "world", "n": 42 }');
        const response = await request('/bff/echo', 'POST', { 'content-type': 'application/json' }, body);
        assert.equal(response.status, 201);
        assert.deepEqual(JSON.parse(response.body), {
          body: { hello: 'world', n: 42 }, pid: server.pid, httpVersion: version, rawBodySha256: hash(body),
        });
      });
      await t.test('parameter pipe accepts integers and rejects invalid input', async () => {
        assert.deepEqual(JSON.parse((await request('/bff/items/42')).body), { id: 42 });
        assert.equal((await request('/bff/items/no')).status, 400);
      });
      await t.test('exception filter and missing routes', async () => {
        const response = await request('/bff/guarded');
        assert.equal(response.status, 403);
        assert.equal(JSON.parse(response.body).message, 'Forbidden resource');
        assert.equal((await request('/bff/missing')).status, 404);
      });
      await t.test('malformed JSON and declared body-size limit', async () => {
        assert.equal((await request('/bff/echo', 'POST', { 'content-type': 'application/json' }, '{')).status, 400);
        // Content-Length lets the parser reject before exceeding the transport's
        // separate unread-byte limit; neither limit should be silently disabled.
        assert.equal((await request('/bff/echo', 'POST', {
          'content-type': 'application/json', 'content-length': String(100 * 1024 + 1),
        }, Buffer.alloc(100 * 1024 + 1, 32))).status, 413);
        assert.equal((await request('/bff/identity')).status, 200);
      });
      await t.test('split cookies reach Nest and Next, and guards still run', async () => {
        const cookie = protocol === 'h1' ? 'demo-session=first; demo-preference=compact' : ['demo-session=first', 'demo-preference=compact'];
        const identity = JSON.parse((await request('/bff/identity', 'GET', { cookie })).body);
        assert.equal(identity.session, 'first');
        assert.equal(identity.preference, 'compact');
        assert.equal(typeof identity.cookieHeader, 'string');
        const next = JSON.parse((await request('/api/session', 'GET', { cookie })).body);
        assert.equal(next.session, 'first');
        assert.equal(next.preference, 'compact');
        assert.equal(next.pid, server.pid);
        const forwarded = JSON.parse((await request('/api/bff', 'GET', { cookie })).body);
        assert.equal(forwarded.session, 'first');
        assert.equal(forwarded.preference, 'compact');
        assert.equal(forwarded.pid, server.pid);
        assert.equal(forwarded.httpVersion, '3.0');
        assert.equal((await request('/bff/guarded', 'GET', { 'x-demo-access': 'allowed' })).status, 200);
        // Prefix ownership is segment-specific, and a BFF 404 must not render Next.
        const missing = await request('/bff/missing');
        assert.match(missing.headers['content-type'], /application\/json/);
        const page404 = await request('/bffish');
        assert.equal(page404.status, 404);
        assert.match(page404.headers['content-type'], /text\/html/);
      });
      await t.test('StreamableFile preserves all bytes and headers', async () => {
        const response = await request('/bff/download');
        const expected = Buffer.concat(Array.from({ length: 16 }, (_, i) => Buffer.alloc(16384, i)));
        assert.equal(response.status, 200);
        assert.equal(response.headers['content-type'], 'application/octet-stream');
        assert.match(response.headers['content-disposition'], /sample\.bin/);
        assert.equal(response.body.length, expected.length);
        assert.equal(hash(response.body), hash(expected));
        assert.equal((await request('/bff/download', 'HEAD')).body.length, 0);
      });
      await t.test('SSE delivers before EOF and cancellation leaves the connection usable', async () => {
        const { stream, headers } = await open('/bff/events');
        assert.match(headers['content-type'], /text\/event-stream/);
        let first = '';
        for await (const chunk of stream) {
          first += chunk.toString();
          if (first.includes('"id":0')) break;
        }
        assert.ok(!first.includes('"id":2'), 'SSE must deliver before the final event');
        stream.destroy();
        assert.equal((await request('/bff/identity')).status, 200);
        const complete = await request('/bff/events');
        for (const id of [0, 1, 2]) assert.ok(complete.body.includes(`"id":${id}`));
      });
    });
  }

  await server.stop();
});
