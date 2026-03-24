/**
 * H3 long-haul stability tests (5-minute sustained load).
 * Gated by HTTP3_LONGHAUL=1 environment variable.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { snapshotMemory } from '../support/native-test-helpers.js';
import { createSecureServer, connectAsync } from '../../lib/index.js';
import type { Http3SecureServer, Http3ClientSession, ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';

interface DoRequestResult {
  status: string;
  headers: Record<string, string>;
  body: Buffer;
}

async function doRequest(
  session: Http3ClientSession,
  method: string,
  path: string,
  body?: Buffer,
): Promise<DoRequestResult> {
  // Retry on StreamBlocked — H3 may not have peer stream credits yet
  let stream: ClientHttp3Stream;
  for (let attempt = 0; ; attempt++) {
    try {
      stream = session.request({
        ':method': method,
        ':path': path,
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: !body });
      break;
    } catch (err: unknown) {
      if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
        await new Promise<void>((r) => { setTimeout(r, 5); });
        continue;
      }
      throw err;
    }
  }

  if (body) {
    stream.end(body);
  }

  return new Promise((resolve, reject) => {
    let status = '';
    const hdrs: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error(`doRequest ${method} ${path} timed out`)), 15000);

    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      for (const [k, v] of Object.entries(h)) {
        hdrs[k] = v;
      }
    });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve({ status, headers: hdrs, body: Buffer.concat(chunks) });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

function formatMB(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

describe('H3 sustained load (5 minutes)', { skip: !process.env.HTTP3_LONGHAUL }, () => {
  let certs: { key: Buffer; cert: Buffer };
  let server: Http3SecureServer;
  let port: number;

  before(async () => {
    certs = generateTestCerts();

    server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxIdleTimeoutMs: 600_000,
    }, (stream: ServerHttp3Stream, headers: IncomingHeaders, _flags: StreamFlags) => {
      const method = headers[':method'] as string;
      const path = headers[':path'] as string;

      if (method === 'GET') {
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end(path);
        return;
      }

      // POST: echo body
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        stream.respond({ ':status': '200' });
        stream.end(Buffer.concat(chunks));
      });
    });

    port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });
  });

  after(async () => {
    await server.close();
  });

  it('5-minute H3 request/response cycle with memory monitoring', { timeout: 330_000 }, async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const initialMem = snapshotMemory();
    console.log(`  [longhaul] initial RSS=${formatMB(initialMem.rss)}MB heap=${formatMB(initialMem.heapUsed)}MB`);

    const DURATION_S = 300;
    const BATCH_SIZE = 10;
    const CHECKPOINT_INTERVAL_S = 30;
    let totalRequests = 0;
    let errors = 0;
    const start = Date.now();
    let lastCheckpoint = start;

    while ((Date.now() - start) / 1000 < DURATION_S) {
      // Send a batch of 10 concurrent GET requests
      const batch = Array.from({ length: BATCH_SIZE }, (_, i) =>
        doRequest(session, 'GET', `/longhaul/${totalRequests + i}`).then(() => {
          totalRequests++;
        }).catch(() => {
          errors++;
          totalRequests++;
        }),
      );
      await Promise.all(batch);

      // Log memory snapshot every 30s
      const now = Date.now();
      if ((now - lastCheckpoint) / 1000 >= CHECKPOINT_INTERVAL_S) {
        const mem = snapshotMemory();
        const elapsedS = (now - start) / 1000;
        console.log(
          `  [longhaul] ${elapsedS.toFixed(0)}s: ` +
          `${totalRequests} reqs (${(totalRequests / elapsedS).toFixed(1)} req/s), ` +
          `${errors} errors, ` +
          `RSS=${formatMB(mem.rss)}MB heap=${formatMB(mem.heapUsed)}MB`,
        );
        lastCheckpoint = now;
      }
    }

    const elapsedS = (Date.now() - start) / 1000;
    const finalMem = snapshotMemory();

    console.log(
      `  [longhaul] DONE: ${totalRequests} total reqs in ${elapsedS.toFixed(1)}s ` +
      `(${(totalRequests / elapsedS).toFixed(1)} req/s), ${errors} errors`,
    );
    console.log(
      `  [longhaul] memory: initial RSS=${formatMB(initialMem.rss)}MB -> ` +
      `final RSS=${formatMB(finalMem.rss)}MB (${(finalMem.rss / initialMem.rss).toFixed(2)}x)`,
    );

    assert.ok(totalRequests > 1000, `expected > 1000 total requests, got ${totalRequests}`);
    // Check heap (not RSS) — RSS includes native allocator fragmentation and
    // kernel page cache which we don't control. Heap is what we can leak.
    assert.ok(
      finalMem.heapUsed < 100 * 1024 * 1024,
      `heap grew too much: ${formatMB(finalMem.heapUsed)}MB (limit 100MB)`,
    );

    await session.close();
  });

  it('5-minute idle connection survives without resource leak', { timeout: 330_000 }, async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      maxIdleTimeoutMs: 600_000, // 10 min — won't fire during test
    });

    const initialMem = snapshotMemory();
    console.log(`  [longhaul/idle] initial RSS=${formatMB(initialMem.rss)}MB`);

    // First idle phase: 150 seconds with 30s checkpoint logging
    for (let checkpoint = 1; checkpoint <= 5; checkpoint++) {
      await new Promise<void>((resolve) => { setTimeout(resolve, 30_000); });
      const mem = snapshotMemory();
      console.log(
        `  [longhaul/idle] ${checkpoint * 30}s: RSS=${formatMB(mem.rss)}MB ` +
        `heap=${formatMB(mem.heapUsed)}MB`,
      );
    }

    // Verify connection is still alive after 150s idle
    const res1 = await doRequest(session, 'GET', '/alive-check-1');
    assert.strictEqual(res1.status, '200', 'connection should be alive after 150s idle');
    console.log('  [longhaul/idle] connection alive at 150s');

    // Second idle phase: another 150 seconds
    for (let checkpoint = 6; checkpoint <= 10; checkpoint++) {
      await new Promise<void>((resolve) => { setTimeout(resolve, 30_000); });
      const mem = snapshotMemory();
      console.log(
        `  [longhaul/idle] ${checkpoint * 30}s: RSS=${formatMB(mem.rss)}MB ` +
        `heap=${formatMB(mem.heapUsed)}MB`,
      );
    }

    // Verify connection still alive after full 300s
    const res2 = await doRequest(session, 'GET', '/alive-check-2');
    assert.strictEqual(res2.status, '200', 'connection should be alive after 300s idle');
    console.log('  [longhaul/idle] connection alive at 300s');

    const finalMem = snapshotMemory();
    const rssGrowthMB = (finalMem.rss - initialMem.rss) / 1024 / 1024;
    console.log(
      `  [longhaul/idle] DONE: RSS growth=${rssGrowthMB.toFixed(1)}MB ` +
      `(${formatMB(initialMem.rss)}MB -> ${formatMB(finalMem.rss)}MB)`,
    );

    assert.ok(
      rssGrowthMB < 50,
      `RSS grew by ${rssGrowthMB.toFixed(1)}MB during idle (limit 50MB)`,
    );

    await session.close();
  });
});
