/**
 * Standalone H3 echo server for two-process benchmarking.
 * Mirrors quic-bench-server.ts but for HTTP/3.
 */

import { createSecureServer } from '../lib/index.js';
import { generateTestCerts } from './generate-certs.js';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../lib/index.js';

async function main(): Promise<void> {
  const certs = generateTestCerts();

  let streamCount = 0;
  let bytesEchoed = 0;

  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    initialMaxStreamsBidi: 50_000,
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
    server.listen(0, '127.0.0.1');
  });

  process.stdout.write(JSON.stringify({
    type: 'ready',
    port: addr.port,
    address: addr.address,
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
    }) + '\n');
  }, 1000);
  statsInterval.unref();

  process.on('SIGTERM', async () => {
    clearInterval(statsInterval);
    await server.close();
    process.exit(0);
  });

  process.stdin.resume();
}

void main();
