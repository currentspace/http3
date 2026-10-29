import assert from 'node:assert/strict';
import { test } from 'node:test';
import { once } from 'node:events';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { request as httpsRequest } from 'node:https';
import { connect as connectH2 } from 'node:http2';
import { spawn } from 'node:child_process';
import { connectAsync } from '@currentspace/http3';
import { createApp } from '../dist/bootstrap.js';

const tls = {
  key: readFileSync(process.env.TLS_KEY_FILE ?? '.certs/key.pem'),
  cert: readFileSync(process.env.TLS_CERT_FILE ?? '.certs/cert.pem'),
};
const hash = body => createHash('sha256').update(body).digest('hex');
async function collect(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
}

for (const [protocol, version] of [['h3', '3.0'], ['h2', '2.0'], ['h1', '1.1']]) {
  test(`${protocol}: Nest controller lifecycle over TLS (Node ${process.version})`, async t => {
    const app = await createApp(tls);
    let session;
    t.after(async () => {
      if (protocol === 'h2') session?.destroy();
      else await session?.close();
      await app.close();
      assert.equal(app.getHttpServer().listening, false);
    });
    await app.listen(0, '127.0.0.1');
    const port = app.getHttpServer().address().port;
    const origin = `https://127.0.0.1:${port}`;
    const clientTls = { ca: tls.cert, servername: 'localhost' };
    if (protocol === 'h3') {
      session = await connectAsync(origin, { ...clientTls, runtimeMode: 'portable' });
    } else if (protocol === 'h2') {
      session = connectH2(origin, clientTls);
      await once(session, 'connect');
      assert.equal(session.alpnProtocol, 'h2');
    }

    async function open(path, method = 'GET', headers = {}, body) {
      if (protocol === 'h1') {
        const req = httpsRequest(origin + path, { ...clientTls, method, headers, agent: false });
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
      const response = await request('/');
      assert.equal(response.status, 200);
      assert.deepEqual(JSON.parse(response.body), { message: 'Hello, NestJS', httpVersion: version });
      assert.equal(response.headers['x-example-middleware'], 'nestjs');
      assert.equal(response.headers['alt-svc'], `h3=":${port}"; ma=60`);
    });
    await t.test('JSON parser and raw-body signature input', async () => {
      const body = Buffer.from('{ "hello": "world", "n": 42 }');
      const response = await request('/echo', 'POST', { 'content-type': 'application/json' }, body);
      assert.equal(response.status, 201);
      assert.deepEqual(JSON.parse(response.body), {
        body: { hello: 'world', n: 42 }, httpVersion: version, rawBodySha256: hash(body),
      });
    });
    await t.test('parameter pipe accepts integers and rejects invalid input', async () => {
      assert.deepEqual(JSON.parse((await request('/items/42')).body), { id: 42 });
      assert.equal((await request('/items/no')).status, 400);
    });
    await t.test('exception filter and missing routes', async () => {
      const response = await request('/forbidden');
      assert.equal(response.status, 403);
      assert.equal(JSON.parse(response.body).message, 'Example denial');
      assert.equal((await request('/missing')).status, 404);
    });
    await t.test('malformed JSON and declared body-size limit', async () => {
      assert.equal((await request('/echo', 'POST', { 'content-type': 'application/json' }, '{')).status, 400);
      // Content-Length lets the parser reject before exceeding the transport's
      // separate unread-byte limit; neither limit should be silently disabled.
      assert.equal((await request('/echo', 'POST', {
        'content-type': 'application/json', 'content-length': String(1024 * 1024 + 1),
      // HTTP/1 rejects the declared size before reading a body and closes the
      // connection; don't race that early response with a megabyte socket write.
      }, protocol === 'h1' ? undefined : Buffer.alloc(1024 * 1024 + 1, 32))).status, 413);
      assert.equal((await request('/')).status, 200);
    });
    await t.test('CORS preflight', async () => {
      const response = await request('/echo', 'OPTIONS', {
        origin: 'https://example.com', 'access-control-request-method': 'POST',
      });
      assert.equal(response.status, 204);
      assert.equal(response.headers['access-control-allow-origin'], 'https://example.com');
      assert.equal(response.body.length, 0);
    });
    await t.test('StreamableFile preserves all bytes and headers', async () => {
      const response = await request('/download');
      const expected = Buffer.concat(Array.from({ length: 16 }, (_, i) => Buffer.alloc(16384, i)));
      assert.equal(response.status, 200);
      assert.equal(response.headers['content-type'], 'application/octet-stream');
      assert.match(response.headers['content-disposition'], /sample\.bin/);
      assert.equal(response.body.length, expected.length);
      assert.equal(hash(response.body), hash(expected));
      assert.equal((await request('/download', 'HEAD')).body.length, 0);
    });
    await t.test('SSE delivers before EOF and cancellation leaves the connection usable', async () => {
      const { stream, headers } = await open('/events');
      assert.match(headers['content-type'], /text\/event-stream/);
      let first = '';
      for await (const chunk of stream) {
        first += chunk.toString();
        if (first.includes('"id":0')) break;
      }
      assert.ok(!first.includes('"id":2'), 'SSE must deliver before the final event');
      stream.destroy();
      assert.equal((await request('/')).status, 200);
      const complete = await request('/events');
      for (const id of [0, 1, 2]) assert.ok(complete.body.includes(`"id":${id}`));
    });
  });
}

test('CLI responds to SIGTERM through Nest shutdown hooks', async () => {
  const child = spawn(process.execPath, ['dist/main.js'], {
    env: { ...process.env, PORT: '0' }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '';
  child.stderr.on('data', chunk => { logs += chunk; });
  const exited = once(child, 'exit');
  let forced = false;
  const deadline = setTimeout(() => { forced = true; child.kill('SIGKILL'); }, 10000);
  try {
    for await (const chunk of child.stdout) {
      logs += chunk;
      if (logs.includes('Ready: https://localhost:')) {
        child.kill('SIGTERM');
        break;
      }
    }
    const [code, signal] = await exited;
    assert.ok(logs.includes('Ready: https://localhost:'), logs);
    assert.equal(forced, false, logs);
    assert.ok(code === 0 || signal === 'SIGTERM', `Unexpected shutdown: ${code}/${signal}\n${logs}`);
  } finally {
    clearTimeout(deadline);
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  }
});
