/**
 * QUIC loopback tests: raw QUIC (no HTTP/3) Node.js ↔ Node.js over localhost.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createQuicServer, connectQuicAsync } from '../lib/index.js';
import type { QuicServerSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

let certs: { key: Buffer; cert: Buffer };

function collect(stream: QuicStream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error('collect timed out')), 10000);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

describe('QUIC loopback', () => {
  before(() => {
    certs = generateTestCerts();
  });

  it('echo server — client sends, server echoes back', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');

    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    const payload = Buffer.from('hello QUIC world');
    stream.end(payload);
    const echoed = await collect(stream);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('multiple streams on one connection', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const results = await Promise.all(
      [0, 1, 2, 3, 4].map(async (i) => {
        const s = client.openStream();
        const msg = Buffer.from(`stream-${i}`);
        s.end(msg);
        const data = await collect(s);
        return data.toString();
      }),
    );

    assert.deepStrictEqual(results.sort(), ['stream-0', 'stream-1', 'stream-2', 'stream-3', 'stream-4']);

    await client.close();
    await server.close();
  });

  it('large payload transfer', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const payload = Buffer.alloc(256 * 1024, 0xab); // 256KB
    const stream = client.openStream();
    stream.end(payload);
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, payload.length);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('server-initiated streams', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    const serverPushPromise = new Promise<Buffer>((resolve) => {
      server.on('session', (session: QuicServerSession) => {
        // When client connects, server opens a stream to push data
        const pushStream = session.openStream();
        pushStream.end(Buffer.from('server push'));

        // Also listen for client streams
        session.on('stream', (stream: QuicStream) => {
          const chunks: Buffer[] = [];
          stream.on('data', (c: Buffer) => chunks.push(c));
          stream.on('end', () => {
            resolve(Buffer.concat(chunks));
            stream.end(Buffer.from('ack'));
          });
        });
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    // Receive server-initiated stream
    const serverData = await new Promise<Buffer>((resolve) => {
      client.on('stream', (stream: QuicStream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (c: Buffer) => chunks.push(c));
        stream.on('end', () => resolve(Buffer.concat(chunks)));
      });
    });

    // Also send from client
    const clientStream = client.openStream();
    clientStream.end(Buffer.from('client data'));
    const ack = await collect(clientStream);

    assert.strictEqual(serverData.toString(), 'server push');
    assert.strictEqual(ack.toString(), 'ack');

    const clientSentData = await serverPushPromise;
    assert.strictEqual(clientSentData.toString(), 'client data');

    await client.close();
    await server.close();
  });

  it('multiple concurrent connections', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');

    const clients = await Promise.all(
      Array.from({ length: 5 }, () =>
        connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false }),
      ),
    );

    const results = await Promise.all(
      clients.map(async (client, i) => {
        const stream = client.openStream();
        const msg = Buffer.from(`conn-${i}`);
        stream.end(msg);
        return (await collect(stream)).toString();
      }),
    );

    assert.deepStrictEqual(results.sort(), ['conn-0', 'conn-1', 'conn-2', 'conn-3', 'conn-4']);

    await Promise.all(clients.map((c) => c.close()));
    await server.close();
  });

  it('session metrics are available', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    stream.end(Buffer.from('metrics test'));
    await collect(stream);

    // Wait briefly for metrics to settle
    await new Promise<void>((r) => { setTimeout(r, 50); });

    const metrics = client.getMetrics();
    assert.ok(metrics, 'metrics should be available');
    assert.ok(metrics!.packetsIn > 0, 'should have received packets');
    assert.ok(metrics!.packetsOut > 0, 'should have sent packets');
    assert.ok(metrics!.bytesIn > 0, 'should have received bytes');
    assert.ok(metrics!.bytesOut > 0, 'should have sent bytes');

    await client.close();
    await server.close();
  });

  it('custom ALPN negotiation', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      alpn: ['my-custom-proto'],
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      alpn: ['my-custom-proto'],
    });

    const stream = client.openStream();
    stream.end(Buffer.from('custom alpn'));
    const echoed = await collect(stream);
    assert.strictEqual(echoed.toString(), 'custom alpn');

    await client.close();
    await server.close();
  });
});
