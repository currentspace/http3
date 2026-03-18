/**
 * Standalone H3 echo server for two-process benchmarking.
 * Mirrors quic-bench-server.ts but for HTTP/3.
 */

import { createSecureServer } from '../lib/index.js';
import { binding } from '../lib/event-loop.js';
import { generateTestCerts } from './generate-certs.js';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../lib/index.js';

interface BenchServerConfig {
  host?: string;
  port?: number;
  runtimeMode?: 'auto' | 'fast' | 'portable';
  fallbackPolicy?: 'error' | 'warn-and-fallback';
  statsIntervalMs?: number;
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
      const body = Buffer.concat(chunks);
      stream.respond({ ':status': '200' });
      stream.end(body);
    });
  });

  const addr = await new Promise<{ address: string; port: number }>((resolve) => {
    server.on('listening', () => {
      resolve(server.address()!);
    });
    server.listen(config.port ?? 0, config.host ?? '127.0.0.1');
  });

  process.stdout.write(JSON.stringify({
    type: 'ready',
    port: addr.port,
    address: addr.address,
    runtimeInfo: server.runtimeInfo,
  }) + '\n');

  const statsInterval = setInterval(() => {
    process.stdout.write(JSON.stringify({
      type: 'stats',
      streamCount,
      bytesEchoed,
      heapUsed: process.memoryUsage().heapUsed,
      rss: process.memoryUsage().rss,
      cpuUser: process.cpuUsage().user,
      cpuSystem: process.cpuUsage().system,
      runtimeInfo: server.runtimeInfo,
      reactorTelemetry: binding.runtimeTelemetry(),
    }) + '\n');
  }, config.statsIntervalMs ?? 1000);
  statsInterval.unref();

  process.on('SIGTERM', async () => {
    clearInterval(statsInterval);
    process.stdout.write(JSON.stringify({
      type: 'summary',
      streamCount,
      bytesEchoed,
      heapUsed: process.memoryUsage().heapUsed,
      rss: process.memoryUsage().rss,
      cpuUser: process.cpuUsage().user,
      cpuSystem: process.cpuUsage().system,
      runtimeInfo: server.runtimeInfo,
      reactorTelemetry: binding.runtimeTelemetry(),
    }) + '\n');
    await server.close();
    process.exit(0);
  });

  process.stdin.resume();
}

void main();
