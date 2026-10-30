import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createSecureServer } from '../../lib/server.js';
import { connectAsync } from '../../lib/client.js';
import { generateDevCertificate } from '../../lib/dev-cert.js';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { wasmSkipReason } from '../support/wasm-test-helpers.js';

test('WASM HTTP server default listener accepts IPv4 and IPv6', { skip: wasmSkipReason() }, async t => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-wasm-cert-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  generateDevCertificate(directory);
  const certs = {
    server: { key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')) },
    ca: { cert: readFileSync(join(directory, 'ca.pem')) },
  };
  const server = createSecureServer({ ...certs.server, runtimeMode: 'wasm' }, stream => {
    stream.respond({ ':status': '200' });
    stream.end('wasm dual stack');
  });
  t.after(() => server.close());
  server.listen({ port: 0 });
  await once(server, 'listening');
  assert.equal(server.address()!.address, '::');
  for (const host of ['127.0.0.1', '[::1]']) {
    const session = await connectAsync(`https://${host}:${server.address()!.port}`, {
      ca: certs.ca.cert, servername: 'localhost', runtimeMode: 'wasm',
    });
    try {
      const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
      const chunks: Buffer[] = [];
      for await (const chunk of stream) chunks.push(chunk as Buffer);
      assert.equal(Buffer.concat(chunks).toString(), 'wasm dual stack');
    } finally { await session.close(); }
  }
});

test('WASM H3 server receives cancellation of an idle response', { skip: wasmSkipReason() }, async t => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-wasm-cancel-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  generateDevCertificate(directory);
  let cancelled!: () => void;
  const cancellation = new Promise<void>(resolve => { cancelled = resolve; });
  const server = createSecureServer({
    key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')), runtimeMode: 'wasm',
  }, stream => {
    stream.on('error', () => {});
    stream.once('aborted', cancelled);
    stream.respond({ ':status': '200', 'content-type': 'text/event-stream' });
  });
  t.after(() => server.close());
  server.listen(0); await once(server, 'listening');
  const session = await connectAsync(`https://localhost:${server.address()!.port}`, {
    ca: readFileSync(join(directory, 'ca.pem')), runtimeMode: 'wasm',
  });
  try {
    const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
    await once(stream, 'response');
    stream.close(0x10c);
    await cancellation;
  } finally { await session.close(); }
});
