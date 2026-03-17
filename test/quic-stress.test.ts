/**
 * QUIC stress testing framework and tests.
 * Validates raw QUIC under high concurrency and throughput.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createQuicServer, connectQuicAsync } from '../lib/index.js';
import type { QuicServer, QuicServerSession, QuicClientSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

const IS_CI = process.env.GITHUB_ACTIONS === 'true' || process.env.CI === 'true';

let certs: { key: Buffer; cert: Buffer };

// ── Stress test framework ──────────────────────────────────────────

export interface StressConfig {
  /** Number of concurrent connections */
  connections: number;
  /** Streams opened per connection */
  streamsPerConnection: number;
  /** Bytes per message sent on each stream */
  messageSize: number;
  /** Maximum test duration (ms) */
  timeoutMs?: number;
}

export interface StressResult {
  totalStreams: number;
  totalBytes: number;
  elapsedMs: number;
  errors: number;
  throughputMbps: number;
  streamsPerSecond: number;
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

export async function runStressTest(
  serverPort: number,
  config: StressConfig,
): Promise<StressResult> {
  const timeoutMs = config.timeoutMs ?? 30_000;
  const streamTimeoutCapMs = IS_CI ? 45_000 : 15_000;
  const streamTimeoutMs = Math.min(timeoutMs, streamTimeoutCapMs);
  const start = Date.now();
  let totalStreams = 0;
  let totalBytes = 0;
  let errors = 0;

  const payload = Buffer.alloc(config.messageSize, 0xcc);

  // Open all connections
  const clients: QuicClientSession[] = [];
  for (let c = 0; c < config.connections; c++) {
    try {
      const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
        rejectUnauthorized: false,
      });
      clients.push(client);
    } catch {
      errors++;
    }
  }

  // Run streams across all connections
  const allStreams: Promise<void>[] = [];

  for (const client of clients) {
    for (let s = 0; s < config.streamsPerConnection; s++) {
      allStreams.push(
        (async () => {
          try {
            const stream = client.openStream();
            stream.end(payload);
            const echoed = await collectStream(stream, streamTimeoutMs);
            if (echoed.length === payload.length) {
              totalStreams++;
              totalBytes += echoed.length * 2; // sent + received
            } else {
              errors++;
            }
          } catch {
            errors++;
          }
        })(),
      );
    }
  }

  await Promise.all(allStreams);

  // Close all connections
  await Promise.all(clients.map((c) => c.close()));

  const elapsedMs = Date.now() - start;

  return {
    totalStreams,
    totalBytes,
    elapsedMs,
    errors,
    throughputMbps: (totalBytes * 8) / (elapsedMs / 1000) / 1_000_000,
    streamsPerSecond: totalStreams / (elapsedMs / 1000),
  };
}

// ── Tests ──────────────────────────────────────────────────────────

describe('QUIC stress tests', () => {
  let server: QuicServer;
  let serverPort: number;

  before(async () => {
    certs = generateTestCerts();

    server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 1000,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    serverPort = addr.port;
  });

  after(async () => {
    await server.close();
  });

  it('10 connections × 10 streams × 1KB', async () => {
    const result = await runStressTest(serverPort, {
      connections: 10,
      streamsPerConnection: 10,
      messageSize: 1024,
      timeoutMs: 30_000,
    });

    console.log(`  Stress: ${result.totalStreams} streams, ${result.errors} errors, ` +
      `${result.elapsedMs}ms, ${result.throughputMbps.toFixed(1)} Mbps, ` +
      `${result.streamsPerSecond.toFixed(0)} streams/s`);

    assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
    assert.strictEqual(result.totalStreams, 100, `expected 100 streams, got ${result.totalStreams}`);
  });

  it('5 connections × 50 streams × 4KB', async () => {
    const result = await runStressTest(serverPort, {
      connections: 5,
      streamsPerConnection: 50,
      messageSize: 4096,
      timeoutMs: 30_000,
    });

    console.log(`  Stress: ${result.totalStreams} streams, ${result.errors} errors, ` +
      `${result.elapsedMs}ms, ${result.throughputMbps.toFixed(1)} Mbps, ` +
      `${result.streamsPerSecond.toFixed(0)} streams/s`);

    assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
    assert.strictEqual(result.totalStreams, 250, `expected 250 streams, got ${result.totalStreams}`);
  });

  it('1 connection × 100 streams × 16KB (single-conn burst)', async () => {
    const result = await runStressTest(serverPort, {
      connections: 1,
      streamsPerConnection: 100,
      messageSize: 16384,
      timeoutMs: 30_000,
    });

    console.log(`  Stress: ${result.totalStreams} streams, ${result.errors} errors, ` +
      `${result.elapsedMs}ms, ${result.throughputMbps.toFixed(1)} Mbps, ` +
      `${result.streamsPerSecond.toFixed(0)} streams/s`);

    assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
    assert.strictEqual(result.totalStreams, 100, `expected 100 streams, got ${result.totalStreams}`);
  });

  it('20 connections × 5 streams × 64KB (many connections)', async () => {
    const result = await runStressTest(serverPort, {
      connections: 20,
      streamsPerConnection: 5,
      messageSize: 65536,
      timeoutMs: 60_000,
    });

    console.log(`  Stress: ${result.totalStreams} streams, ${result.errors} errors, ` +
      `${result.elapsedMs}ms, ${result.throughputMbps.toFixed(1)} Mbps, ` +
      `${result.streamsPerSecond.toFixed(0)} streams/s`);

    assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
    assert.strictEqual(result.totalStreams, 100, `expected 100 streams, got ${result.totalStreams}`);
  });
});
