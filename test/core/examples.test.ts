/**
 * Smoke tests for examples — verify they work end-to-end.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect } from '../../lib/index.js';
import { serveFetch } from '../../lib/fetch-adapter.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

describe('Examples', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('serveFetch() works with a fetch handler (Hono-style)', async () => {
    const fetchHandler = (req: Request): Response => {
      const url = new URL(req.url);
      if (url.pathname === '/json') {
        return new Response(JSON.stringify({ hello: 'http3' }), {
          headers: { 'content-type': 'application/json' },
        });
      }
      return new Response(`Path: ${url.pathname}`);
    };

    const server = serveFetch({
      port: 0,
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      fetch: fetchHandler,
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    // GET /json
    const stream = session.request({
      ':method': 'GET', ':path': '/json', ':authority': 'localhost', ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let complete = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { complete = true; });
    await waitFor(() => complete, 3000);

    const parsed = JSON.parse(body) as { hello: string };
    assert.strictEqual(parsed.hello, 'http3');

    await session.close();
    await server.close();
  });

  it('Express-style adapter works with req/res pattern', async () => {
    // Simple req/res adapter that uses the stream API directly
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      // Express-style: read pseudo-headers, write response
      const method = (headers[':method'] as string | undefined) ?? 'GET';
      const path = (headers[':path'] as string | undefined) ?? '/';

      stream.respond({
        ':status': '200',
        'content-type': 'text/plain',
      });
      stream.end(`${method} ${path} handled`);
    });

    const listenPromise = new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
    });
    server.listen(0, '127.0.0.1');
    const port = await listenPromise;

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET', ':path': '/hello', ':authority': 'localhost', ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let complete = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { complete = true; });
    await waitFor(() => complete, 3000);

    assert.strictEqual(body, 'GET /hello handled');

    await session.close();
    await server.close();
  });
});
