/**
 * Standalone QUIC stress client for two-process benchmarking.
 * Reads config from argv[2] (JSON), reports results to stdout.
 *
 * Usage: node dist-test/test/quic-bench-client.js '{"port":6000,...}'
 */

import { connectQuicAsync } from '../lib/index.js';
import type { QuicClientSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

interface BenchConfig {
  port: number;
  connections: number;
  streamsPerConnection: number;
  messageSize: number;
  timeoutMs: number;
}

interface LatencyTracker {
  values: number[];
  add(ms: number): void;
  percentile(p: number): number;
  mean(): number;
  max(): number;
  min(): number;
}

function createLatencyTracker(): LatencyTracker {
  const values: number[] = [];
  return {
    values,
    add(ms: number) { values.push(ms); },
    percentile(p: number): number {
      if (values.length === 0) return 0;
      const sorted = [...values].sort((a, b) => a - b);
      const idx = Math.min(Math.floor(sorted.length * p / 100), sorted.length - 1);
      return sorted[idx];
    },
    mean(): number {
      if (values.length === 0) return 0;
      return values.reduce((a, b) => a + b, 0) / values.length;
    },
    max(): number {
      return values.length > 0 ? Math.max(...values) : 0;
    },
    min(): number {
      return values.length > 0 ? Math.min(...values) : 0;
    },
  };
}

function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream timed out')), timeoutMs);
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

async function main(): Promise<void> {
  const configStr = process.argv[2];
  if (!configStr) {
    process.stderr.write('Usage: quic-bench-client.js \'{"port":6000,...}\'\n');
    process.exit(1);
  }

  const config: BenchConfig = JSON.parse(configStr);
  const cpuStart = process.cpuUsage();
  const memStart = process.memoryUsage();
  const hrStart = process.hrtime.bigint();

  const streamLatency = createLatencyTracker();
  const connLatency = createLatencyTracker();

  let totalStreams = 0;
  let totalBytes = 0;
  let errors = 0;

  const payload = Buffer.alloc(config.messageSize, 0xcc);

  // Phase 1: Open connections
  const clients: QuicClientSession[] = [];
  for (let c = 0; c < config.connections; c++) {
    const connStart = process.hrtime.bigint();
    try {
      const client = await connectQuicAsync(`127.0.0.1:${config.port}`, {
        rejectUnauthorized: false,
        initialMaxStreamsBidi: 50_000,
      });
      const connMs = Number(process.hrtime.bigint() - connStart) / 1e6;
      connLatency.add(connMs);
      clients.push(client);
    } catch (err) {
      errors++;
      process.stderr.write(`Connection ${c} failed: ${err}\n`);
    }
  }

  // Phase 2: Run streams
  const allStreams: Promise<void>[] = [];
  for (const client of clients) {
    for (let s = 0; s < config.streamsPerConnection; s++) {
      allStreams.push(
        (async () => {
          const streamStart = process.hrtime.bigint();
          try {
            const stream = client.openStream();
            stream.end(payload);
            const echoed = await collectStream(stream, config.timeoutMs);
            const streamMs = Number(process.hrtime.bigint() - streamStart) / 1e6;
            if (echoed.length === payload.length) {
              totalStreams++;
              totalBytes += echoed.length * 2;
              streamLatency.add(streamMs);
            } else {
              errors++;
              process.stderr.write(`Stream length mismatch: expected ${payload.length}, got ${echoed.length}\n`);
            }
          } catch (err) {
            errors++;
            const msg = err instanceof Error ? err.message : String(err);
            process.stderr.write(`Stream error (conn ${clients.indexOf(client)}, stream ${s}): ${msg}\n`);
          }
        })(),
      );
    }
  }

  await Promise.all(allStreams);

  // Phase 3: Close
  await Promise.all(clients.map((c) => c.close()));

  const hrEnd = process.hrtime.bigint();
  const elapsedMs = Number(hrEnd - hrStart) / 1e6;
  const cpuEnd = process.cpuUsage(cpuStart);
  const memEnd = process.memoryUsage();

  const result = {
    type: 'result',
    config,
    totalStreams,
    totalBytes,
    errors,
    elapsedMs: Math.round(elapsedMs),
    throughputMbps: Number(((totalBytes * 8) / (elapsedMs / 1000) / 1e6).toFixed(1)),
    streamsPerSecond: Number((totalStreams / (elapsedMs / 1000)).toFixed(0)),
    connEstablish: {
      count: connLatency.values.length,
      meanMs: Number(connLatency.mean().toFixed(2)),
      p50Ms: Number(connLatency.percentile(50).toFixed(2)),
      p95Ms: Number(connLatency.percentile(95).toFixed(2)),
      p99Ms: Number(connLatency.percentile(99).toFixed(2)),
      maxMs: Number(connLatency.max().toFixed(2)),
    },
    streamLatency: {
      count: streamLatency.values.length,
      meanMs: Number(streamLatency.mean().toFixed(2)),
      p50Ms: Number(streamLatency.percentile(50).toFixed(2)),
      p95Ms: Number(streamLatency.percentile(95).toFixed(2)),
      p99Ms: Number(streamLatency.percentile(99).toFixed(2)),
      maxMs: Number(streamLatency.max().toFixed(2)),
      minMs: Number(streamLatency.min().toFixed(2)),
    },
    cpu: {
      userMs: Math.round(cpuEnd.user / 1000),
      systemMs: Math.round(cpuEnd.system / 1000),
      totalMs: Math.round((cpuEnd.user + cpuEnd.system) / 1000),
      utilizationPct: Number((((cpuEnd.user + cpuEnd.system) / 1000) / elapsedMs * 100).toFixed(1)),
    },
    memory: {
      heapUsedStart: memStart.heapUsed,
      heapUsedEnd: memEnd.heapUsed,
      heapDeltaMB: Number(((memEnd.heapUsed - memStart.heapUsed) / 1e6).toFixed(1)),
      rssEnd: memEnd.rss,
      rssMB: Number((memEnd.rss / 1e6).toFixed(1)),
    },
  };

  process.stdout.write(JSON.stringify(result) + '\n');
  process.exit(0);
}

void main();
