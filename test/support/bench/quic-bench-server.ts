/**
 * Standalone QUIC echo server for two-process benchmarking.
 * Communicates readiness via stdout JSON.
 *
 * Usage: node dist-test/test/support/bench/quic-bench-server.js
 */

import { createQuicServer } from '../../../lib/index.js';
import { binding } from '../../../lib/event-loop.js';
import { generateTestCerts } from '../generate-certs.js';
import type { QuicServerSession } from '../../../lib/index.js';
import type { QuicStream } from '../../../lib/quic-stream.js';

interface BenchServerConfig {
  host?: string;
  port?: number;
  runtimeMode?: 'auto' | 'fast' | 'portable';
  fallbackPolicy?: 'error' | 'warn-and-fallback';
  statsIntervalMs?: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function loadConfig(): BenchServerConfig {
  const configStr = process.argv[2];
  if (!configStr) {
    return {};
  }

  try {
    return JSON.parse(configStr) as BenchServerConfig;
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`Invalid quic bench server config: ${message}\n`);
    process.exit(1);
  }
}

async function main(): Promise<void> {
  const config = loadConfig();
  binding.resetRuntimeTelemetry();
  const certs = generateTestCerts();

  const server = createQuicServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    maxConnections: 10_000,
    initialMaxStreamsBidi: 50_000,
    runtimeMode: config.runtimeMode,
    fallbackPolicy: config.fallbackPolicy,
  });

  let sessionCount = 0;
  let activeSessions = 0;
  let sessionsClosed = 0;
  let streamCount = 0;
  let bytesEchoed = 0;

  server.on('session', (session: QuicServerSession) => {
    sessionCount++;
    activeSessions++;
    session.once('close', () => {
      activeSessions = Math.max(0, activeSessions - 1);
      sessionsClosed++;
    });
    session.on('stream', (stream: QuicStream) => {
      streamCount++;
      let singleChunk: Buffer | null = null;
      let chunks: Buffer[] | null = null;
      let totalLength = 0;
      stream.on('data', (chunk: Buffer) => {
        bytesEchoed += chunk.length;
        totalLength += chunk.length;
        if (singleChunk === null && chunks === null) {
          singleChunk = chunk;
        } else {
          if (chunks === null) {
            chunks = [singleChunk!];
            singleChunk = null;
          }
          chunks.push(chunk);
        }
      });
      stream.on('end', () => {
        const body = chunks
          ? Buffer.concat(chunks, totalLength)
          : (singleChunk ?? Buffer.alloc(0));
        stream.end(body);
      });
    });
  });

  const emitJson = (message: Record<string, unknown>): void => {
    process.stdout.write(`${JSON.stringify(message)}\n`);
  };

  const snapshot = (type: 'stats' | 'summary'): Record<string, unknown> => {
    const cpu = process.cpuUsage();
    const memory = process.memoryUsage();
    return {
      type,
      timestamp: Date.now(),
      sessionCount,
      activeSessions,
      sessionsClosed,
      streamCount,
      bytesEchoed,
      heapUsed: memory.heapUsed,
      rss: memory.rss,
      cpuUser: cpu.user,
      cpuSystem: cpu.system,
      runtimeInfo: server.runtimeInfo,
      reactorTelemetry: binding.runtimeTelemetry(),
    };
  };

  const addr = await server.listen(config.port ?? 0, config.host ?? '127.0.0.1');

  emitJson({
    type: 'ready',
    port: addr.port,
    address: addr.address,
    runtimeInfo: server.runtimeInfo,
  });

  const statsInterval = setInterval(() => {
    emitJson(snapshot('stats'));
  }, config.statsIntervalMs ?? 1000);
  statsInterval.unref();

  let shuttingDown = false;
  const shutdown = async (): Promise<void> => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    clearInterval(statsInterval);
    await server.close();
    await sleep(100);
    emitJson(snapshot('summary'));
    process.exit(0);
  };

  process.on('SIGTERM', () => {
    void shutdown();
  });
  process.on('SIGINT', () => {
    void shutdown();
  });

  process.stdin.resume();
}

void main();
