import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createServer, connect } from '../lib/http2-parity.js';

describe('http2 parity surface', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('supports createServer alias and connect request flow', async () => {
    const server = createServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('parity-ok');
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
    await new Promise<void>((resolve, reject) => {
      session.once('connect', () => resolve());
      session.once('error', reject);
    });

    const stream = session.request({
      ':method': 'GET',
      ':path': '/',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    const body = await new Promise<string>((resolve, reject) => {
      let text = '';
      stream.on('data', (chunk: Buffer) => { text += chunk.toString('utf8'); });
      stream.on('end', () => resolve(text));
      stream.on('error', reject);
    });

    assert.strictEqual(body, 'parity-ok');
    await session.close();
    await server.close();
  });
});

