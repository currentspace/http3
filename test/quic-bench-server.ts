/**
 * Standalone QUIC echo server for two-process benchmarking.
 * Communicates readiness via stdout JSON.
 *
 * Usage: node dist-test/test/quic-bench-server.js
 */

import { createQuicServer } from '../lib/index.js';
import { generateTestCerts } from './generate-certs.js';
import type { QuicServerSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';

async function main(): Promise<void> {
  const certs = generateTestCerts();

  const server = createQuicServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    maxConnections: 10_000,
    initialMaxStreamsBidi: 50_000,
  });

  let sessionCount = 0;
  let streamCount = 0;
  let bytesEchoed = 0;

  server.on('session', (session: QuicServerSession) => {
    sessionCount++;
    session.on('stream', (stream: QuicStream) => {
      streamCount++;
      stream.on('data', (chunk: Buffer) => {
        bytesEchoed += chunk.length;
      });
      stream.pipe(stream);
    });
  });

  const addr = await server.listen(0, '127.0.0.1');

  const readyMsg = JSON.stringify({
    type: 'ready',
    port: addr.port,
    address: addr.address,
  });
  process.stdout.write(readyMsg + '\n');

  const statsInterval = setInterval(() => {
    const msg = JSON.stringify({
      type: 'stats',
      sessionCount,
      streamCount,
      bytesEchoed,
      heapUsed: process.memoryUsage().heapUsed,
      rss: process.memoryUsage().rss,
      cpuUser: process.cpuUsage().user,
      cpuSystem: process.cpuUsage().system,
    });
    process.stdout.write(msg + '\n');
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
