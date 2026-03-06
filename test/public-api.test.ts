/**
 * Phase 3 tests: Public API (createSecureServer, connect, request/respond).
 * Tests the full TypeScript layer end-to-end over real UDP.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createSecureServer, connect } from '../lib/index.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

describe('Public API', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should complete a GET request/response via createSecureServer + connect', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      const path = headers[':path'] as string | undefined;
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end(`Hello from ${path ?? '/'}\n`);
    });

    // Wait for listening
    const listenPromise = new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr, 'server should have an address');
        resolve(addr.port);
      });
    });
    server.listen(0, '127.0.0.1');
    const port = await listenPromise;

    // Connect client
    const session = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
    });

    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    // Send request
    const stream = session.request({
      ':method': 'GET',
      ':path': '/hello',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let responseStatus = '';
    let responseBody = '';
    let responseComplete = false;

    stream.on('response', (headers: Record<string, string>) => {
      responseStatus = headers[':status'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => {
      responseBody += chunk.toString();
    });
    stream.on('end', () => {
      responseComplete = true;
    });

    await waitFor(() => responseComplete, 3000);

    assert.strictEqual(responseStatus, '200');
    assert.strictEqual(responseBody, 'Hello from /hello\n');

    await session.close();
    await server.close();
  });

  it('should emit session event on the server', async () => {
    let sessionCount = 0;

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '204' }, { endStream: true });
    });

    server.on('session', (session) => {
      sessionCount++;
      assert.strictEqual(session.alpnProtocol, 'h3');
    });

    const listenPromise = new Promise<number>((resolve) => {
      server.on('listening', () => {
        const a = server.address();
        assert.ok(a);
        resolve(a.port);
      });
    });
    server.listen(0, '127.0.0.1');
    const port = await listenPromise;

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    session.request({
      ':method': 'GET', ':path': '/', ':authority': 'localhost', ':scheme': 'https',
    }, { endStream: true });

    await waitFor(() => sessionCount > 0, 2000);
    assert.ok(sessionCount >= 1);

    await session.close();
    await server.close();
  });

  it('should handle multiple concurrent requests', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      const path = headers[':path'] as string | undefined;
      stream.respond({ ':status': '200' });
      stream.end(path ?? '/');
    });

    const listenPromise = new Promise<number>((resolve) => {
      server.on('listening', () => { const a = server.address();
        assert.ok(a);
        resolve(a.port); });
    });
    server.listen(0, '127.0.0.1');
    const port = await listenPromise;

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const results: string[] = [];

    for (let i = 0; i < 3; i++) {
      const stream = session.request({
        ':method': 'GET', ':path': `/p${i}`, ':authority': 'localhost', ':scheme': 'https',
      }, { endStream: true });

      stream.on('data', (chunk: Buffer) => {
        results.push(chunk.toString());
      });
    }

    await waitFor(() => results.length >= 3, 3000);
    assert.strictEqual(results.length, 3);
    // All paths should be present (order may vary)
    assert.ok(results.includes('/p0'));
    assert.ok(results.includes('/p1'));
    assert.ok(results.includes('/p2'));

    await session.close();
    await server.close();
  });
});
