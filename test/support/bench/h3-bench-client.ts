/**
 * Standalone H3 stress client for two-process benchmarking.
 * Mirrors quic-bench-client.ts but for HTTP/3.
 */

import { connectAsync } from '../../../lib/index.js';
import { binding } from '../../../lib/event-loop.js';
import type { Http3ClientSession } from '../../../lib/index.js';

interface BenchConfig {
  host?: string;
  port: number;
  connections: number;
  streamsPerConnection: number;
  messageSize: number;
  timeoutMs: number;
  connectTimeoutMs?: number;
  warmupMs?: number;
  durationMs?: number;
  maxInflightPerConnection?: number;
  runtimeMode?: 'auto' | 'fast' | 'portable';
  fallbackPolicy?: 'error' | 'warn-and-fallback';
  clientId?: number;
  connectionBarrier?: boolean;
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

interface BenchDiag {
  enabled: boolean;
  connectionsStarted: number;
  connectionsOpened: number;
  requestsStarted: number;
  requestStreamsOpened: number;
  responsesCompleted: number;
  requestErrors: number;
  closeStarted: number;
  closeCompleted: number;
  interval: NodeJS.Timeout | null;
}

function createDiag(): BenchDiag {
  return {
    enabled: process.env.H3_BENCH_DIAG === '1',
    connectionsStarted: 0,
    connectionsOpened: 0,
    requestsStarted: 0,
    requestStreamsOpened: 0,
    responsesCompleted: 0,
    requestErrors: 0,
    closeStarted: 0,
    closeCompleted: 0,
    interval: null,
  };
}

function startDiag(diag: BenchDiag, clientId: number | undefined): void {
  if (!diag.enabled) return;
  diag.interval = setInterval(() => {
    process.stderr.write(
      `h3-bench-client ${clientId ?? '?'} diag ` +
      `conn=${diag.connectionsOpened}/${diag.connectionsStarted} ` +
      `req=${diag.requestStreamsOpened}/${diag.requestsStarted} ` +
      `done=${diag.responsesCompleted} err=${diag.requestErrors} ` +
      `close=${diag.closeCompleted}/${diag.closeStarted}\n`,
    );
  }, 1000);
}

function stopDiag(diag: BenchDiag): void {
  if (!diag.interval) return;
  clearInterval(diag.interval);
  diag.interval = null;
}

async function doRequest(
  session: Http3ClientSession,
  payload: Buffer,
  timeoutMs: number,
  diag?: BenchDiag,
): Promise<number> {
  if (diag) diag.requestsStarted++;
  const stream = await session.requestAsync({
    ':method': 'POST',
    ':path': '/echo',
    ':authority': 'localhost',
    ':scheme': 'https',
  }, { endStream: false, timeoutMs });
  if (diag) diag.requestStreamsOpened++;
  stream.end(payload);

  return new Promise((resolve, reject) => {
    let responseBytes = 0;
    const timer = setTimeout(() => reject(new Error('h3 request timed out')), timeoutMs);
    stream.on('response', () => { /* headers received */ });
    stream.on('data', (chunk: Buffer) => { responseBytes += chunk.length; });
    stream.on('end', () => {
      clearTimeout(timer);
      if (diag) diag.responsesCompleted++;
      resolve(responseBytes);
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      if (diag) diag.requestErrors++;
      reject(err);
    });
  });
}

function writeJson(message: Record<string, unknown>): void {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function classifyError(error: unknown): string {
  if (error instanceof Error) {
    return error.message || error.name;
  }
  return String(error);
}

function incrementCount(counts: Record<string, number>, key: string): void {
  counts[key] = (counts[key] ?? 0) + 1;
}

function waitForStartSignal(): Promise<void> {
  return new Promise((resolve, reject) => {
    let input = '';
    let settled = false;

    const cleanup = (): void => {
      process.stdin.off('data', onData);
      process.stdin.off('end', onEnd);
      process.stdin.off('error', onError);
    };

    const settle = (fn: () => void): void => {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      fn();
    };

    const onData = (data: Buffer | string): void => {
      input += data.toString();
      const lines = input.split('\n');
      input = lines.pop() ?? '';
      for (const line of lines) {
        const command = line.trim();
        if (!command) {
          continue;
        }
        if (command === 'start') {
          settle(resolve);
          return;
        }
        settle(() => reject(new Error(`unexpected benchmark control command: ${command}`)));
        return;
      }
    };

    const onEnd = (): void => {
      settle(() => reject(new Error('benchmark control stream closed before start')));
    };

    const onError = (error: Error): void => {
      settle(() => reject(error));
    };

    process.stdin.setEncoding('utf8');
    process.stdin.on('data', onData);
    process.stdin.once('end', onEnd);
    process.stdin.once('error', onError);
    process.stdin.resume();
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
    process.stderr.write('Usage: h3-bench-client.js \'{"port":6000,...}\'\n');
    process.exit(1);
  }

  const config: BenchConfig = JSON.parse(configStr);
  const diag = createDiag();
  startDiag(diag, config.clientId);
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
  const errorCounts: Record<string, number> = {};

  const payload = Buffer.alloc(config.messageSize, 0xcc);
  const host = config.host ?? '127.0.0.1';
  const connectTimeoutMs = config.connectTimeoutMs ?? config.timeoutMs;

  // Phase 1: Open connections
  const clients: Http3ClientSession[] = [];
  for (let c = 0; c < config.connections; c++) {
    diag.connectionsStarted++;
    const connStart = process.hrtime.bigint();
    try {
      const client = await connectAsync(`${host}:${config.port}`, {
        rejectUnauthorized: false,
        initialMaxStreamsBidi: 50_000,
        connectTimeoutMs,
        runtimeMode: config.runtimeMode,
        fallbackPolicy: config.fallbackPolicy,
      });
      const connMs = Number(process.hrtime.bigint() - connStart) / 1e6;
      connLatency.add(connMs);
      clients.push(client);
      diag.connectionsOpened++;
      const runtimeKey = formatRuntimeSelection(client.runtimeInfo);
      runtimeSelections.set(runtimeKey, (runtimeSelections.get(runtimeKey) ?? 0) + 1);
    } catch (err) {
      errors++;
      incrementCount(errorCounts, classifyError(err));
      process.stderr.write(`H3 connection ${c} failed: ${err}\n`);
    }
  }

  if (config.connectionBarrier) {
    writeJson({
      type: 'ready',
      clientId: config.clientId ?? null,
      expectedConnections: config.connections,
      connectionsOpened: clients.length,
      errors,
      errorCounts,
    });
    await waitForStartSignal();
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

    async function runSteadyStateWorker(client: Http3ClientSession): Promise<void> {
      for (;;) {
        const streamStart = process.hrtime.bigint();
        const loadElapsedAtStartMs = Number(streamStart - loadStart) / 1e6;
        if (loadElapsedAtStartMs >= stopAfterMs) {
          return;
        }
        const phase = classifyMeasurementPhase(loadElapsedAtStartMs, warmupMs, durationMs);

        try {
          const echoedBytes = await doRequest(client, payload, config.timeoutMs, diag);
          const streamMs = Number(process.hrtime.bigint() - streamStart) / 1e6;
          if (echoedBytes === payload.length) {
            recordCompletion(phase, echoedBytes * 2, streamMs);
          } else {
            incrementCount(errorCounts, 'response length mismatch');
            errors++;
          }
        } catch (err) {
          incrementCount(errorCounts, classifyError(err));
          errors++;
        }
      }
    }

    await Promise.all(
      clients.flatMap((client) =>
        Array.from(
          { length: maxInflightPerConnection ?? 1 },
          () => runSteadyStateWorker(client),
        ),
      ),
    );
    loadElapsedMs = Number(process.hrtime.bigint() - loadStart) / 1e6;
  } else {
    const fixedMaxInflightPerConnection = Math.max(
      1,
      Math.min(config.streamsPerConnection, config.maxInflightPerConnection ?? config.streamsPerConnection),
    );

    async function runFixedRequest(client: Http3ClientSession): Promise<void> {
      const streamStart = process.hrtime.bigint();
      try {
        const echoedBytes = await doRequest(client, payload, config.timeoutMs, diag);
        const streamMs = Number(process.hrtime.bigint() - streamStart) / 1e6;
        if (echoedBytes === payload.length) {
          totalStreams++;
          totalBytes += echoedBytes * 2;
          streamLatency.add(streamMs);
        } else {
          incrementCount(errorCounts, 'response length mismatch');
          errors++;
        }
      } catch (err) {
        incrementCount(errorCounts, classifyError(err));
        errors++;
      }
    }

    async function runFixedConnection(client: Http3ClientSession): Promise<void> {
      let nextStream = 0;
      async function worker(): Promise<void> {
        for (;;) {
          const streamIndex = nextStream;
          nextStream++;
          if (streamIndex >= config.streamsPerConnection) {
            return;
          }
          await runFixedRequest(client);
        }
      }
      await Promise.all(
        Array.from({ length: fixedMaxInflightPerConnection }, () => worker()),
      );
    }

    await Promise.all(clients.map((client) => runFixedConnection(client)));
    loadElapsedMs = Number(process.hrtime.bigint() - loadStart) / 1e6;
  }

  // Phase 3: Close
  await Promise.all(clients.map(async (c) => {
    diag.closeStarted++;
    await c.close();
    diag.closeCompleted++;
  }));
  stopDiag(diag);

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
    errorCounts,
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
      maxInflightPerConnection: steadyState
        ? maxInflightPerConnection
        : (config.maxInflightPerConnection ?? null),
      warmupStreams,
      warmupBytes,
      cooldownStreams,
      cooldownBytes,
      overallStreams,
      overallBytes,
    },
    reactorTelemetry: binding.runtimeTelemetry(),
  };

  writeJson(result);
  process.exit(0);
}

void main();
