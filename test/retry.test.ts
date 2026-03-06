/**
 * Tests for stateless retry token flow.
 * Verifies that retry is transparent to the client when enabled.
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

describe('Retry Flow', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should complete handshake with retry enabled (default)', async () => {
    // Server with retry ON (default — disableRetry not set)
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      // disableRetry is NOT set — defaults to false (retry enabled)
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('retry works');
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

    // Client connects — quiche handles retry transparently
    const session = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
    });

    let connected = false;
    session.on('connect', () => { connected = true; });

    await waitFor(() => connected, 5000);
    assert.ok(connected, 'client should connect even with retry enabled');

    // Verify request works after retry handshake
    const stream = session.request({
      ':method': 'GET',
      ':path': '/test',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let complete = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { complete = true; });

    await waitFor(() => complete, 3000);
    assert.strictEqual(body, 'retry works');

    await session.close();
    await server.close();
  });

  it('should work with retry explicitly disabled', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('no retry');
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

    const session = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
    });

    let connected = false;
    session.on('connect', () => { connected = true; });

    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let complete = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { complete = true; });

    await waitFor(() => complete, 3000);
    assert.strictEqual(body, 'no retry');

    await session.close();
    await server.close();
  });
});
