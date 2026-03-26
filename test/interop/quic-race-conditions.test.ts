/**
 * Targeted tests for each "false alarm" claim.
 * Prove or disprove that these scenarios actually work correctly.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type { QuicServerSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { echoStream } from '../support/echo-stream.js';

let certs: { key: Buffer; cert: Buffer };

function collect(stream: QuicStream, timeoutMs = 10000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collect timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
    stream.on('error', (err: Error) => { clearTimeout(timer); reject(err); });
  });
}

describe('QUIC race condition verification', () => {
  before(() => {
    certs = generateTestCerts();
  });

  // ── Claim 1: "Data before listener — Duplex push() buffers internally" ──
  // Test: Send data so fast that DATA events arrive in the same TSFN batch
  // as NEW_STREAM. Verify no data is lost.
  it('data arriving in same batch as new-stream is not lost', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

    // Server echoes everything back, proving it received all data
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    // Rapidly open many streams and send data immediately — maximizes chance
    // of NEW_STREAM + DATA being batched together by Rust worker
    const results = await Promise.all(
      Array.from({ length: 100 }, async (_, i) => {
        const stream = client.openStream();
        const payload = Buffer.alloc(512, i & 0xff);
        stream.end(payload);
        const echoed = await collect(stream);
        return { i, payloadLen: payload.length, echoedLen: echoed.length, match: echoed.equals(payload) };
      }),
    );

    const lost = results.filter((r) => !r.match);
    assert.strictEqual(lost.length, 0,
      `${lost.length}/100 streams lost data: ${JSON.stringify(lost.slice(0, 5))}`);

    await client.close();
    await server.close();
  });

  // ── Claim 2: "NEW_SESSION/NEW_STREAM same batch — synchronous dispatch" ──
  // Test: Many clients connecting simultaneously. Each immediately sends data.
  // The NEW_SESSION and first NEW_STREAM+DATA may arrive in the same batch.
  it('session and first stream in same batch — no data loss', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');

    // 20 clients connecting simultaneously, each sending data immediately after handshake
    const results = await Promise.all(
      Array.from({ length: 20 }, async (_, i) => {
        const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });
        const stream = client.openStream();
        const payload = Buffer.from(`client-${i}-data`);
        stream.end(payload);
        const echoed = await collect(stream);
        await client.close();
        return { i, match: echoed.equals(payload) };
      }),
    );

    const lost = results.filter((r) => !r.match);
    assert.strictEqual(lost.length, 0,
      `${lost.length}/20 connections lost data from first stream`);

    await server.close();
  });

  // ── Claim 3: "_final() returns 1 — FIN actually reaches peer" ──
  // Test: Send data, call end(), verify peer sees exact data + EOF.
  // Do this under load to stress the FIN delivery path.
  it('stream.end() FIN is actually delivered to peer under load', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

    let serverFinCount = 0;
    const serverReceivedAll = new Promise<void>((resolve) => {
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => {
          const chunks: Buffer[] = [];
          stream.on('data', (c: Buffer) => chunks.push(c));
          stream.on('end', () => {
            serverFinCount++;
            // Echo back with FIN
            stream.end(Buffer.concat(chunks));
            if (serverFinCount >= 50) resolve();
          });
        });
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    let clientFinCount = 0;
    const results = await Promise.all(
      Array.from({ length: 50 }, async (_, i) => {
        const stream = client.openStream();
        stream.end(Buffer.from(`fin-test-${i}`));
        const echoed = await collect(stream);
        clientFinCount++;
        return echoed.toString();
      }),
    );

    // Wait for server to confirm all FINs received
    await Promise.race([
      serverReceivedAll,
      new Promise<void>((_, reject) => { setTimeout(() => reject(new Error('server FIN timeout')), 5000); }),
    ]);

    assert.strictEqual(clientFinCount, 50, `client received ${clientFinCount}/50 FINs`);
    assert.strictEqual(serverFinCount, 50, `server received ${serverFinCount}/50 FINs`);
    // Verify data integrity — all 50 unique values present
    const expected = new Set(Array.from({ length: 50 }, (_, i) => `fin-test-${i}`));
    const actual = new Set(results);
    assert.strictEqual(actual.size, 50, `expected 50 unique results, got ${actual.size}`);
    for (const val of expected) {
      assert.ok(actual.has(val), `missing stream data: ${val}`);
    }

    await client.close();
    await server.close();
  });

  // ── Claim 4: "FIN during flow control — flush preserves pw.fin" ──
  // Test: Send enough data to trigger flow control blocking, then send FIN.
  // Verify the FIN is eventually delivered after flow control unblocks.
  it('FIN delivered after flow-control stall', async () => {
    const server = createQuicServer({
      key: certs.key, cert: certs.cert, disableRetry: true,
      // Small initial window to force flow control
      initialMaxStreamDataBidiLocal: 8192,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        // Slowly consume — read but delay response to build backpressure
        const chunks: Buffer[] = [];
        stream.on('data', (c: Buffer) => chunks.push(c));
        stream.on('end', () => {
          const total = Buffer.concat(chunks);
          stream.end(Buffer.from(`received:${total.length}`));
        });
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      initialMaxStreamDataBidiLocal: 8192,
    });

    // Send data larger than the initial window to force flow control
    const stream = client.openStream();
    const payload = Buffer.alloc(64 * 1024, 0xab); // 64KB >> 8KB window
    stream.end(payload);

    const response = await collect(stream, 15000);
    assert.strictEqual(response.toString(), `received:${payload.length}`,
      'server should have received all data + FIN');

    await client.close();
    await server.close();
  });

  // ── Claim 5: "Drain after expired timer — connections get poll_quic_events" ──
  // Test: Keep a connection alive across many timer ticks with ongoing writes.
  // The timer fires, on_timeout() is called, then events are polled.
  it('drain events fire correctly across timer boundaries', async () => {
    const server = createQuicServer({
      key: certs.key, cert: certs.cert, disableRetry: true,
      initialMaxStreamDataBidiLocal: 4096, // Tiny window to force many drains
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      initialMaxStreamDataBidiLocal: 4096,
    });

    // Send data in chunks that will require many drain cycles
    // With 4KB window and 128KB data, this needs ~32 drain rounds
    const stream = client.openStream();
    const payload = Buffer.alloc(128 * 1024, 0xcd);
    stream.end(payload);

    const echoed = await collect(stream, 15000);
    assert.strictEqual(echoed.length, payload.length,
      `expected ${payload.length} bytes, got ${echoed.length} — drain events may have been lost`);
    assert.ok(echoed.equals(payload), 'data integrity check failed across drain boundaries');

    await client.close();
    await server.close();
  });

  // ── Additional: Verify no data loss under sustained concurrent load ──
  it('200 concurrent streams with mixed sizes — zero data loss', async () => {
    const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

    const sizes = [0, 1, 100, 1024, 4096, 16384, 65536]; // 0B to 64KB
    const results = await Promise.all(
      Array.from({ length: 200 }, async (_, i) => {
        const size = sizes[i % sizes.length];
        const stream = client.openStream();
        const payload = Buffer.alloc(size, i & 0xff);
        stream.end(payload);
        const echoed = await collect(stream);
        return {
          i,
          size,
          echoedLen: echoed.length,
          match: echoed.equals(payload),
        };
      }),
    );

    const mismatches = results.filter((r) => !r.match);
    assert.strictEqual(mismatches.length, 0,
      `${mismatches.length}/200 streams had data mismatch: ${JSON.stringify(mismatches.slice(0, 5))}`);

    await client.close();
    await server.close();
  });
});
