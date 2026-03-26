/**
 * HTTP/3 edge-case and error-path tests.
 * Covers server/client lifecycle, stream resets, concurrency, and limits.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect, connectAsync } from '../../lib/index.js';
import type { Http3SecureServer, Http3ClientSession, ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../lib/index.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

async function startServer(
  certs: { key: Buffer; cert: Buffer },
  handler: (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void,
  opts?: { maxConnections?: number },
): Promise<{ server: Http3SecureServer; port: number }> {
  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    ...opts,
  }, handler);

  const port = await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
    server.listen(0, '127.0.0.1');
  });

  return { server, port };
}

async function connectClient(port: number): Promise<Http3ClientSession> {
  const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
  let connected = false;
  session.on('connect', () => { connected = true; });
  await waitFor(() => connected, 3000);
  return session;
}

function collectBody(session: Http3ClientSession, path: string, timeoutMs = 5000): Promise<{ status: string; body: Buffer }> {
  return new Promise((resolve, reject) => {
    const reqStream = session.request({
      ':method': 'GET',
      ':path': path,
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collectBody timed out')), timeoutMs);

    reqStream.on('response', (h: Record<string, string>) => { status = h[':status'] ?? ''; });
    reqStream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    reqStream.on('end', () => { clearTimeout(timer); resolve({ status, body: Buffer.concat(chunks) }); });
    reqStream.on('error', (err: Error) => { clearTimeout(timer); reject(err); });
  });
}

describe('H3 edge cases', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('empty response body', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' }, { endStream: true });
    });
    const session = await connectClient(port);

    try {
      const { status, body } = await collectBody(session, '/empty');
      assert.strictEqual(status, '200');
      assert.strictEqual(body.length, 0, 'body should be empty');
    } finally {
      await session.close();
      await server.close();
    }
  });

  it('server stream reset mid-transfer', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      // Start sending a large body, then abort after a short delay
      const big = Buffer.alloc(256 * 1024, 'X');
      stream.write(big);
      stream.on('error', () => { /* expected */ });
      setTimeout(() => {
        stream.destroy(new Error('server-side abort'));
      }, 100);
    });
    const session = await connectClient(port);

    try {
      const reqStream = session.request({
        ':method': 'GET',
        ':path': '/reset-mid',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      // Client should see error, end, or close — any is fine, the key is no crash
      let gotSomeData = false;
      reqStream.on('data', () => { gotSomeData = true; });

      await new Promise<void>((resolve) => {
        const fallback = setTimeout(() => resolve(), 5000);
        reqStream.on('error', () => { clearTimeout(fallback); resolve(); });
        reqStream.on('end', () => { clearTimeout(fallback); resolve(); });
        reqStream.on('close', () => { clearTimeout(fallback); resolve(); });
      });

      // If we got here without crashing, the test passes.
      // We may or may not have received partial data depending on timing.
      assert.ok(true, 'no crash on server stream reset mid-transfer');
    } finally {
      try { await session.close(); } catch { /* may already be closed */ }
      await server.close();
    }
  });

  it('server close while requests are active', { timeout: 15000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.write('partial');
      // Don't end — leave in-flight (same pattern as worker-edge-cases)
      stream.on('error', () => { /* expected on server close */ });
    });
    const session = await connectClient(port);

    try {
      // Fire 3 requests
      const streams = ['/a', '/b', '/c'].map((path) =>
        session.request({
          ':method': 'GET',
          ':path': path,
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true }),
      );

      // Wait for partial data on at least one stream
      let gotData = false;
      for (const s of streams) {
        s.on('data', () => { gotData = true; });
        s.on('error', () => { /* expected */ });
      }
      await waitFor(() => gotData, 5000);

      // Close server while requests are still in-flight — should not hang
      await server.close();
    } finally {
      try { await session.close(); } catch { /* may already be closed */ }
    }
  });

  it('request after session close fails gracefully', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    });
    const session = await connectClient(port);

    try {
      // First request should succeed
      const { status, body } = await collectBody(session, '/before-close');
      assert.strictEqual(status, '200');
      assert.strictEqual(body.toString(), 'ok');

      // Close the session
      await session.close();

      // Attempt another request — should throw, not crash
      assert.throws(() => {
        session.request({
          ':method': 'GET',
          ':path': '/after-close',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });
      }, /not connected|closed|invalid state/i);
    } finally {
      await server.close();
    }
  });

  it('rapid connect/request/disconnect churn (10 iterations)', { timeout: 30000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('pong');
    });

    let succeeded = 0;
    try {
      for (let i = 0; i < 10; i++) {
        const session = await connectClient(port);
        const { status, body } = await collectBody(session, `/churn-${i}`);
        assert.strictEqual(status, '200');
        assert.strictEqual(body.toString(), 'pong');
        await session.close();
        succeeded++;
      }

      assert.strictEqual(succeeded, 10, `expected all 10 to succeed, got ${succeeded}`);
    } finally {
      await server.close();
    }
  });

  it('concurrent requests on same session', { timeout: 15000 }, async () => {
    const { server, port } = await startServer(certs, (stream, headers) => {
      const path = headers[':path'] as string;
      stream.respond({ ':status': '200' });
      stream.end(`reply-${path}`);
    });
    const session = await connectClient(port);

    try {
      // Fire 20 requests without awaiting
      const promises: Promise<{ status: string; body: Buffer }>[] = [];
      for (let i = 0; i < 20; i++) {
        promises.push(collectBody(session, `/concurrent-${i}`));
      }

      const results = await Promise.all(promises);
      assert.strictEqual(results.length, 20);

      for (let i = 0; i < 20; i++) {
        assert.strictEqual(results[i].status, '200');
        assert.strictEqual(results[i].body.toString(), `reply-/concurrent-${i}`);
      }
    } finally {
      await session.close();
      await server.close();
    }
  });

  it('maxConnections enforcement', { timeout: 15000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    }, { maxConnections: 2 });

    // Connect 2 clients — should work
    const c1 = await connectClient(port);
    const c2 = await connectClient(port);

    try {
      const r1 = await collectBody(c1, '/1');
      assert.strictEqual(r1.body.toString(), 'ok');

      const r2 = await collectBody(c2, '/2');
      assert.strictEqual(r2.body.toString(), 'ok');

      // 3rd client — should fail to connect (no handshake complete)
      const c3 = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
      let c3Connected = false;
      c3.on('connect', () => { c3Connected = true; });

      // Wait a bit — c3 should NOT connect
      await new Promise<void>((resolve) => { setTimeout(resolve, 2000); });
      assert.ok(!c3Connected, '3rd client should not connect when maxConnections=2');

      await c3.close();
    } finally {
      await c1.close();
      await c2.close();
      await server.close();
    }
  });
});
