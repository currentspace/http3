import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../../lib/index.js';
import type { IncomingHeaders } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';

let certs: { key: Buffer; cert: Buffer };

function listen(server: ReturnType<typeof createSecureServer>): Promise<number> {
  return new Promise((resolve) => {
    server.on('listening', () => {
      const address = server.address();
      assert.ok(address);
      resolve(address.port);
    });
    server.listen(0, '127.0.0.1');
  });
}

async function collect(
  stream: NodeJS.EventEmitter,
): Promise<{ headers: IncomingHeaders; body: string; trailers: IncomingHeaders }> {
  return await new Promise((resolve, reject) => {
    let headers: IncomingHeaders | null = null;
    let trailers: IncomingHeaders = {};
    const chunks: Buffer[] = [];
    stream.on('response', (received: IncomingHeaders) => { headers = received; });
    stream.on('trailers', (received: IncomingHeaders) => { trailers = received; });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('error', reject);
    stream.on('end', () => {
      assert.ok(headers);
      resolve({
        headers,
        body: Buffer.concat(chunks).toString(),
        trailers,
      });
    });
  });
}

describe('HTTP/3 header preservation', () => {
  before(() => {
    certs = generateTestCerts();
  });

  it('preserves duplicate request headers, response headers, and trailers', async () => {
    let requestHeaders: IncomingHeaders | null = null;
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      requestHeaders = headers;
      stream.respond({
        ':status': '200',
        'set-cookie': ['a=1', 'b=2'],
        'x-repeat': ['response-a', 'response-b'],
      });
      stream.write('ok');
      stream.sendTrailers({ 'x-trailer': ['trailer-a', 'trailer-b'] });
      stream.end();
    });

    const port = await listen(server);
    const client = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    await client.ready();

    try {
      const stream = client.request({
        ':method': 'GET',
        ':path': '/headers',
        ':authority': 'localhost',
        ':scheme': 'https',
        'x-repeat': ['request-a', 'request-b'],
      }, { endStream: true });

      const response = await collect(stream);
      assert.deepStrictEqual(requestHeaders?.['x-repeat'], ['request-a', 'request-b']);
      assert.deepStrictEqual(response.headers['set-cookie'], ['a=1', 'b=2']);
      assert.deepStrictEqual(response.headers['x-repeat'], ['response-a', 'response-b']);
      assert.deepStrictEqual(response.trailers['x-trailer'], ['trailer-a', 'trailer-b']);
      assert.strictEqual(response.body, 'ok');
    } finally {
      await client.close();
      await server.close();
    }
  });
});
