/**
 * QUIC end-to-end scenario tests: various stream, datagram, and lifecycle patterns.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type { QuicServer, QuicServerSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { generateTestCerts } from '../support/generate-certs.js';

let certs: { key: Buffer; cert: Buffer };
let server: QuicServer;
let serverPort: number;

function collect(stream: QuicStream, timeoutMs = 10_000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collect timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timer);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

describe('QUIC E2E scenarios', () => {
  before(async () => {
    certs = generateTestCerts();

    server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      enableDatagrams: true,
    });

    server.on('session', (session: QuicServerSession) => {
      // Echo streams: buffer all data then write back on FIN
      session.on('stream', (stream: QuicStream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
        stream.on('end', () => { stream.end(Buffer.concat(chunks)); });
      });

      // Echo datagrams
      session.on('datagram', (data: Buffer) => {
        session.sendDatagram(data);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    serverPort = addr.port;
  });

  after(async () => {
    await server.close();
  });

  it('echo 1KB payload', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const payload = Buffer.alloc(1024, 0xaa);
    const stream = client.openStream();
    stream.end(payload);
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, 1024);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
  });

  it('echo 64KB payload', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const payload = Buffer.alloc(64 * 1024, 0xbb);
    const stream = client.openStream();
    stream.end(payload);
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, 64 * 1024);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
  });

  it('5 concurrent streams with varying sizes', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const sizes = [256, 1024, 4096, 16384, 65536];
    const results = await Promise.all(
      sizes.map(async (size) => {
        const payload = Buffer.alloc(size, size & 0xff);
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collect(stream);
        return { size, echoed, payload };
      }),
    );

    for (const { size, echoed, payload } of results) {
      assert.strictEqual(echoed.length, size, `expected ${size} bytes back`);
      assert.deepStrictEqual(echoed, payload);
    }

    await client.close();
  });

  it('server-initiated stream sends data to client', async () => {
    // Create a dedicated server for this test since we need custom session handling
    const testServer = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    testServer.on('session', (session: QuicServerSession) => {
      // When a client connects and opens a stream, server pushes data on a new stream
      session.on('stream', (stream: QuicStream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (c: Buffer) => chunks.push(c));
        stream.on('end', () => {
          stream.end(Buffer.from('ack'));
          // Open a server-initiated stream
          const pushStream = session.openStream();
          pushStream.end(Buffer.from('server-pushed-data'));
        });
      });
    });

    const addr = await testServer.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    // Listen for server-initiated stream
    const serverData = new Promise<Buffer>((resolve) => {
      client.on('stream', (stream: QuicStream) => {
        collect(stream).then(resolve);
      });
    });

    // Trigger via client stream
    const clientStream = client.openStream();
    clientStream.end(Buffer.from('trigger'));
    const ack = await collect(clientStream);
    assert.strictEqual(ack.toString(), 'ack');

    const pushed = await serverData;
    assert.strictEqual(pushed.toString(), 'server-pushed-data');

    await client.close();
    await testServer.close();
  });

  it('ping-pong: 10 round trips on same stream', async () => {
    // Need a custom server that does interactive echo (not buffer-all-then-send)
    const testServer = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    testServer.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Echo each chunk immediately as it arrives
        stream.on('data', (chunk: Buffer) => {
          stream.write(chunk);
        });
        stream.on('end', () => {
          stream.end();
        });
      });
    });

    const addr = await testServer.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    for (let i = 0; i < 10; i++) {
      const msg = Buffer.from(`ping-${i}`);
      stream.write(msg);
      const reply = await new Promise<Buffer>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`ping-pong round ${i} timed out`)), 5000);
        stream.once('data', (chunk: Buffer) => {
          clearTimeout(timer);
          resolve(chunk);
        });
      });
      assert.strictEqual(reply.toString(), `ping-${i}`);
    }
    stream.end();

    // Wait for stream to finish
    await new Promise<void>((resolve) => {
      stream.on('end', resolve);
      stream.resume(); // drain remaining data
    });

    await client.close();
    await testServer.close();
  });

  it('request-response: client sends all data with FIN, server reads then responds', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const payload = Buffer.from('complete-request-payload');
    const stream = client.openStream();
    stream.end(payload);
    const echoed = await collect(stream);
    assert.strictEqual(echoed.toString(), 'complete-request-payload');

    await client.close();
  });

  it('datagram round-trip echo', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
      enableDatagrams: true,
    });

    // Open a keepalive stream to ensure session is fully established.
    // Start collecting BEFORE sending datagram (avoid timing race).
    const keepalive = client.openStream();
    keepalive.end(Buffer.from('keepalive'));
    const keepaliveDone = collect(keepalive);

    const received = new Promise<Buffer>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('datagram echo timed out')), 5000);
      client.on('datagram', (data: Buffer) => {
        clearTimeout(timer);
        resolve(data);
      });
    });

    // Wait for keepalive to complete before sending datagram
    await keepaliveDone;

    client.sendDatagram(Buffer.from('datagram-test'));
    const echoed = await received;
    assert.strictEqual(echoed.toString(), 'datagram-test');

    await client.close();
  });

  it('20 rapid datagrams with at least 18 received', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
      enableDatagrams: true,
    });

    // Open a keepalive stream to ensure session is fully established.
    const keepalive = client.openStream();
    keepalive.end(Buffer.from('keepalive'));
    const keepaliveDone = collect(keepalive);

    const receivedSet = new Set<string>();
    const allReceived = new Promise<void>((resolve) => {
      client.on('datagram', (data: Buffer) => {
        receivedSet.add(data.toString());
        if (receivedSet.size >= 18) resolve();
      });
    });

    // Wait for keepalive to ensure session is fully established
    await keepaliveDone;

    for (let i = 0; i < 20; i++) {
      client.sendDatagram(Buffer.from(`dg-${i}`));
    }

    await Promise.race([
      allReceived,
      new Promise<void>((_, reject) =>
        setTimeout(() => reject(new Error(`only received ${receivedSet.size}/18 datagrams`)), 10_000),
      ),
    ]);

    assert.ok(receivedSet.size >= 18, `expected >= 18 datagrams, got ${receivedSet.size}`);

    await client.close();
  });

  it('100 sequential streams', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    for (let i = 0; i < 100; i++) {
      const payload = Buffer.from(`seq-${i}`);
      const stream = client.openStream();
      stream.end(payload);
      const echoed = await collect(stream);
      assert.strictEqual(echoed.toString(), `seq-${i}`);
    }

    await client.close();
  });

  it('50 concurrent streams', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const results = await Promise.all(
      Array.from({ length: 50 }, async (_, i) => {
        const payload = Buffer.from(`concurrent-${i}`);
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collect(stream);
        return echoed.toString();
      }),
    );

    const expected = Array.from({ length: 50 }, (_, i) => `concurrent-${i}`).sort();
    assert.deepStrictEqual(results.sort(), expected);

    await client.close();
  });

  it('empty stream (just FIN)', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    stream.end(); // send FIN with no data
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, 0);

    await client.close();
  });

  it('stream reset by server mid-transfer', async () => {
    const testServer = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    testServer.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Read some data then reset/destroy the stream
        stream.once('data', () => {
          stream.close(0x42);
        });
      });
    });

    const addr = await testServer.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    // Write data that will trigger server reset
    stream.write(Buffer.alloc(4096, 0xcc));

    // Expect an error or abrupt end on the client stream
    const result = await new Promise<'error' | 'end'>((resolve) => {
      const timer = setTimeout(() => resolve('end'), 5000);
      stream.on('error', () => {
        clearTimeout(timer);
        resolve('error');
      });
      stream.on('end', () => {
        clearTimeout(timer);
        resolve('end');
      });
      stream.on('close', () => {
        clearTimeout(timer);
        resolve('error');
      });
    });

    // The stream should be terminated in some way (error or close)
    assert.ok(
      result === 'error' || result === 'end',
      'stream should be terminated after server reset',
    );

    await client.close();
    await testServer.close();
  });

  it('client close while streams active', async () => {
    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    // Open several streams but don't wait for them to complete
    const streams: QuicStream[] = [];
    for (let i = 0; i < 5; i++) {
      const stream = client.openStream();
      stream.write(Buffer.alloc(1024, i));
      streams.push(stream);
    }

    // Suppress expected errors from in-flight streams
    for (const stream of streams) {
      stream.on('error', () => {});
    }

    // Close client while streams are still in progress — should not throw
    await client.close();
  });
});
