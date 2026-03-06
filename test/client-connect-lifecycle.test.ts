import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { connect, connectAsync, createSecureServer } from '../lib/index.js';
import { generateTestCerts } from './generate-certs.js';

describe('client connect lifecycle', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('supports connectAsync with custom CA and strict verification', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = await connectAsync(`127.0.0.1:${port}`, {
      ca: certs.cert,
      servername: 'localhost',
      rejectUnauthorized: true,
    });

    const stream = session.request({
      ':method': 'GET',
      ':path': '/',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    const body = await new Promise<string>((resolve, reject) => {
      let value = '';
      stream.on('data', (chunk: Buffer) => { value += chunk.toString('utf8'); });
      stream.on('end', () => resolve(value));
      stream.on('error', reject);
    });

    assert.strictEqual(body, 'ok');
    await session.close();
    await server.close();
  });

  it('exposes ready() for event-compatible connect flow', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '204' }, { endStream: true });
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    await session.ready();

    await session.close();
    await server.close();
  });
});

