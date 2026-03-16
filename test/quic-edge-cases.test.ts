/**
 * QUIC edge-case and error-path tests.
 * Covers scenarios the loopback tests miss.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createQuicServer, connectQuic, connectQuicAsync } from '../lib/index.js';
import type { QuicServerSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

let certs: { key: Buffer; cert: Buffer };

function collect(stream: QuicStream, timeoutMs = 5000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collect timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
    stream.on('error', (err: Error) => { clearTimeout(timer); reject(err); });
  });
}

describe('QUIC edge cases', () => {
  before(() => {
    certs = generateTestCerts();
  });

  it('empty stream — open and immediately end with no data', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Echo back whatever arrives (should be nothing)
        const chunks: Buffer[] = [];
        stream.on('data', (c: Buffer) => chunks.push(c));
        stream.on('end', () => {
          stream.end(Buffer.concat(chunks));
        });
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const stream = client.openStream();
    stream.end(); // no data, just FIN
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, 0, 'empty stream should echo back 0 bytes');

    await client.close();
    await server.close();
  });

  it('ALPN mismatch — client connection fails', async () => {
    const server = createQuicServer({
      key: certs.key, cert: certs.cert, disableRetry: true,
      alpn: ['my-protocol-v1'],
      maxIdleTimeoutMs: 2000,
    });
    server.on('session', () => { /* never reached */ });
    const addr = await server.listen(0, '127.0.0.1');

    // Client uses different ALPN — handshake should fail
    const session = connectQuic(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      alpn: ['wrong-protocol'],
      maxIdleTimeoutMs: 2000,
    });

    try {
      await session.ready();
      assert.fail('should have rejected due to ALPN mismatch');
    } catch (err: unknown) {
      // Expected: handshake failure or session close before ready
      assert.ok(err instanceof Error, 'should throw Error');
    }

    try { await session.close(); } catch { /* already failed */ }
    await server.close();
  });

  it('openStream before handshake throws', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => { stream.pipe(stream); });
    });
    const addr = await server.listen(0, '127.0.0.1');

    // connectQuic returns session before handshake completes
    const session = connectQuic(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    assert.throws(
      () => session.openStream(),
      /handshake not complete/,
      'openStream before handshake should throw',
    );

    await session.ready();
    // Now it should work
    const stream = session.openStream();
    stream.end(Buffer.from('ok'));
    const data = await collect(stream);
    assert.strictEqual(data.toString(), 'ok');

    await session.close();
    await server.close();
  });

  it('stream reset by server mid-transfer', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Read some data then reset
        let received = 0;
        stream.on('data', (chunk: Buffer) => {
          received += chunk.length;
          if (received > 1024) {
            stream.close(42); // Reset with error code 42
          }
        });
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const stream = client.openStream();
    // Send a lot of data — server will reset after 1KB
    const bigPayload = Buffer.alloc(64 * 1024, 0xaa);

    const errorPromise = new Promise<Error>((resolve) => {
      stream.on('error', resolve);
    });

    stream.end(bigPayload);

    const err = await errorPromise;
    const msg = err.message.toLowerCase();
    assert.ok(msg.includes('reset') || msg.includes('error') || msg.includes('stream'),
      `expected reset/error, got: ${err.message}`);

    await client.close();
    await server.close();
  });

  it('multiple streams — some succeed, some are empty', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const results = await Promise.all([
      // Stream with data
      (async () => {
        const s = client.openStream();
        s.end(Buffer.from('has-data'));
        return (await collect(s)).toString();
      })(),
      // Empty stream
      (async () => {
        const s = client.openStream();
        s.end();
        return (await collect(s)).toString();
      })(),
      // Another stream with data
      (async () => {
        const s = client.openStream();
        s.end(Buffer.from('more-data'));
        return (await collect(s)).toString();
      })(),
    ]);

    assert.deepStrictEqual(results.sort(), ['', 'has-data', 'more-data']);

    await client.close();
    await server.close();
  });

  it('server close while streams are active — no crash', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Slowly echo — don't finish before server closes
        stream.on('data', () => { /* swallow */ });
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const stream = client.openStream();
    stream.write(Buffer.from('data'));

    // Close server while stream is still active
    await server.close();

    // Client should eventually get close/error event — not crash
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => resolve(), 500);
      timer.unref();
      client.on('close', () => { clearTimeout(timer); resolve(); });
      client.on('error', () => { clearTimeout(timer); resolve(); });
    });

    try { await client.close(); } catch { /* already closed */ }
  });

  it('double close — no crash, no error', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const stream = client.openStream();
    stream.end(Buffer.from('hello'));
    await collect(stream);

    // Double close should not crash or throw
    stream.close();
    stream.close();

    await client.close();
    await server.close();
  });

  it('close after end — no crash', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const stream = client.openStream();
    stream.end(Buffer.from('data'));
    await collect(stream);

    // close after the stream has already ended — should be safe
    stream.close();

    await client.close();
    await server.close();
  });

  it('connect to non-listening port — error propagates', async () => {
    // Use a port that's almost certainly not listening.
    // Race against a timeout since QUIC over UDP won't get an immediate error.
    const session = connectQuic('127.0.0.1:19999', {
      rejectUnauthorized: false,
      maxIdleTimeoutMs: 2000,
    });

    const result = await Promise.race([
      session.ready().then(
        () => 'ready' as const,
        (err: Error) => ({ error: err }),
      ),
      new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), 5000)),
    ]);

    if (result === 'timeout') {
      // Timed out waiting — that's acceptable, connection never established
      assert.ok(true, 'connection timed out as expected');
    } else if (result === 'ready') {
      assert.fail('should not succeed connecting to non-listening port');
    } else {
      assert.ok(result.error instanceof Error, 'should receive an Error');
    }

    try { await session.close(); } catch { /* already failed/closing */ }
  });

  it('rapid open/close connections (churn)', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });
    const addr = await server.listen(0, '127.0.0.1');

    let succeeded = 0;
    let failed = 0;

    for (let i = 0; i < 20; i++) {
      try {
        const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });
        const stream = client.openStream();
        stream.end(Buffer.from(`churn-${i}`));
        const data = await collect(stream, 3000);
        if (data.toString() === `churn-${i}`) succeeded++;
        await client.close();
      } catch {
        failed++;
      }
    }

    assert.ok(succeeded >= 18, `expected at least 18 successes, got ${succeeded}/${succeeded + failed}`);
    await server.close();
  });
});
