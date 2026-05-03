/**
 * Standalone H3 echo server for two-process benchmarking.
 * Mirrors quic-bench-server.ts but for HTTP/3.
 */

import { createSecureServer } from '../../../lib/index.js';
import { binding } from '../../../lib/event-loop.js';
import { generateTestCerts } from '../generate-certs.js';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../../lib/index.js';

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

function waitForDrain(stream: ServerHttp3Stream): Promise<void> {
  return new Promise((resolve, reject) => {
    const cleanup = (): void => {
      stream.off('drain', onDrain);
      stream.off('error', onError);
      stream.off('close', onClose);
    };
    const onDrain = (): void => {
      cleanup();
      resolve();
    };
    const onError = (error: Error): void => {
      cleanup();
      reject(error);
    };
    const onClose = (): void => {
      cleanup();
      reject(new Error('stream closed before drain'));
    };
    stream.once('drain', onDrain);
    stream.once('error', onError);
    stream.once('close', onClose);
  });
}

function endAndWait(stream: ServerHttp3Stream, chunk?: Buffer): Promise<void> {
  return new Promise((resolve, reject) => {
    const cleanup = (): void => {
      stream.off('finish', onFinish);
      stream.off('error', onError);
      stream.off('close', onClose);
    };
    const onFinish = (): void => {
      cleanup();
      resolve();
    };
    const onError = (error: Error): void => {
      cleanup();
      reject(error);
    };
    const onClose = (): void => {
      cleanup();
      reject(new Error('stream closed before finish'));
    };
    stream.once('finish', onFinish);
    stream.once('error', onError);
    stream.once('close', onClose);
    if (chunk === undefined) {
      stream.end();
    } else {
      stream.end(chunk);
    }
  });
}

async function writeChunkedResponse(stream: ServerHttp3Stream, chunks: Buffer[]): Promise<void> {
  stream.respond({ ':status': '200' });

  if (chunks.length === 0) {
    await endAndWait(stream);
    return;
  }

  for (let index = 0; index < chunks.length - 1; index += 1) {
    if (!stream.write(chunks[index])) {
      await waitForDrain(stream);
    }
  }

  await endAndWait(stream, chunks[chunks.length - 1]);
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
    process.stderr.write(`Invalid h3 bench server config: ${message}\n`);
    process.exit(1);
  }
}

async function main(): Promise<void> {
  const config = loadConfig();
  binding.resetRuntimeTelemetry();
  const certs = generateTestCerts();

  let sessionCount = 0;
  let activeSessions = 0;
  let sessionsClosed = 0;
  let streamCount = 0;
  let bytesEchoed = 0;

  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    initialMaxStreamsBidi: 50_000,
    runtimeMode: config.runtimeMode,
    fallbackPolicy: config.fallbackPolicy,
  }, (stream: ServerHttp3Stream, _headers: IncomingHeaders, flags: StreamFlags) => {
    streamCount++;
    if (flags.endStream) {
      stream.respond({ ':status': '200' });
      stream.end();
      return;
    }

    const chunks: Buffer[] = [];
    stream.on('data', (c: Buffer) => {
      bytesEchoed += c.length;
      chunks.push(c);
    });
    stream.on('end', () => {
      void writeChunkedResponse(stream, chunks).catch((error) => {
        stream.destroy(error);
      });
    });
  });

  server.on('session', (session) => {
    sessionCount++;
    activeSessions++;
    session.once('close', () => {
      activeSessions = Math.max(0, activeSessions - 1);
      sessionsClosed++;
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

  const addr = await new Promise<{ address: string; port: number }>((resolve) => {
    server.on('listening', () => {
      resolve(server.address()!);
    });
    server.listen(config.port ?? 0, config.host ?? '127.0.0.1');
  });

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

  process.on('SIGTERM', async () => {
    clearInterval(statsInterval);
    await server.close();
    await sleep(100);
    emitJson(snapshot('summary'));
    process.exit(0);
  });

  process.stdin.resume();
}

void main();
