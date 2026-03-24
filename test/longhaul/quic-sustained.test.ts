/**
 * QUIC long-haul stability tests (5-minute sustained load).
 * Gated by HTTP3_LONGHAUL=1 environment variable.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { snapshotMemory } from '../support/native-test-helpers.js';
import { createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type { QuicServer, QuicServerSession, QuicClientSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { echoStream } from '../support/echo-stream.js';

function formatMB(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

async function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream collect timed out')), timeoutMs);
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

describe('QUIC sustained load (5 minutes)', { skip: !process.env.HTTP3_LONGHAUL }, () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('5-minute stream creation with throughput tracking', { timeout: 330_000 }, async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 1000,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const serverPort = addr.port;

    const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
      rejectUnauthorized: false,
    });

    const initialMem = snapshotMemory();
    console.log(`  [longhaul/quic] initial RSS=${formatMB(initialMem.rss)}MB heap=${formatMB(initialMem.heapUsed)}MB`);

    const DURATION_S = 300;
    const BATCH_SIZE = 10;
    const PAYLOAD = Buffer.alloc(1024, 0xab);
    const CHECKPOINT_INTERVAL_S = 30;
    let totalStreams = 0;
    let errors = 0;
    const start = Date.now();
    let lastCheckpoint = start;

    while ((Date.now() - start) / 1000 < DURATION_S) {
      // Open 10 streams, send 1KB each, await echo
      const batch = Array.from({ length: BATCH_SIZE }, () =>
        (async () => {
          try {
            const stream = client.openStream();
            stream.end(PAYLOAD);
            const echoed = await collectStream(stream, 15_000);
            if (echoed.length === PAYLOAD.length) {
              totalStreams++;
            } else {
              errors++;
            }
          } catch {
            errors++;
          }
        })(),
      );
      await Promise.all(batch);

      // Log checkpoint every 30s
      const now = Date.now();
      if ((now - lastCheckpoint) / 1000 >= CHECKPOINT_INTERVAL_S) {
        const mem = snapshotMemory();
        const elapsedS = (now - start) / 1000;
        const total = totalStreams + errors;
        console.log(
          `  [longhaul/quic] ${elapsedS.toFixed(0)}s: ` +
          `${totalStreams} streams (${(totalStreams / elapsedS).toFixed(1)}/s), ` +
          `${errors} errors, ` +
          `RSS=${formatMB(mem.rss)}MB heap=${formatMB(mem.heapUsed)}MB`,
        );
        lastCheckpoint = now;
      }
    }

    const elapsedS = (Date.now() - start) / 1000;
    const finalMem = snapshotMemory();
    const total = totalStreams + errors;
    const errorRate = total > 0 ? (errors / total) * 100 : 0;

    console.log(
      `  [longhaul/quic] DONE: ${totalStreams} streams in ${elapsedS.toFixed(1)}s ` +
      `(${(totalStreams / elapsedS).toFixed(1)}/s), ${errors} errors (${errorRate.toFixed(2)}%)`,
    );
    console.log(
      `  [longhaul/quic] memory: initial RSS=${formatMB(initialMem.rss)}MB -> ` +
      `final RSS=${formatMB(finalMem.rss)}MB (${(finalMem.rss / initialMem.rss).toFixed(2)}x)`,
    );

    assert.ok(
      errorRate < 1,
      `error rate ${errorRate.toFixed(2)}% exceeds 1% limit (${errors}/${total})`,
    );
    assert.ok(
      finalMem.rss < initialMem.rss * 3,
      `RSS grew too much: ${formatMB(initialMem.rss)}MB -> ${formatMB(finalMem.rss)}MB ` +
      `(${(finalMem.rss / initialMem.rss).toFixed(2)}x, limit 3x)`,
    );

    await client.close();
    await server.close();
  });

  it('5-minute connection churn', { timeout: 330_000 }, async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 1000,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const serverPort = addr.port;

    const initialMem = snapshotMemory();
    console.log(`  [longhaul/churn] initial RSS=${formatMB(initialMem.rss)}MB`);

    const DURATION_S = 300;
    const PAYLOAD = Buffer.alloc(1024, 0xcd);
    const CHECKPOINT_INTERVAL_S = 30;
    let successfulConnections = 0;
    let errors = 0;
    const start = Date.now();
    let lastCheckpoint = start;

    while ((Date.now() - start) / 1000 < DURATION_S) {
      try {
        const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
          rejectUnauthorized: false,
        });

        // Send 1 stream of 1KB, await echo
        const stream = client.openStream();
        stream.end(PAYLOAD);
        const echoed = await collectStream(stream, 15_000);
        if (echoed.length === PAYLOAD.length) {
          successfulConnections++;
        } else {
          errors++;
        }

        await client.close();
      } catch {
        errors++;
      }

      // Log checkpoint every 30s
      const now = Date.now();
      if ((now - lastCheckpoint) / 1000 >= CHECKPOINT_INTERVAL_S) {
        const mem = snapshotMemory();
        const elapsedS = (now - start) / 1000;
        console.log(
          `  [longhaul/churn] ${elapsedS.toFixed(0)}s: ` +
          `${successfulConnections} conns (${(successfulConnections / elapsedS).toFixed(1)}/s), ` +
          `${errors} errors, ` +
          `RSS=${formatMB(mem.rss)}MB heap=${formatMB(mem.heapUsed)}MB`,
        );
        lastCheckpoint = now;
      }
    }

    const elapsedS = (Date.now() - start) / 1000;
    const finalMem = snapshotMemory();
    const total = successfulConnections + errors;
    const errorRate = total > 0 ? (errors / total) * 100 : 0;

    console.log(
      `  [longhaul/churn] DONE: ${successfulConnections} connections in ${elapsedS.toFixed(1)}s ` +
      `(${(successfulConnections / elapsedS).toFixed(1)}/s), ${errors} errors (${errorRate.toFixed(2)}%)`,
    );
    console.log(
      `  [longhaul/churn] memory: initial RSS=${formatMB(initialMem.rss)}MB -> ` +
      `final RSS=${formatMB(finalMem.rss)}MB`,
    );

    assert.ok(
      successfulConnections > 200,
      `expected > 200 successful connections, got ${successfulConnections}`,
    );
    assert.ok(
      errorRate < 5,
      `error rate ${errorRate.toFixed(2)}% exceeds 5% limit (${errors}/${total})`,
    );

    await server.close();
  });
});
