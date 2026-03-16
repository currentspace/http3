/**
 * Kqueue driver stress tests.
 *
 * Validates the kqueue-based event loop (macOS) under high concurrency,
 * backpressure, sustained rounds, and rapid connection churn — the same
 * pressure patterns that uncovered real bugs in the io_uring driver.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from './generate-certs.js';
import { createQuicServer, connectQuicAsync } from '../lib/index.js';
import type { QuicServer, QuicServerSession, QuicClientSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

// ── Helpers ──────────────────────────────────────────────────────────

function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
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

interface ResourceSnapshot {
  cpu: NodeJS.CpuUsage;
  mem: number;
  time: number;
}

function snapResources(): ResourceSnapshot {
  return {
    cpu: process.cpuUsage(),
    mem: process.memoryUsage().rss,
    time: Date.now(),
  };
}

function reportLine(
  label: string,
  streams: number,
  errors: number,
  startSnap: ResourceSnapshot,
): void {
  const elapsed = Date.now() - startSnap.time;
  const cpuDelta = process.cpuUsage(startSnap.cpu);
  const memDelta = process.memoryUsage().rss - startSnap.mem;
  const userMs = (cpuDelta.user / 1000).toFixed(0);
  const sysMs = (cpuDelta.system / 1000).toFixed(0);
  const memMB = (memDelta / 1024 / 1024).toFixed(1);
  const streamsPerSec = (streams / (elapsed / 1000)).toFixed(0);
  console.log(
    `  ${label}: ${streams} streams, ${errors} errors, ${elapsed}ms, ${streamsPerSec} streams/s` +
    `  CPU: user=${userMs}ms sys=${sysMs}ms  Mem: ${Number(memMB) >= 0 ? '+' : ''}${memMB}MB`,
  );
}

/** Open connections in batches to avoid fd exhaustion. */
async function openConnectionsBatched(
  port: number,
  total: number,
  batchSize: number,
): Promise<{ clients: QuicClientSession[]; errors: number }> {
  const clients: QuicClientSession[] = [];
  let errors = 0;
  for (let i = 0; i < total; i += batchSize) {
    const batch = Math.min(batchSize, total - i);
    const promises: Promise<void>[] = [];
    for (let j = 0; j < batch; j++) {
      promises.push(
        (async () => {
          try {
            const client = await connectQuicAsync(`127.0.0.1:${port}`, {
              rejectUnauthorized: false,
            });
            clients.push(client);
          } catch {
            errors++;
          }
        })(),
      );
    }
    await Promise.all(promises);
  }
  return { clients, errors };
}

/** Run streams on a single client, returning success/error counts. */
async function runStreamsOnClient(
  client: QuicClientSession,
  count: number,
  payload: Buffer,
  streamTimeoutMs: number,
  verifyData?: boolean,
): Promise<{ ok: number; errors: number }> {
  let ok = 0;
  let errors = 0;
  const promises: Promise<void>[] = [];
  for (let i = 0; i < count; i++) {
    promises.push(
      (async () => {
        try {
          const stream = client.openStream();
          stream.end(payload);
          const echoed = await collectStream(stream, streamTimeoutMs);
          if (verifyData) {
            if (echoed.equals(payload)) {
              ok++;
            } else {
              errors++;
            }
          } else if (echoed.length === payload.length) {
            ok++;
          } else {
            errors++;
          }
        } catch {
          errors++;
        }
      })(),
    );
  }
  await Promise.all(promises);
  return { ok, errors };
}

// ── Tests ────────────────────────────────────────────────────────────

describe('QUIC kqueue stress tests', () => {
  let server: QuicServer;
  let serverPort: number;

  before(async () => {
    const certs = generateTestCerts();

    server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 10_000,
      initialMaxStreamsBidi: 50_000,
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

  // ── 1. Wide blast ──────────────────────────────────────────────────

  it('wide blast — 20 connections × 15 streams × 64KB', { timeout: 25_000 }, async () => {
    const snap = snapResources();
    const payload = Buffer.alloc(64 * 1024, 0xaa);
    const streamTimeout = 20_000;

    const { clients, errors: connErrors } = await openConnectionsBatched(serverPort, 20, 10);

    let totalStreams = 0;
    let totalErrors = connErrors;

    const results = await Promise.all(
      clients.map((c) => runStreamsOnClient(c, 15, payload, streamTimeout)),
    );
    for (const r of results) {
      totalStreams += r.ok;
      totalErrors += r.errors;
    }

    await Promise.all(clients.map((c) => c.close()));

    reportLine('20×15×64KB', totalStreams, totalErrors, snap);
    assert.strictEqual(totalErrors, 0, `expected 0 errors, got ${totalErrors}`);
    assert.strictEqual(totalStreams, 300, `expected 300 streams, got ${totalStreams}`);
  });

  // ── 2. Deep pipeline ──────────────────────────────────────────────

  it('deep pipeline — 5 connections × 200 streams × 4KB', { timeout: 20_000 }, async () => {
    const snap = snapResources();
    const payload = Buffer.alloc(4096, 0xbb);
    const streamTimeout = 15_000;

    const { clients, errors: connErrors } = await openConnectionsBatched(serverPort, 5, 5);

    let totalStreams = 0;
    let totalErrors = connErrors;

    const results = await Promise.all(
      clients.map((c) => runStreamsOnClient(c, 200, payload, streamTimeout)),
    );
    for (const r of results) {
      totalStreams += r.ok;
      totalErrors += r.errors;
    }

    await Promise.all(clients.map((c) => c.close()));

    reportLine('5×200×4KB', totalStreams, totalErrors, snap);
    assert.strictEqual(totalErrors, 0, `expected 0 errors, got ${totalErrors}`);
    assert.strictEqual(totalStreams, 1000, `expected 1000 streams, got ${totalStreams}`);
  });

  // ── 3. Sustained rounds ───────────────────────────────────────────

  it('sustained rounds — 10 connections × 3 rounds × 20 streams × 64KB', { timeout: 45_000 }, async () => {
    const snap = snapResources();
    const payload = Buffer.alloc(64 * 1024, 0xcc);
    const streamTimeout = 20_000;
    const rounds = 3;
    const streamsPerRound = 20;

    const { clients, errors: connErrors } = await openConnectionsBatched(serverPort, 10, 10);

    let totalStreams = 0;
    let totalErrors = connErrors;
    const roundTimes: number[] = [];

    for (let round = 0; round < rounds; round++) {
      const roundStart = Date.now();

      const results = await Promise.all(
        clients.map((c) => runStreamsOnClient(c, streamsPerRound, payload, streamTimeout)),
      );
      for (const r of results) {
        totalStreams += r.ok;
        totalErrors += r.errors;
      }

      roundTimes.push(Date.now() - roundStart);

      // Brief idle between rounds
      if (round < rounds - 1) {
        await new Promise<void>((resolve) => { setTimeout(resolve, 100); });
      }
    }

    await Promise.all(clients.map((c) => c.close()));

    reportLine('10×3×20×64KB', totalStreams, totalErrors, snap);
    for (let i = 0; i < roundTimes.length; i++) {
      console.log(`    round ${i + 1}: ${roundTimes[i]}ms`);
    }

    assert.strictEqual(totalErrors, 0, `expected 0 errors, got ${totalErrors}`);
    assert.strictEqual(totalStreams, 600, `expected 600 streams, got ${totalStreams}`);

    // No round should be >2× slower than round 1
    const baseline = roundTimes[0];
    for (let i = 1; i < roundTimes.length; i++) {
      assert.ok(
        roundTimes[i] <= baseline * 2,
        `round ${i + 1} (${roundTimes[i]}ms) >2× slower than round 1 (${baseline}ms)`,
      );
    }
  });

  // ── 4. Backpressure probe ─────────────────────────────────────────

  it('backpressure probe — 2 connections × 5 streams × 512KB', { timeout: 20_000 }, async () => {
    const snap = snapResources();
    const payload = Buffer.alloc(512 * 1024);
    // Fill with a recognizable pattern for data integrity check
    for (let i = 0; i < payload.length; i++) {
      payload[i] = i & 0xff;
    }
    const streamTimeout = 15_000;

    const { clients, errors: connErrors } = await openConnectionsBatched(serverPort, 2, 2);

    let totalStreams = 0;
    let totalErrors = connErrors;

    const results = await Promise.all(
      clients.map((c) => runStreamsOnClient(c, 5, payload, streamTimeout, true)),
    );
    for (const r of results) {
      totalStreams += r.ok;
      totalErrors += r.errors;
    }

    await Promise.all(clients.map((c) => c.close()));

    reportLine('2×5×512KB', totalStreams, totalErrors, snap);
    assert.strictEqual(totalErrors, 0, `expected 0 errors, got ${totalErrors}`);
    assert.strictEqual(totalStreams, 10, `expected 10 streams, got ${totalStreams}`);
  });

  // ── 5. Connection churn ───────────────────────────────────────────

  it('connection churn — 100 connections in batches of 20 × 1 stream × 1KB', { timeout: 25_000 }, async () => {
    const snap = snapResources();
    const payload = Buffer.alloc(1024, 0xdd);
    const streamTimeout = 10_000;
    const total = 100;
    const batchSize = 20;
    const latencies: number[] = [];

    let successes = 0;
    let errors = 0;

    for (let i = 0; i < total; i += batchSize) {
      const batch = Math.min(batchSize, total - i);
      const batchPromises: Promise<void>[] = [];

      for (let j = 0; j < batch; j++) {
        batchPromises.push(
          (async () => {
            const t0 = Date.now();
            try {
              const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
                rejectUnauthorized: false,
              });
              latencies.push(Date.now() - t0);

              const stream = client.openStream();
              stream.end(payload);
              const echoed = await collectStream(stream, streamTimeout);
              if (echoed.length === payload.length) {
                successes++;
              } else {
                errors++;
              }
              await client.close();
            } catch {
              errors++;
            }
          })(),
        );
      }

      await Promise.all(batchPromises);
    }

    reportLine('100×1×1KB churn', successes, errors, snap);

    // Report latency percentiles
    latencies.sort((a, b) => a - b);
    if (latencies.length > 0) {
      const p50 = latencies[Math.floor(latencies.length * 0.5)];
      const p95 = latencies[Math.floor(latencies.length * 0.95)];
      const p99 = latencies[Math.floor(latencies.length * 0.99)];
      console.log(`    connect latency p50=${p50}ms p95=${p95}ms p99=${p99}ms`);
    }

    assert.ok(
      successes >= 95,
      `expected ≥95 successes, got ${successes} (${errors} errors)`,
    );
  });
});
