import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer as createTcpServer, connect as connectTcp } from 'node:net';
import { connect as connectTls } from 'node:tls';
import { createServer } from 'node:http';
import { get } from 'node:https';
import { connect as connectH2 } from 'node:http2';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createDevTls } from '../../lib/dev-cert.js';
import { Http3NodeServer } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import { proxy } from '../../lib/proxy.js';

let directory: string;
let tls: ReturnType<typeof createDevTls>;
before(() => { directory = mkdtempSync(join(tmpdir(), 'http3-resilience-')); tls = createDevTls({ directory }); });
after(() => rmSync(directory, { recursive: true, force: true }));

test('H2 resets and invalid prefaces are connection errors, not uncaught server errors', async t => {
  const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode: 'portable' }, (_req, res) => res.end('still alive'));
  t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
  server.listen(0); await once(server, 'listening');
  const port = server.address()!.port;
  // No application sessionError/error handler: the listener must survive.
  for (const kind of ['reset', 'bad-preface']) {
    const tcp = connectTcp({ port, host: '127.0.0.1' });
    await once(tcp, 'connect');
    const client = connectTls({ socket: tcp, servername: 'localhost', ca: tls.ca, ALPNProtocols: ['h2'] });
    await once(client, 'secureConnect');
    const closed = new Promise<void>(resolve => client.once('close', () => resolve()));
    client.on('error', () => {});
    client.resume();
    tcp.once('close', () => client.destroy());
    if (kind === 'reset') tcp.resetAndDestroy();
    else client.end('not an HTTP/2 connection preface\r\n');
    await closed;
  }
  const session = connectH2(`https://localhost:${port}`, { ca: tls.ca });
  await once(session, 'connect');
  const stream = session.request({ ':path': '/' });
  const chunks: Buffer[] = []; for await (const chunk of stream) chunks.push(chunk as Buffer);
  assert.equal(Buffer.concat(chunks).toString(), 'still alive');
  session.close(); await once(session, 'close');
});

for (const protocol of ['h1', 'h2', 'h3'] as const) {
  for (const trigger of ['response-destroy', 'upstream-abort'] as const) {
    test(`${protocol}: ${trigger} closes an idle SSE response`, async t => {
      let cancel: () => void = () => assert.fail('response did not start');
      const upstream = createServer((_req, res) => {
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        res.write('data: ready\n\n');
        cancel = () => res.socket!.destroy();
      });
      upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
      t.after(async () => new Promise<void>(resolve => upstream.close(() => resolve())));
      const upstreamPort = (upstream.address() as { port: number }).port;
      const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, allowHTTP1: true, runtimeMode: 'portable' },
        trigger === 'upstream-abort' ? proxy(`http://127.0.0.1:${upstreamPort}`) : (_req, res) => {
          res.writeHead(200, { 'content-type': 'text/event-stream' });
          res.write('data: ready\n\n'); cancel = () => res.destroy();
        });
      server.listen(0); await once(server, 'listening');
      t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
      const origin = `https://localhost:${server.address()!.port}`;
      if (protocol === 'h1') {
        await new Promise<void>((resolve, reject) => {
          get(origin, { ca: tls.ca, agent: false }, response => {
            response.on('error', () => {});
            response.once('data', () => cancel());
            response.once('close', () => { assert.equal(response.complete, false); resolve(); });
          }).once('error', reject);
        });
      } else {
        const session = protocol === 'h2' ? connectH2(origin, { ca: tls.ca }) : await connectAsync(origin, { ca: tls.ca, runtimeMode: 'portable' });
        try {
          const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
          stream.on('error', () => {});
          const closed = new Promise<void>(resolve => stream.once('close', resolve));
          stream.once('data', () => cancel()); stream.resume();
          await closed;
          assert.ok(stream.destroyed);
        } finally { await session.close(); }
      }
    });
  }
}


test('proxy bounds an upstream TLS handshake even when SSE inactivity timeout is disabled', async t => {
  const sockets = new Set<import('node:net').Socket>();
  const upstream = createTcpServer(socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
  upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
  t.after(async () => { for (const socket of sockets) socket.destroy(); await new Promise<void>(resolve => upstream.close(() => resolve())); });
  const port = (upstream.address() as { port: number }).port;
  const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, allowHTTP1: true, runtimeMode: 'portable' },
    proxy(`https://127.0.0.1:${port}`, { ca: tls.ca, timeoutMs: 0, connectTimeoutMs: 50 }));
  server.listen(0); await once(server, 'listening');
  t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
  await new Promise<void>((resolve, reject) => {
    get(`https://localhost:${server.address()!.port}`, { ca: tls.ca, agent: false }, response => {
      assert.equal(response.statusCode, 502); response.resume(); response.once('end', resolve);
    }).once('error', reject);
  });
});
