/**
 * QUIC mixed workload long-haul test (5-minute sustained mixed scenarios).
 * Gated by HTTP3_LONGHAUL=1 environment variable.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type { QuicServer, QuicServerSession, QuicClientSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { generateTestCerts } from '../support/generate-certs.js';
import { LatencyTracker, pickWeighted } from '../support/scenario-helpers.js';

function formatMB(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

function collectStream(stream: QuicStream, timeoutMs = 10_000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    if (stream.destroyed) {
      reject(new Error('stream already destroyed'));
      return;
    }
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream collect timed out')), timeoutMs);
    const cleanup = (): void => { clearTimeout(timer); };
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { cleanup(); resolve(Buffer.concat(chunks)); });
    stream.on('error', (err: Error) => { cleanup(); reject(err); });
    stream.on('close', () => {
      cleanup();
      if (!stream.readableEnded) {
        reject(new Error('stream closed before end'));
      }
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

describe('QUIC mixed workload (5 minutes)', { skip: !process.env.HTTP3_LONGHAUL }, () => {
  it('sustained mixed QUIC scenario traffic with latency tracking', { timeout: 330_000 }, async () => {
    const certs = generateTestCerts();

    // Track server-push streams delivered to client
    let serverPushResolve: ((buf: Buffer) => void) | null = null;

    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 1000,
      initialMaxStreamsBidi: 1_000_000,
      maxIdleTimeoutMs: 600_000,
      initialMaxData: 2_000_000_000, // 2GB
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
        stream.on('end', () => {
          const data = Buffer.concat(chunks);
          // Check if this is a "push" command stream
          if (data.toString() === 'push') {
            // Server opens a new stream and sends 512B to client
            const pushStream = session.openStream();
            pushStream.end(Buffer.alloc(512, 0xfe));
            stream.end(Buffer.from('push-ack'));
          } else {
            // Normal echo
            stream.end(data);
          }
        });
      });

      // Echo datagrams
      session.on('datagram', (data: Buffer) => {
        session.sendDatagram(data);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const serverPort = addr.port;

    let client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
      initialMaxStreamsBidi: 1_000_000,
      maxIdleTimeoutMs: 600_000,
      initialMaxData: 4_000_000_000,
    });
    // Session may close under extreme stream churn (quiche FLOW_CONTROL_ERROR
    // when MAX_DATA credit extension doesn't arrive fast enough). The reconnect
    // logic below handles this gracefully.

    // Listen for server-initiated streams (for server-push scenario)
    client.on('stream', (stream: QuicStream) => {
      collectStream(stream).then((buf) => {
        if (serverPushResolve) {
          serverPushResolve(buf);
          serverPushResolve = null;
        }
      }).catch(() => {});
    });

    const tracker = new LatencyTracker();
    const DURATION_S = 300;
    const CHECKPOINT_INTERVAL_S = 30;
    let totalOps = 0;
    let errors = 0;
    const start = Date.now();
    let lastCheckpoint = start;

    const scenarios: Array<[string, number]> = [
      ['echo-256', 40],
      ['echo-4k', 20],
      ['echo-64k', 15],
      ['burst-5-streams', 15],
    ];

    // Scenario executors
    async function runEcho(size: number): Promise<void> {
      const payload = Buffer.alloc(size, size & 0xff);
      const stream = client.openStream();
      stream.end(payload);
      try {
        const echoed = await collectStream(stream, 30_000);
        if (echoed.length !== size) {
          throw new Error(`echo mismatch: expected ${size}, got ${echoed.length}`);
        }
      } catch (err: unknown) {
        // Add diagnostic info
        const msg = err instanceof Error ? err.message : String(err);
        throw new Error(
          `${msg} (streamId=${stream.id} destroyed=${stream.destroyed} ` +
          `readableEnded=${stream.readableEnded} writableFinished=${stream.writableFinished})`,
        );
      }
    }

    async function runBurst(): Promise<void> {
      const results = await Promise.all(
        Array.from({ length: 5 }, async (_, i) => {
          const payload = Buffer.alloc(1024, i);
          const stream = client.openStream();
          stream.end(payload);
          return collectStream(stream, 15_000);
        }),
      );
      for (let i = 0; i < 5; i++) {
        if (results[i].length !== 1024) {
          throw new Error(`burst stream ${i} mismatch: expected 1024, got ${results[i].length}`);
        }
      }
    }

    async function runDatagram(): Promise<void> {
      const tag = `dg-${totalOps}`;
      const received = new Promise<Buffer>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('datagram echo timed out')), 500);
        const handler = (data: Buffer) => {
          if (data.toString() === tag) {
            clearTimeout(timer);
            client.removeListener('datagram', handler);
            resolve(data);
          }
        };
        client.on('datagram', handler);
      });
      client.sendDatagram(Buffer.from(tag));
      await received;
    }

    async function runServerPush(): Promise<void> {
      const pushReceived = new Promise<Buffer>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('server push timed out')), 5000);
        serverPushResolve = (buf) => {
          clearTimeout(timer);
          resolve(buf);
        };
      });
      // Send "push" command on a stream
      const cmdStream = client.openStream();
      cmdStream.end(Buffer.from('push'));
      await collectStream(cmdStream, 5000); // wait for push-ack
      const pushed = await pushReceived;
      if (pushed.length !== 512) {
        throw new Error(`server push size mismatch: expected 512, got ${pushed.length}`);
      }
    }

    while ((Date.now() - start) / 1000 < DURATION_S) {
      const scenario = pickWeighted(scenarios);
      const startTs = Date.now();

      try {
        switch (scenario) {
          case 'echo-256': await runEcho(256); break;
          case 'echo-4k': await runEcho(4096); break;
          case 'echo-64k': await runEcho(65536); break;
          case 'burst-5-streams': await runBurst(); break;
        }
        tracker.record(Date.now() - startTs);
        totalOps++;
      } catch (err: unknown) {
        errors++;
        totalOps++;
        const msg = err instanceof Error ? err.message : String(err);
        console.log(`  [quic-mix] ERROR on ${scenario}: ${msg}`);
        // Reconnect if session died.
        try {
          const probe = client.openStream();
          probe.end(Buffer.from('probe'));
          await collectStream(probe, 3000);
        } catch {
          try { await client.close(); } catch { /* ignore */ }
          client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
            rejectUnauthorized: false,
          });
        }
      }

      // Log stats every 30s
      const now = Date.now();
      if ((now - lastCheckpoint) / 1000 >= CHECKPOINT_INTERVAL_S) {
        if (typeof global.gc === 'function') global.gc();
        const mem = process.memoryUsage();
        const elapsedS = (now - start) / 1000;
        const stats = tracker.stats();
        console.log(
          `  [quic-mix] ${elapsedS.toFixed(0)}s: ` +
          `${totalOps} ops (${(totalOps / elapsedS).toFixed(1)} ops/s), ` +
          `${errors} errors, ` +
          `p50=${stats.p50.toFixed(0)}ms p95=${stats.p95.toFixed(0)}ms p99=${stats.p99.toFixed(0)}ms, ` +
          `RSS=${formatMB(mem.rss)}MB heap=${formatMB(mem.heapUsed)}MB`,
        );
        lastCheckpoint = now;
      }
    }

    const elapsedS = (Date.now() - start) / 1000;
    if (typeof global.gc === 'function') global.gc();
    const finalMem = process.memoryUsage();
    const total = totalOps;
    const errorRate = total > 0 ? (errors / total) * 100 : 0;
    const stats = tracker.stats();

    console.log(
      `  [quic-mix] DONE: ${totalOps} ops in ${elapsedS.toFixed(1)}s ` +
      `(${(totalOps / elapsedS).toFixed(1)} ops/s), ${errors} errors (${errorRate.toFixed(2)}%)`,
    );
    console.log(
      `  [quic-mix] latency: p50=${stats.p50.toFixed(0)}ms p95=${stats.p95.toFixed(0)}ms p99=${stats.p99.toFixed(0)}ms`,
    );
    console.log(
      `  [quic-mix] memory: RSS=${formatMB(finalMem.rss)}MB heap=${formatMB(finalMem.heapUsed)}MB`,
    );

    assert.ok(
      errorRate < 1,
      `error rate ${errorRate.toFixed(2)}% exceeds 1% limit (${errors}/${total})`,
    );
    assert.ok(
      finalMem.heapUsed < 100 * 1024 * 1024,
      `heap too large: ${formatMB(finalMem.heapUsed)}MB (limit 100MB)`,
    );

    await client.close();
    await server.close();
  });
});
