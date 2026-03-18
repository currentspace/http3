/**
 * Worker mode tests: edge cases and error paths.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect } from '../../lib/index.js';
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

describe('Worker Edge Cases', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('server close with in-flight request', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      // Intentionally delay response
      stream.respond({ ':status': '200' });
      stream.write('partial');
      // Don't end — leave in-flight
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET',
      ':path': '/inflight',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    // Wait for partial data
    let gotData = false;
    reqStream.on('data', () => { gotData = true; });
    await waitFor(() => gotData, 3000);

    // Close server while request is in-flight — should not hang
    await server.close();
    await session.close();
  });

  it('stream reset mid-transfer — Fix 2 (destroy with error)', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      // Write a large amount — client will reset mid-transfer
      const big = Buffer.alloc(256 * 1024, 'R');
      stream.write(big);
      stream.on('error', () => { /* expected */ });
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET',
      ':path': '/reset',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    // Get some data then destroy (simulates client reset)
    let gotSomeData = false;
    reqStream.on('data', () => { gotSomeData = true; });
    await waitFor(() => gotSomeData, 3000);
    reqStream.destroy();

    // Give time for reset to propagate
    await new Promise<void>((resolve) => { setTimeout(resolve, 500); });

    await session.close();
    await server.close();
  });

  it('client disconnect mid-transfer', { timeout: 10000 }, async () => {
    let serverSawClose = false;
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      // Slow streaming — client will disconnect
      const interval = setInterval(() => {
        if (stream.destroyed) {
          clearInterval(interval);
          return;
        }
        stream.write('chunk');
      }, 50);
      stream.on('error', () => { clearInterval(interval); });
      stream.on('close', () => { clearInterval(interval); });
    });
    server.on('session', (sess) => {
      sess.on('close', () => { serverSawClose = true; });
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET',
      ':path': '/disconnect',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let gotData = false;
    reqStream.on('data', () => { gotData = true; });
    await waitFor(() => gotData, 3000);

    // Abruptly close client session
    await session.close();

    // Server should see the session close — may take up to a few seconds
    // for the CONNECTION_CLOSE to propagate and quiche to drain
    await waitFor(() => serverSawClose, 8000);
    assert.ok(serverSawClose, 'server should see session close after client disconnect');

    await server.close();
  });

  it('double listen throws', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' }, { endStream: true });
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

  it('maxConnections enforcement', { timeout: 10000 }, async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
    }, { maxConnections: 2 });

    // Connect 2 clients — should work
    const c1 = await connectClient(port);
    const c2 = await connectClient(port);

    const r1 = await new Promise<string>((resolve, reject) => {
      const s = c1.request({
        ':method': 'GET', ':path': '/1', ':authority': 'localhost', ':scheme': 'https',
      }, { endStream: true });
      const chunks: Buffer[] = [];
      const timeout = setTimeout(() => reject(new Error('timeout')), 5000);
      s.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      s.on('end', () => { clearTimeout(timeout); resolve(Buffer.concat(chunks).toString()); });
    });
    assert.strictEqual(r1, 'ok');

    const r2 = await new Promise<string>((resolve, reject) => {
      const s = c2.request({
        ':method': 'GET', ':path': '/2', ':authority': 'localhost', ':scheme': 'https',
      }, { endStream: true });
      const chunks: Buffer[] = [];
      const timeout = setTimeout(() => reject(new Error('timeout')), 5000);
      s.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      s.on('end', () => { clearTimeout(timeout); resolve(Buffer.concat(chunks).toString()); });
    });
    assert.strictEqual(r2, 'ok');

    // 3rd client — should fail to connect (no handshake complete)
    const c3 = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let c3Connected = false;
    c3.on('connect', () => { c3Connected = true; });

    // Wait a bit — c3 should NOT connect
    await new Promise<void>((resolve) => { setTimeout(resolve, 2000); });
    assert.ok(!c3Connected, '3rd client should not connect when maxConnections=2');

    await c1.close();
    await c2.close();
    await c3.close();
    await server.close();
  });

  it('respond() called twice is no-op', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      // Second call should be silently ignored
      stream.respond({ ':status': '500' });
      stream.end('first wins');
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET', ':path': '/double', ':authority': 'localhost', ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    const chunks: Buffer[] = [];
    let complete = false;

    reqStream.on('response', (h: Record<string, string>) => { status = h[':status'] ?? ''; });
    reqStream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    reqStream.on('end', () => { complete = true; });

    await waitFor(() => complete, 3000);
    assert.strictEqual(status, '200');
    assert.strictEqual(Buffer.concat(chunks).toString(), 'first wins');

    await session.close();
    await server.close();
  });

  it('write after end errors', async () => {
    let gotWriteError = false;
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('done');
      // Catch the error event from write-after-end
      stream.on('error', (err) => {
        assert.ok(err.message.includes('write after end') || (err as NodeJS.ErrnoException).code === 'ERR_STREAM_WRITE_AFTER_END');
        gotWriteError = true;
      });
      // Write after end should emit an error
      stream.write('too late');
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET', ':path': '/writeafterend', ':authority': 'localhost', ':scheme': 'https',
    }, { endStream: true });

    let complete = false;
    reqStream.on('data', () => {});
    reqStream.on('end', () => { complete = true; });

    await waitFor(() => complete, 3000);
    assert.ok(gotWriteError, 'server should see write-after-end error');

    await session.close();
    await server.close();
  });
});
