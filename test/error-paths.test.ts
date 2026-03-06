/**
 * Error path and edge case tests.
 * Validates guards, cleanup, and failure handling.
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

describe('Error Paths', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should throw when listen() is called twice', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    });

    await new Promise<void>((resolve) => {
      server.on('listening', () => { resolve(); });
      server.listen(0, '127.0.0.1');
    });

    assert.throws(() => {
      server.listen(0, '127.0.0.1');
    }, /already listening/);

    await server.close();
  });

  it('should reject malformed serverId when quicLb is enabled', () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      quicLb: true,
      serverId: Buffer.from([0x01, 0x02, 0x03]),
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    });

    assert.throws(() => {
      server.listen(0, '127.0.0.1');
    }, /serverId must be exactly 8 bytes/);
  });

  it('should require quicLb=true when serverId is set', () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      serverId: '0x1122334455667788',
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    });

    assert.throws(() => {
      server.listen(0, '127.0.0.1');
    }, /serverId requires quicLb=true/);
  });

  it('should accept hex serverId when quicLb is enabled', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      quicLb: true,
      serverId: '0x1122334455667788',
    }, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('quic-lb ok');
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
      ':path': '/quic-lb',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let done = false;
    stream.on('data', (chunk: Buffer) => {
      body += chunk.toString('utf8');
    });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);
    assert.strictEqual(body, 'quic-lb ok');

    await session.close();
    await server.close();
  });

  it('should throw when request() is called before handshake', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
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

    // Immediately try to request before handshake completes
    assert.throws(() => {
      session.request({
        ':method': 'GET',
        ':path': '/',
        ':authority': 'localhost',
        ':scheme': 'https',
      });
    }, /handshake not complete/);

    // Wait for connection to settle then clean up
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    await session.close();
    await server.close();
  });

  it('should transfer a large body through the TS layer', async () => {
    const receivedChunks: Buffer[] = [];

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      // Send 64KB body
      const body = Buffer.alloc(64 * 1024, 0x42);
      stream.end(body);
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
      ':path': '/large',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let complete = false;
    stream.on('data', (chunk: Buffer) => {
      receivedChunks.push(Buffer.from(chunk));
    });
    stream.on('end', () => { complete = true; });

    await waitFor(() => complete, 10000);

    const totalReceived = receivedChunks.reduce((sum, c) => sum + c.length, 0);
    assert.strictEqual(totalReceived, 64 * 1024, `expected 64KB, got ${totalReceived}`);

    // Verify content
    const fullBody = Buffer.concat(receivedChunks);
    assert.ok(fullBody.every(b => b === 0x42), 'all bytes should be 0x42');

    await session.close();
    await server.close();
  });

  it('should handle session close during active request', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      // Don't end the stream — leave it open
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
      ':path': '/hang',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let responseReceived = false;
    stream.on('response', () => { responseReceived = true; });

    await waitFor(() => responseReceived, 3000);

    // Close the session while the stream is still open
    await session.close();

    // Session should be closed, streams should be cleaned up
    // (no hanging promises or leaked resources)
    assert.ok(true, 'session closed without hanging');

    await server.close();
  });
});
