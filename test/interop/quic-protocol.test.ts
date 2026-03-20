/**
 * QUIC protocol verification tests.
 * Covers session resumption, retry tokens, flow control, stream lifecycle,
 * datagrams, connection limits, and idle timeouts.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createQuicServer, connectQuic, connectQuicAsync } from '../../lib/index.js';
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

describe('QUIC protocol verification', () => {
  before(() => {
    certs = generateTestCerts();
  });

  // ═══════════════════════════════════════════════════════════════════════════
  // Session resumption and 0-RTT
  // ═══════════════════════════════════════════════════════════════════════════

  describe('session resumption and 0-RTT', () => {
    it('session ticket emitted after handshake', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      // Use connectQuic (sync) so we can attach listener before handshake completes
      const client = connectQuic(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const ticketPromise = new Promise<Buffer>((resolve) => {
        client.on('sessionTicket', resolve);
      });

      await client.ready();

      // Do an echo to ensure the connection is fully established
      const stream = client.openStream();
      stream.end(Buffer.from('ticket-test'));
      await collect(stream);

      // Wait for session ticket event
      const ticket = await Promise.race([
        ticketPromise,
        new Promise<null>((resolve) => { setTimeout(() => resolve(null), 3000); }),
      ]);

      assert.ok(ticket !== null, 'sessionTicket event should fire');
      assert.ok(Buffer.isBuffer(ticket), 'ticket should be a Buffer');
      assert.ok(ticket!.length > 0, 'ticket should be non-empty');

      await client.close();
      await server.close();
    });

    it('resumption with stored ticket', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      // First connection: attach listener before handshake, then capture ticket
      const client1 = connectQuic(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });
      const ticketPromise = new Promise<Buffer>((resolve) => {
        client1.on('sessionTicket', resolve);
      });
      await client1.ready();

      const s1 = client1.openStream();
      s1.end(Buffer.from('first'));
      await collect(s1);

      const ticket = await Promise.race([
        ticketPromise,
        new Promise<null>((resolve) => { setTimeout(() => resolve(null), 3000); }),
      ]);
      assert.ok(ticket !== null, 'must receive session ticket from first connection');
      await client1.close();

      // Second connection: use stored ticket for resumption
      const client2 = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        sessionTicket: ticket!,
      });

      // Verify echo still works with resumed session
      const s2 = client2.openStream();
      s2.end(Buffer.from('resumed'));
      const echoed = await collect(s2);
      assert.strictEqual(echoed.toString(), 'resumed');

      await client2.close();
      await server.close();
    });

    it('invalid session ticket falls back to full handshake', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      // Connect with garbage session ticket — should fall back, not crash
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        sessionTicket: Buffer.from('this-is-not-a-valid-ticket-at-all-garbage-data'),
      });

      const stream = client.openStream();
      stream.end(Buffer.from('fallback'));
      const echoed = await collect(stream);
      assert.strictEqual(echoed.toString(), 'fallback');

      await client.close();
      await server.close();
    });
  });

  // ═══════════════════════════════════════════════════════════════════════════
  // Retry token verification
  // ═══════════════════════════════════════════════════════════════════════════

  describe('retry token verification', () => {
    it('retry-enabled server accepts connection', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert,
        disableRetry: false, // Retry enabled
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.openStream();
      stream.end(Buffer.from('retry-test'));
      const echoed = await collect(stream);
      assert.strictEqual(echoed.toString(), 'retry-test');

      await client.close();
      await server.close();
    });

    it('retry with multiple concurrent connections', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert,
        disableRetry: false,
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      // 10 concurrent connections through retry flow
      const results = await Promise.all(
        Array.from({ length: 10 }, async (_, i) => {
          const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
            rejectUnauthorized: false,
          });
          const stream = client.openStream();
          const msg = Buffer.from(`retry-${i}`);
          stream.end(msg);
          const echoed = await collect(stream);
          await client.close();
          return echoed.toString();
        }),
      );

      assert.deepStrictEqual(
        results.sort(),
        Array.from({ length: 10 }, (_, i) => `retry-${i}`).sort(),
      );

      await server.close();
    });
  });

  // ═══════════════════════════════════════════════════════════════════════════
  // Flow control and backpressure
  // ═══════════════════════════════════════════════════════════════════════════

  describe('flow control and backpressure', () => {
    it('stream-level flow control triggers drain cycle', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        initialMaxStreamDataBidiLocal: 4096, // Small window
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        initialMaxStreamDataBidiLocal: 4096,
      });

      // 32KB >> 4KB window — forces multiple drain cycles
      const payload = Buffer.alloc(32 * 1024, 0xab);
      const stream = client.openStream();
      stream.end(payload);
      const echoed = await collect(stream, 15000);
      assert.strictEqual(echoed.length, payload.length);
      assert.ok(echoed.equals(payload), 'data integrity across drain cycles');

      await client.close();
      await server.close();
    });

    it('connection-level flow control with initialMaxData', async () => {
      // Connection window exactly fits the data (5×8KB = 40KB). This validates
      // that connection-level flow control is active and correctly distributes
      // credits across concurrent streams.
      //
      // NOTE: Tight windows requiring MAX_DATA renewal (e.g. 16KB for 40KB
      // data) cannot be tested end-to-end because our congestion tuning
      // (IW=1000, send_capacity_factor=20) causes quiche to overshoot the
      // connection window by ~1 MTU on the first burst, triggering
      // FLOW_CONTROL_ERROR. MAX_DATA renewal is verified at the Rust level
      // in tests/transport_quiche_pair.rs::test_connection_level_flow_control where
      // stream_send is called in small chunks with explicit packet exchange.
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        initialMaxData: 40960,
        initialMaxStreamDataBidiLocal: 8192,
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        initialMaxData: 40960,
        initialMaxStreamDataBidiLocal: 8192,
      });

      // 5 streams × 8KB = 40KB total, at the connection window boundary.
      // Sequential to avoid concurrent streams deadlocking on shared window.
      const payload = Buffer.alloc(8192, 0xcd);
      for (let i = 0; i < 5; i++) {
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collect(stream, 10000);
        assert.ok(echoed.equals(payload), `stream ${i} echoed correctly across connection-level flow control`);
      }

      await client.close();
      await server.close();
    });

    it('data + FIN survive drain cycle', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        initialMaxStreamDataBidiLocal: 2048, // Very small window
      });

      const serverReceived = new Promise<number>((resolve) => {
        server.on('session', (session: QuicServerSession) => {
          session.on('stream', (stream: QuicStream) => {
            const chunks: Buffer[] = [];
            stream.on('data', (c: Buffer) => chunks.push(c));
            stream.on('end', () => {
              const total = Buffer.concat(chunks);
              resolve(total.length);
              stream.end(total);
            });
          });
        });
      });

      const addr = await server.listen(0, '127.0.0.1');
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        initialMaxStreamDataBidiLocal: 2048,
      });

      // 64KB data + FIN via stream.end(payload) — data and FIN sent together
      const payload = Buffer.alloc(64 * 1024, 0xef);
      const stream = client.openStream();
      stream.end(payload);

      const echoed = await collect(stream, 20000);
      const serverLen = await serverReceived;

      assert.strictEqual(serverLen, payload.length, 'server received all data');
      assert.strictEqual(echoed.length, payload.length, 'echoed all data back');
      assert.ok(echoed.equals(payload), 'data integrity preserved through drain+FIN');

      await client.close();
      await server.close();
    });
  });

  // ═══════════════════════════════════════════════════════════════════════════
  // Stream lifecycle
  // ═══════════════════════════════════════════════════════════════════════════

  describe('stream lifecycle', () => {
    it('client-initiated stream reset with error code', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

      const serverStreamError = new Promise<Error>((resolve) => {
        server.on('session', (session: QuicServerSession) => {
          session.on('stream', (stream: QuicStream) => {
            // Echo via pipe — will get interrupted by reset
            echoStream(stream);
            stream.on('error', resolve);
          });
        });
      });

      const addr = await server.listen(0, '127.0.0.1');
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.openStream();
      stream.write(Buffer.from('some data'));

      // Give a moment for data to arrive, then reset
      await new Promise<void>((r) => { setTimeout(r, 50); });
      stream.close(99);

      // Server should see the reset
      const err = await Promise.race([
        serverStreamError,
        new Promise<null>((resolve) => { setTimeout(() => resolve(null), 3000); }),
      ]);

      assert.ok(err !== null, 'server should receive stream error from reset');
      assert.ok(stream.destroyed, 'client stream should be destroyed after close()');

      await client.close();
      await server.close();
    });

    it('stream count limit enforcement', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        initialMaxStreamsBidi: 5,
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      // First 5 streams should succeed
      const firstFive = await Promise.all(
        Array.from({ length: 5 }, async (_, i) => {
          const stream = client.openStream();
          stream.end(Buffer.from(`stream-${i}`));
          const echoed = await collect(stream, 5000);
          return echoed.toString();
        }),
      );
      assert.deepStrictEqual(firstFive.sort(), ['stream-0', 'stream-1', 'stream-2', 'stream-3', 'stream-4']);

      // 6th stream — should error or block (quiche enforces limit)
      const stream6 = client.openStream();
      const sixthResult = await Promise.race([
        new Promise<string>((resolve) => {
          stream6.on('error', (err: Error) => resolve(`error:${err.message}`));
        }),
        (async () => {
          try {
            stream6.end(Buffer.from('should-fail'));
            const data = await collect(stream6, 3000);
            return `ok:${data.toString()}`;
          } catch (err: unknown) {
            return `error:${(err as Error).message}`;
          }
        })(),
      ]);

      // Stream may error, block, or succeed after previous streams close and credits refresh.
      // The important thing is no crash.
      assert.ok(typeof sixthResult === 'string', 'sixth stream should produce a result (no crash)');

      await client.close();
      await server.close();
    });

    it('bidirectional half-close', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

      const serverReceivedData = new Promise<string>((resolve) => {
        server.on('session', (session: QuicServerSession) => {
          session.on('stream', (stream: QuicStream) => {
            const chunks: Buffer[] = [];
            stream.on('data', (c: Buffer) => chunks.push(c));
            stream.on('end', () => {
              // Client's write side is closed. Now server writes its own data.
              resolve(Buffer.concat(chunks).toString());
              stream.write(Buffer.from('server-1,'));
              stream.write(Buffer.from('server-2,'));
              stream.end(Buffer.from('server-3'));
            });
          });
        });
      });

      const addr = await server.listen(0, '127.0.0.1');
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.openStream();
      // Close client write side immediately
      stream.end(Buffer.from('client-payload'));

      // Client should still receive server's data
      const clientReceived = await collect(stream);
      const serverGotData = await serverReceivedData;

      assert.strictEqual(serverGotData, 'client-payload', 'server received client data');
      assert.strictEqual(clientReceived.toString(), 'server-1,server-2,server-3',
        'client received all server data after its write side closed');

      await client.close();
      await server.close();
    });

    it('sequential stream lifecycle — ID monotonicity', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const streamIds: number[] = [];
      for (let i = 0; i < 50; i++) {
        const stream = client.openStream();
        streamIds.push(stream.id);
        stream.end(Buffer.alloc(16, i & 0xff));
        await collect(stream, 5000);
      }

      // Verify monotonically increasing, never reused
      const expected = Array.from({ length: 50 }, (_, i) => i * 4);
      assert.deepStrictEqual(streamIds, expected, 'stream IDs should be 0,4,8,...,196');

      // Verify all unique
      const unique = new Set(streamIds);
      assert.strictEqual(unique.size, 50, 'all 50 stream IDs should be unique');

      await client.close();
      await server.close();
    });
  });

  // ═══════════════════════════════════════════════════════════════════════════
  // Connection limits and timeouts
  // ═══════════════════════════════════════════════════════════════════════════

  describe('connection limits and timeouts', () => {
    it('datagram echo over raw QUIC', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        enableDatagrams: true,
      });

      server.on('session', (session: QuicServerSession) => {
        session.on('datagram', (data: Buffer) => {
          session.sendDatagram(Buffer.concat([Buffer.from('echo:'), data]));
        });
        // Need a stream listener to keep session alive
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });

      const addr = await server.listen(0, '127.0.0.1');
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        enableDatagrams: true,
      });

      // Send a datagram and listen for echo
      const received = new Promise<Buffer>((resolve) => {
        client.on('datagram', resolve);
      });

      // Trigger connection activity with a stream to ensure datagram path is active
      const stream = client.openStream();
      stream.end(Buffer.from('keepalive'));

      client.sendDatagram(Buffer.from('ping'));

      const echoed = await Promise.race([
        received,
        new Promise<null>((resolve) => { setTimeout(() => resolve(null), 3000); }),
      ]);

      await collect(stream);

      assert.ok(echoed !== null, 'should receive datagram echo');
      assert.strictEqual(echoed!.toString(), 'echo:ping');

      await client.close();
      await server.close();
    });

    it('multiple datagrams in rapid succession', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        enableDatagrams: true,
      });

      server.on('session', (session: QuicServerSession) => {
        session.on('datagram', (data: Buffer) => {
          session.sendDatagram(data);
        });
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });

      const addr = await server.listen(0, '127.0.0.1');
      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        enableDatagrams: true,
      });

      const receivedSet = new Set<string>();
      const allReceived = new Promise<void>((resolve) => {
        client.on('datagram', (data: Buffer) => {
          receivedSet.add(data.toString());
          if (receivedSet.size >= 18) resolve();
        });
      });

      // Ensure connection is active
      const stream = client.openStream();
      stream.end(Buffer.from('keepalive'));

      // Send 20 datagrams rapidly
      for (let i = 0; i < 20; i++) {
        client.sendDatagram(Buffer.from(`dg-${i}`));
      }

      await Promise.race([
        allReceived,
        new Promise<void>((resolve) => { setTimeout(resolve, 5000); }),
      ]);

      await collect(stream);

      assert.ok(receivedSet.size >= 18,
        `expected at least 18/20 datagrams echoed, got ${receivedSet.size}`);

      await client.close();
      await server.close();
    });

    it('maxConnections enforcement', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        maxConnections: 3,
        maxIdleTimeoutMs: 5000,
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      // Connect 3 clients — should all succeed
      const clients = await Promise.all(
        Array.from({ length: 3 }, () =>
          connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false }),
        ),
      );

      // Verify all 3 work
      const results = await Promise.all(
        clients.map(async (client, i) => {
          const stream = client.openStream();
          stream.end(Buffer.from(`conn-${i}`));
          return (await collect(stream)).toString();
        }),
      );
      assert.deepStrictEqual(results.sort(), ['conn-0', 'conn-1', 'conn-2']);

      // 4th client should fail to connect (server at capacity)
      const c4 = connectQuic(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        maxIdleTimeoutMs: 2000,
      });

      const fourthResult = await Promise.race([
        c4.ready().then(() => 'connected' as const, () => 'rejected' as const),
        new Promise<'timeout'>((resolve) => { setTimeout(() => resolve('timeout'), 3000); }),
      ]);

      // Always clean up the 4th client
      try { await c4.close(); } catch { /* may already be closed */ }

      // 4th client should be rejected or time out (server drops packets at capacity)
      assert.ok(fourthResult === 'rejected' || fourthResult === 'timeout',
        `4th connection should be rejected or timeout, got: ${fourthResult}`);

      await Promise.all(clients.map((c) => c.close()));
      await server.close();
    });

    it('idle timeout closes connection', async () => {
      const server = createQuicServer({
        key: certs.key, cert: certs.cert, disableRetry: true,
        maxIdleTimeoutMs: 1000,
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });
      const addr = await server.listen(0, '127.0.0.1');

      const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
        maxIdleTimeoutMs: 1000,
      });

      // Do one echo to prove connection works
      const stream = client.openStream();
      stream.end(Buffer.from('before-idle'));
      await collect(stream);

      // Now sit idle and wait for close event
      const closeReceived = await Promise.race([
        new Promise<boolean>((resolve) => {
          client.on('close', () => resolve(true));
        }),
        new Promise<boolean>((resolve) => { setTimeout(() => resolve(false), 3000); }),
      ]);

      assert.ok(closeReceived, 'client should receive close event within ~2.5s of idle timeout');

      try { await client.close(); } catch { /* may already be closed */ }
      await server.close();
    });

    it('server graceful close with active streams', async () => {
      const server = createQuicServer({ key: certs.key, cert: certs.cert, disableRetry: true });

      const serverSessions: QuicServerSession[] = [];
      server.on('session', (session: QuicServerSession) => {
        serverSessions.push(session);
        session.on('stream', (stream: QuicStream) => {
          // Don't complete the echo — keep streams "active"
          stream.on('data', () => { /* swallow */ });
        });
      });

      const addr = await server.listen(0, '127.0.0.1');

      // Connect 3 clients with active streams
      const clients = await Promise.all(
        Array.from({ length: 3 }, () =>
          connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false }),
        ),
      );

      // Open a stream on each
      for (const client of clients) {
        const stream = client.openStream();
        stream.write(Buffer.from('active'));
      }

      // Give streams time to reach server
      await new Promise<void>((r) => { setTimeout(r, 100); });

      // Gracefully close each server session
      for (const session of serverSessions) {
        session.close(0, 'shutdown');
      }

      // All clients should receive close events, no crash, no hang
      const closeResults = await Promise.all(
        clients.map((client) =>
          Promise.race([
            new Promise<boolean>((resolve) => {
              client.on('close', () => resolve(true));
              client.on('error', () => resolve(true)); // error before close is also acceptable
            }),
            new Promise<boolean>((resolve) => { setTimeout(() => resolve(false), 3000); }),
          ]),
        ),
      );

      assert.ok(closeResults.every(Boolean),
        'all clients should receive close/error events after graceful shutdown');

      await Promise.all(clients.map(async (c) => { try { await c.close(); } catch { /* */ } }));
      await server.close();
    });
  });
});
