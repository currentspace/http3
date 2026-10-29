import { test, before } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { connect as connectH2 } from 'node:http2';
import { get as getHttps } from 'node:https';
import { Http3NodeServer, Http3IncomingMessage, Http3ServerResponse } from '../../lib/node-server.js';
import { ServerHttp3Stream } from '../../lib/stream.js';
import { connectAsync } from '../../lib/client.js';
import { generateMutualTlsTestCerts } from '../support/generate-certs.js';

let tls: { key: Buffer; cert: Buffer };
let ca: Buffer;
before(() => {
  const certs = generateMutualTlsTestCerts();
  tls = certs.server;
  ca = certs.ca.cert;
});
const close = (server: Http3NodeServer): Promise<void> => new Promise((resolve, reject) => {
  server.close(error => error ? reject(error) : resolve());
});

test('Node response validates headers and excludes hop-by-hop fields', async () => {
  const stream = new ServerHttp3Stream();
  let transmitted: unknown;
  stream.respond = headers => { transmitted = headers; };
  const req = new Http3IncomingMessage(stream, { ':method': 'GET', ':path': '/' }, { endStream: true });
  const res = new Http3ServerResponse(req);
  assert.throws(() => res.setHeader('bad\nname', 'x'));
  assert.throws(() => res.setHeader('x-test', 'bad\r\nvalue'));
  assert.throws(() => res.setHeader('x-test', undefined as never));
  res.setHeader('x-length', 12);
  res.appendHeader('set-cookie', 'one=1');
  res.appendHeader('set-cookie', 'two=2');
  res.setHeader('connection', 'x-internal');
  res.setHeader('x-internal', 'secret');
  res.setHeader('transfer-encoding', 'chunked');
  res.setHeader('x-remove', 'yes');
  res.removeHeader('X-Remove');
  res.removeHeader('date');
  res.flushHeaders();
  assert.deepEqual(transmitted, { ':status': '200', 'x-length': '12', 'set-cookie': ['one=1', 'two=2'] });
  assert.throws(() => res.setHeader('x-late', 'no'), { code: 'ERR_HTTP_HEADERS_SENT' });
  res.destroy();
  stream.destroy();
});

for (const protocol of ['h3', 'h2']) {
  test(`${protocol}: Node facade HEAD, trailers and no-content responses`, async t => {
    const server = new Http3NodeServer(tls, (req, res) => {
      res.setHeader('x-version', req.httpVersion);
      if (req.url === '/empty') { res.statusCode = 204; res.end('discard'); return; }
      if (req.url === '/cached') { res.statusCode = 304; res.end('discard'); return; }
      if (req.url === '/trailers') {
        res.write('body'); res.addTrailers({ 'x-tail': 'yes' }); res.end(); return;
      }
      res.setHeader('content-length', '4');
      res.end('body');
    });
    t.after(() => close(server));
    server.listen({ port: 0, host: '127.0.0.1' });
    await once(server, 'listening');
    const authority = `https://127.0.0.1:${server.address()!.port}`;
    const session = protocol === 'h3' ? await connectAsync(authority, { ca, servername: 'localhost' })
      : connectH2(authority, { ca });
    if (protocol === 'h2') await once(session, 'connect');
    t.after(async () => { await session.close(); });
    for (const [method, path, status, body] of [
      ['HEAD', '/', '200', ''], ['GET', '/empty', '204', ''],
      ['GET', '/cached', '304', ''], ['GET', '/trailers', '200', 'body'],
    ]) {
      const stream = session.request({ ':method': method, ':path': path, ':authority': 'localhost', ':scheme': 'https' }, { endStream: true });
      const response = once(stream, 'response');
      const tail = path === '/trailers' ? once(stream, 'trailers') : null;
      const chunks: Buffer[] = [];
      for await (const chunk of stream) chunks.push(chunk as Buffer);
      assert.equal(String((await response)[0][':status']), status);
      assert.equal(Buffer.concat(chunks).toString(), body);
      if (tail) assert.equal((await tail)[0]['x-tail'], 'yes');
    }
  });
}

test('Node facade reports listen failures and permits repeated close callbacks', async () => {
  const server = new Http3NodeServer(tls, () => {});
  const error = once(server, 'error');
  server.listen({ port: -1 });
  assert.ok((await error)[0] instanceof Error);
  await close(server);
  await close(server);
  assert.equal(server.address(), null);
});

test('Node facade close while resolving a hostname completes', async () => {
  const server = new Http3NodeServer(tls, () => {});
  server.listen({ port: 0, host: 'localhost' });
  await close(server);
  assert.equal(server.listening, false);
  assert.equal(server.address(), null);
});

test('Node facade bounds shutdown of a stalled HTTP/1 request', async t => {
  let enter!: () => void;
  const entered = new Promise<void>(resolve => { enter = resolve; });
  const server = new Http3NodeServer({ ...tls, allowHTTP1: true, shutdownTimeoutMs: 30 }, () => enter());
  t.after(() => close(server));
  server.listen({ port: 0, host: '127.0.0.1' });
  await once(server, 'listening');
  const req = getHttps({ host: '127.0.0.1', port: server.address()!.port, ca, agent: false });
  req.on('error', () => {});
  t.after(() => req.destroy());
  await entered;
  await close(server);
  assert.equal(server.address(), null);
});

test('request headers support Node property checks without prototype mutation', () => {
  const stream = new ServerHttp3Stream();
  const headers = { ':method': 'GET', ['__proto__']: 'untrusted', 'X-Test': 'yes' };
  const req = new Http3IncomingMessage(stream, headers, { endStream: true });
  assert.equal(Object.getPrototypeOf(req.headers), Object.prototype);
  assert.equal(Object.prototype.hasOwnProperty.call(req.headers, '__proto__'), true);
  assert.equal(req.headers.__proto__, 'untrusted');
  // Exercise the exact Node middleware contract that failed with null-prototype headers.
  // eslint-disable-next-line no-prototype-builtins
  assert.equal(req.headers.hasOwnProperty('x-test'), true);
  req.destroy();
  stream.destroy();
});

test('flushHeaders invokes wrapped writeHead once before committing headers', () => {
  const stream = new ServerHttp3Stream();
  let sent: Record<string, unknown> | undefined;
  stream.respond = headers => { sent = headers; };
  const req = new Http3IncomingMessage(stream, {}, { endStream: true });
  const res = new Http3ServerResponse(req);
  const writeHead = res.writeHead.bind(res);
  let calls = 0;
  res.writeHead = (...args) => {
    calls++;
    res.setHeader('x-middleware', 'present');
    return writeHead(...args);
  };
  assert.equal(res._header, null);
  res.flushHeaders();
  res.flushHeaders();
  assert.equal(calls, 1);
  assert.equal(sent?.['x-middleware'], 'present');
  assert.ok(res._header);
  res.destroy();
  stream.destroy();
});

test('public HTTP/1 upgrade forwards head and closes a half-open socket at the deadline', async t => {
  const { connect } = await import('node:tls');
  const server = new Http3NodeServer({ ...tls, allowHTTP1: true, shutdownTimeoutMs: 60 }, () => {
    assert.fail('an upgrade must not invoke the request handler');
  });
  t.after(() => close(server));
  let upgradeSocket: import('node:net').Socket | undefined;
  server.on('upgrade', (req, socket, head) => {
    assert.equal(req.url, '/hmr');
    assert.equal(req.httpVersionMajor, 1);
    assert.equal(head.toString(), 'first-frame');
    upgradeSocket = socket;
    socket.write('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n');
  });
  server.listen({ host: '127.0.0.1', port: 0 });
  await once(server, 'listening');
  const client = connect({ host: '127.0.0.1', port: server.address()!.port, servername: 'localhost', ca,
    ALPNProtocols: ['http/1.1'] });
  client.allowHalfOpen = true;
  t.after(() => client.destroy());
  await once(client, 'secureConnect');
  const reply = once(client, 'data');
  client.write('GET /hmr HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nfirst-frame');
  assert.match((await reply)[0].toString(), /101 Switching Protocols/);
  const started = Date.now();
  await close(server);
  assert.ok(Date.now() - started < 1000, 'shutdown must remain bounded after transport close starts');
  assert.equal(upgradeSocket?.destroyed, true);
  assert.equal(server.address(), null);
});

test('HTTP/1 upgrade without an owner closes the socket', async t => {
  const { connect } = await import('node:tls');
  const server = new Http3NodeServer({ ...tls, allowHTTP1: true }, () => {});
  t.after(() => close(server));
  server.listen({ host: '127.0.0.1', port: 0 });
  await once(server, 'listening');
  const client = connect({ host: '127.0.0.1', port: server.address()!.port, servername: 'localhost', ca,
    ALPNProtocols: ['http/1.1'] });
  t.after(() => client.destroy());
  await once(client, 'secureConnect');
  const closed = once(client, 'close');
  client.write('GET /hmr HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n');
  await closed;
});
