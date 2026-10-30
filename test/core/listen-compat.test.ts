import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { connect as connectH2 } from 'node:http2';
import { createSecureServer } from '../../lib/server.js';
import { Http3NodeServer } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import { generateDevCertificate } from '../../lib/dev-cert.js';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { resolveConnectionEndpoint } from '../../lib/endpoint.js';

let tls: { key: Buffer; cert: Buffer };
let ca: Buffer;
let directory: string;
before(() => {
  directory = mkdtempSync(join(tmpdir(), 'http3-listen-cert-'));
  generateDevCertificate(directory);
  tls = { key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')) };
  ca = readFileSync(join(directory, 'ca.pem'));
});
after(() => { if (directory) rmSync(directory, { recursive: true, force: true }); });

for (const facade of [false, true]) {
  for (const shape of ['object', 'positional'] as const) {
    test(`${facade ? 'Node facade' : 'stream server'}: ${shape} listen accepts IPv4 and IPv6 on one port`, async t => {
      const options = { ...tls, runtimeMode: 'portable' as const };
      const server = facade ? new Http3NodeServer(options, (req, res) => {
        assert.equal(req.socket.setKeepAlive(true, 1000), req.socket);
        assert.equal(req.socket.setKeepAlive(false), req.socket);
        assert.equal(req.socket.setNoDelay(true), req.socket);
        assert.equal(req.socket.setNoDelay(false), req.socket);
        assert.equal(req.socket.setTimeout(1, () => assert.fail('socket tuning must be a no-op')), req.socket);
        res.setHeader('content-type', 'text/event-stream');
        res.write('data: first\n\n');
        setTimeout(() => res.end('data: last\n\n'), 10);
      }) : createSecureServer(options, stream => {
        stream.respond({ ':status': '200' });
        stream.end('dual stack');
      });
      t.after(async () => {
        if (server instanceof Http3NodeServer) await new Promise<void>((resolve, reject) => server.close(e => e ? reject(e) : resolve()));
        else await server.close();
      });
      let callbacks = 0;
      const listening = once(server, 'listening');
      if (shape === 'object') server.listen({ port: 0 }, () => callbacks++);
      else server.listen(0, () => callbacks++);
      await listening;
      assert.equal(callbacks, 1);
      assert.equal(server.address()!.address, '::');
      for (const host of ['127.0.0.1', '[::1]']) {
        for (const protocol of ['h3', 'h2']) {
          const origin = `https://${host}:${server.address()!.port}`;
          const session = protocol === 'h3'
            ? await connectAsync(origin, { ca, servername: 'localhost', runtimeMode: 'portable' })
            : connectH2(origin, { ca, servername: 'localhost' });
          try {
            if (protocol === 'h2') await once(session, 'connect');
            const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
            const chunks: Buffer[] = [];
            for await (const chunk of stream) chunks.push(chunk as Buffer);
            assert.equal(Buffer.concat(chunks).toString(), facade ? 'data: first\n\ndata: last\n\n' : 'dual stack');
          } finally { await session.close(); }
        }
      }
    });
  }
}

test('stream listen rejects malformed arguments before calling NAPI', () => {
  const server = createSecureServer(tls);
  for (const input of [null, [], '443', { port: '443' }, { host: 123 }, { path: '/tmp/server.sock' }]) {
    assert.throws(() => server.listen(input as never), TypeError);
  }
  for (const port of [-1, 65536, 0.5, NaN]) assert.throws(() => server.listen(port), RangeError);
  assert.equal(server.address(), null);
});

test('IPv6 URL literals bypass DNS and keep brackets in socket addresses', async () => {
  for (const endpoint of ['https://[::1]:4433', '[::1]:4433']) {
    const result = await resolveConnectionEndpoint(endpoint, {
      defaultScheme: 'https', defaultPort: 443,
      dnsLookup: async () => { throw new Error('IPv6 literals must not reach DNS'); },
    });
    assert.equal(result.host, '::1');
    assert.equal(result.family, 6);
    assert.equal(result.socketAddress, '[::1]:4433');
  }
});
