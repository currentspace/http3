/**
 * Standalone QUIC stress client for two-process benchmarking.
 * Reads config from argv[2] (JSON), reports results to stdout.
 *
 * Usage: node dist-test/test/support/bench/quic-bench-client.js '{"port":6000,...}'
 */

import { connectQuicAsync } from '../../../lib/index.js';
import { binding } from '../../../lib/event-loop.js';
import type { QuicClientSession } from '../../../lib/index.js';
import type { QuicStream } from '../../../lib/quic-stream.js';

interface BenchConfig {
  host?: string;
  port: number;
  connections: number;
  streamsPerConnection: number;
  messageSize: number;
  timeoutMs: number;
  warmupMs?: number;
  durationMs?: number;
  maxInflightPerConnection?: number;
  runtimeMode?: 'auto' | 'fast' | 'portable';
  fallbackPolicy?: 'error' | 'warn-and-fallback';
  clientId?: number;
}

type MeasurementPhase = 'warmup' | 'measured' | 'cooldown';

function formatRuntimeSelection(runtimeInfo: {
  selectedMode?: string | null;
  driver?: string | null;
  fallbackOccurred?: boolean;
  requestedMode?: string | null;
} | null | undefined): string {
  const selectedMode = runtimeInfo?.selectedMode ?? 'unknown';
  const driver = runtimeInfo?.driver ?? 'unknown';
  const fallback = runtimeInfo?.fallbackOccurred === true ? 'fallback' : 'direct';
  const requestedMode = runtimeInfo?.requestedMode ?? 'unknown';
  return `${requestedMode}->${selectedMode}/${driver}/${fallback}`;
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
      let max = 0;
      for (const value of values) {
        if (value > max) {
          max = value;
        }
      }
      return max;
    },
    min(): number {
      if (values.length === 0) return 0;
      let min = values[0];
      for (let i = 1; i < values.length; i += 1) {
        if (values[i] < min) {
          min = values[i];
        }
      }
      return min;
    },
  };
}

function collectStreamLength(stream: QuicStream, timeoutMs: number): Promise<number> {
  return new Promise((resolve, reject) => {
    let settled = false;
    let totalLength = 0;

    const cleanup = (): void => {
      clearTimeout(timer);
      stream.off('data', onData);
      stream.off('end', onEnd);
      stream.off('error', onError);
    };

    const settle = (fn: () => void): void => {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      fn();
    };

    const onData = (chunk: Buffer): void => {
      totalLength += chunk.length;
    };

    const onEnd = (): void => {
      settle(() => {
        resolve(totalLength);
      });
    };

    const onError = (err: Error): void => {
      settle(() => {
        reject(err);
      });
    };

    const timer = setTimeout(() => {
      settle(() => {
        try {
          stream.close();
        } catch {
          if (!stream.destroyed) {
            stream.destroy();
          }
        }
        reject(new Error('stream timed out'));
      });
    }, timeoutMs);

    stream.on('data', onData);
    stream.once('end', onEnd);
    stream.once('error', onError);
  });
}

function hasSteadyStateWindow(config: BenchConfig): boolean {
  return Number.isFinite(config.durationMs) && (config.durationMs ?? 0) > 0;
}

function classifyMeasurementPhase(
  loadElapsedMs: number,
  warmupMs: number,
  durationMs: number,
): MeasurementPhase {
  if (loadElapsedMs < warmupMs) {
    return 'warmup';
  }
  if (loadElapsedMs < warmupMs + durationMs) {
    return 'measured';
  }
  return 'cooldown';
}

function computeMeasuredWindowMs(loadElapsedMs: number, warmupMs: number, durationMs: number): number {
  const measurementEndMs = Math.min(loadElapsedMs, warmupMs + durationMs);
  return Math.max(0, measurementEndMs - warmupMs);
}

async function main(): Promise<void> {
  const configStr = process.argv[2];
  if (!configStr) {
    process.stderr.write('Usage: quic-bench-client.js \'{"port":6000,...}\'\n');
    process.exit(1);
  }

  const config: BenchConfig = JSON.parse(configStr);
  binding.resetRuntimeTelemetry();
  const cpuStart = process.cpuUsage();
  const memStart = process.memoryUsage();
  const hrStart = process.hrtime.bigint();

  const streamLatency = createLatencyTracker();
  const connLatency = createLatencyTracker();
  const runtimeSelections = new Map<string, number>();

  let totalStreams = 0;
  let totalBytes = 0;
  let warmupStreams = 0;
  let warmupBytes = 0;
  let cooldownStreams = 0;
  let cooldownBytes = 0;
  let errors = 0;

  const payload = Buffer.alloc(config.messageSize, 0xcc);
  const host = config.host ?? '127.0.0.1';

  // Phase 1: Open connections
  const clients: QuicClientSession[] = [];
  for (let c = 0; c < config.connections; c++) {
    const connStart = process.hrtime.bigint();
    try {
      const client = await connectQuicAsync(`${host}:${config.port}`, {
        rejectUnauthorized: false,
        initialMaxStreamsBidi: 50_000,
        runtimeMode: config.runtimeMode,
        fallbackPolicy: config.fallbackPolicy,
      });
      const connMs = Number(process.hrtime.bigint() - connStart) / 1e6;
      connLatency.add(connMs);
      clients.push(client);
      const runtimeKey = formatRuntimeSelection(client.runtimeInfo);
      runtimeSelections.set(runtimeKey, (runtimeSelections.get(runtimeKey) ?? 0) + 1);
    } catch (err) {
      errors++;
      process.stderr.write(`Connection ${c} failed: ${err}\n`);
    }
  }

  // Phase 2: Run streams
  const steadyState = hasSteadyStateWindow(config);
  const warmupMs = steadyState ? Math.max(0, config.warmupMs ?? 0) : 0;
  const durationMs = steadyState ? Math.max(1, config.durationMs ?? 0) : 0;
  const maxInflightPerConnection = steadyState ? Math.max(1, config.maxInflightPerConnection ?? 1) : null;
  const loadStart = process.hrtime.bigint();
  let loadElapsedMs = 0;

  if (steadyState) {
    const stopAfterMs = warmupMs + durationMs;
    const recordCompletion = (phase: MeasurementPhase, responseBytes: number, streamMs: number) => {
      if (phase === 'warmup') {
        warmupStreams++;
        warmupBytes += responseBytes;
        return;
      }
      if (phase === 'cooldown') {
        cooldownStreams++;
        cooldownBytes += responseBytes;
        return;
      }
      totalStreams++;
      totalBytes += responseBytes;
      streamLatency.add(streamMs);
    };

    async function runSteadyStateWorker(client: QuicClientSession, clientIndex: number, workerIndex: number): Promise<void> {
      for (;;) {
        const streamStart = process.hrtime.bigint();
        const loadElapsedAtStartMs = Number(streamStart - loadStart) / 1e6;
        if (loadElapsedAtStartMs >= stopAfterMs) {
          return;
        }
        const phase = classifyMeasurementPhase(loadElapsedAtStartMs, warmupMs, durationMs);

        try {
          const stream = client.openStream();
          stream.end(payload);
          const echoedLength = await collectStreamLength(stream, config.timeoutMs);
          const streamMs = Number(process.hrtime.bigint() - streamStart) / 1e6;
          if (echoedLength === payload.length) {
            recordCompletion(phase, echoedLength * 2, streamMs);
          } else {
            errors++;
            process.stderr.write(`Stream length mismatch: expected ${payload.length}, got ${echoedLength}\n`);
          }
        } catch (err) {
          errors++;
          const msg = err instanceof Error ? err.message : String(err);
          process.stderr.write(`Steady-state stream error (conn ${clientIndex}, worker ${workerIndex}): ${msg}\n`);
        }
      }
    }

    await Promise.all(
      clients.flatMap((client, clientIndex) =>
        Array.from(
          { length: maxInflightPerConnection ?? 1 },
          (_, workerIndex) => runSteadyStateWorker(client, clientIndex, workerIndex),
        ),
      ),
    );
    loadElapsedMs = Number(process.hrtime.bigint() - loadStart) / 1e6;
  } else {
    const allStreams: Promise<void>[] = [];
    for (const [clientIndex, client] of clients.entries()) {
      for (let s = 0; s < config.streamsPerConnection; s++) {
        allStreams.push(
          (async () => {
            const streamStart = process.hrtime.bigint();
            try {
              const stream = client.openStream();
              stream.end(payload);
              const echoedLength = await collectStreamLength(stream, config.timeoutMs);
              const streamMs = Number(process.hrtime.bigint() - streamStart) / 1e6;
              if (echoedLength === payload.length) {
                totalStreams++;
                totalBytes += echoedLength * 2;
                streamLatency.add(streamMs);
              } else {
                errors++;
                process.stderr.write(`Stream length mismatch: expected ${payload.length}, got ${echoedLength}\n`);
              }
            } catch (err) {
              errors++;
              const msg = err instanceof Error ? err.message : String(err);
              process.stderr.write(`Stream error (conn ${clientIndex}, stream ${s}): ${msg}\n`);
            }
          })(),
        );
      }
    }

    await Promise.all(allStreams);
    loadElapsedMs = Number(process.hrtime.bigint() - loadStart) / 1e6;
  }

  // Phase 3: Close
  await Promise.all(clients.map((c) => c.close()));

  const hrEnd = process.hrtime.bigint();
  const elapsedMs = Number(hrEnd - hrStart) / 1e6;
  const cpuEnd = process.cpuUsage(cpuStart);
  const memEnd = process.memoryUsage();
  const measuredElapsedMs = steadyState
    ? computeMeasuredWindowMs(loadElapsedMs, warmupMs, durationMs)
    : elapsedMs;
  const overallStreams = totalStreams + warmupStreams + cooldownStreams;
  const overallBytes = totalBytes + warmupBytes + cooldownBytes;

  const result = {
    type: 'result',
    clientId: config.clientId ?? null,
    config,
    connectionsOpened: clients.length,
    runtimeSelections: Object.fromEntries(
      Array.from(runtimeSelections.entries()).sort((left, right) => left[0].localeCompare(right[0])),
    ),
    totalStreams,
    totalBytes,
    errors,
    elapsedMs: Math.round(elapsedMs),
    throughputMbps: Number((measuredElapsedMs > 0 ? ((totalBytes * 8) / (measuredElapsedMs / 1000) / 1e6) : 0).toFixed(1)),
    streamsPerSecond: Number((measuredElapsedMs > 0 ? (totalStreams / (measuredElapsedMs / 1000)) : 0).toFixed(0)),
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
    measurement: {
      mode: steadyState ? 'steady-state' : 'fixed-workload',
      warmupMs,
      targetDurationMs: steadyState ? durationMs : null,
      measuredMs: Math.round(measuredElapsedMs),
      loadElapsedMs: Math.round(steadyState ? loadElapsedMs : elapsedMs),
      maxInflightPerConnection,
      warmupStreams,
      warmupBytes,
      cooldownStreams,
      cooldownBytes,
      overallStreams,
      overallBytes,
    },
    reactorTelemetry: binding.runtimeTelemetry(),
  };

  process.stdout.write(JSON.stringify(result) + '\n');
  process.exit(0);
}

void main();
