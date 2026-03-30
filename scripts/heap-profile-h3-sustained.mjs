#!/usr/bin/env node
/**
 * Heap profiling harness for H3 sustained load.
 * Takes heap snapshots at intervals to identify retained objects.
 *
 * Run with: node --expose-gc scripts/heap-profile-h3-sustained.mjs
 */

import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { Session } from 'node:inspector/promises';
import { join } from 'node:path';

const RESULTS_DIR = process.argv[2] || 'results/heap-profile';
const DURATION_S = 60;       // 60s is enough to show the trend
const BATCH_SIZE = 10;
const SNAPSHOT_INTERVAL_S = 15;

// ── Load the library ──
const require_ = createRequire(import.meta.url);
const { createSecureServer, connectAsync } = require_('../dist/index.js');
const { generateTestCerts } = require_('../dist-test/test/support/generate-certs.js');

// ── Inspector session for heap snapshots ──
const inspector = new Session();
inspector.connect();

import { createWriteStream } from 'node:fs';

async function takeHeapSnapshot(label) {
  const path = join(RESULTS_DIR, `${label}.heapsnapshot`);
  const ws = createWriteStream(path);
  let size = 0;
  inspector.on('HeapProfiler.addHeapSnapshotChunk', (m) => {
    ws.write(m.params.chunk);
    size += m.params.chunk.length;
  });
  await inspector.post('HeapProfiler.takeHeapSnapshot', { reportProgress: false });
  inspector.removeAllListeners('HeapProfiler.addHeapSnapshotChunk');
  ws.end();
  await new Promise(r => ws.on('finish', r));
  console.log(`  snapshot: ${label} (${(size / 1024 / 1024).toFixed(1)}MB)`);
  return path;
}

async function getHeapStats() {
  // Force GC first
  if (global.gc) global.gc();
  await new Promise(r => setTimeout(r, 100));
  if (global.gc) global.gc();

  const mem = process.memoryUsage();
  return {
    rss: (mem.rss / 1024 / 1024).toFixed(1),
    heapUsed: (mem.heapUsed / 1024 / 1024).toFixed(1),
    heapTotal: (mem.heapTotal / 1024 / 1024).toFixed(1),
    external: (mem.external / 1024 / 1024).toFixed(1),
    arrayBuffers: (mem.arrayBuffers / 1024 / 1024).toFixed(1),
  };
}

// ── doRequest helper ──
async function doRequest(session, path) {
  for (let attempt = 0; ; attempt++) {
    try {
      const stream = session.request({
        ':method': 'GET',
        ':path': path,
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      return new Promise((resolve, reject) => {
        const chunks = [];
        const timer = setTimeout(() => reject(new Error('timeout')), 10000);
        stream.on('response', () => {});
        stream.on('data', (chunk) => chunks.push(chunk));
        stream.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
        stream.on('error', (err) => { clearTimeout(timer); reject(err); });
      });
    } catch (err) {
      if (attempt < 50 && err.message?.includes('StreamBlocked')) {
        await new Promise(r => setTimeout(r, 5));
        continue;
      }
      throw err;
    }
  }
}

// ── Main ──
async function main() {
  console.log(`Heap profiling H3 sustained load (${DURATION_S}s, snapshots every ${SNAPSHOT_INTERVAL_S}s)`);
  console.log(`Results: ${RESULTS_DIR}`);

  const certs = generateTestCerts();
  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
  });

  server.on('stream', (stream, headers) => {
    const body = `ok:${headers[':path']}`;
    stream.respond({ ':status': '200' });
    stream.end(body);
  });

  const port = await new Promise((resolve, reject) => {
    server.on('listening', () => resolve(server.address().port));
    server.on('error', reject);
    server.listen(0, '127.0.0.1');
  });
  const session = await connectAsync(`https://localhost:${port}`, {
    rejectUnauthorized: false,
    ca: certs.cert,
  });

  // Warmup
  for (let i = 0; i < 100; i++) {
    await doRequest(session, `/warmup/${i}`);
  }

  // Force GC after warmup
  if (global.gc) { global.gc(); global.gc(); }
  await new Promise(r => setTimeout(r, 500));

  const stats0 = await getHeapStats();
  console.log(`  baseline after warmup+GC: RSS=${stats0.rss}MB heap=${stats0.heapUsed}MB ext=${stats0.external}MB arrayBuf=${stats0.arrayBuffers}MB`);

  // Take baseline snapshot
  await takeHeapSnapshot('00-baseline');

  const start = Date.now();
  let totalRequests = 0;
  let snapshotCount = 1;
  let nextSnapshot = SNAPSHOT_INTERVAL_S * 1000;

  while (Date.now() - start < DURATION_S * 1000) {
    // Fire batch
    const promises = [];
    for (let i = 0; i < BATCH_SIZE; i++) {
      promises.push(doRequest(session, `/req/${totalRequests + i}`));
    }
    await Promise.all(promises);
    totalRequests += BATCH_SIZE;

    const elapsed = Date.now() - start;
    if (elapsed >= nextSnapshot) {
      // Force GC before snapshot to isolate true retention
      if (global.gc) { global.gc(); global.gc(); }
      await new Promise(r => setTimeout(r, 200));

      const stats = await getHeapStats();
      const label = String(snapshotCount).padStart(2, '0') + `-${Math.round(elapsed / 1000)}s`;
      console.log(`  ${label}: ${totalRequests} reqs (${(totalRequests / (elapsed / 1000)).toFixed(0)} req/s), RSS=${stats.rss}MB heap=${stats.heapUsed}MB ext=${stats.external}MB arrayBuf=${stats.arrayBuffers}MB`);
      await takeHeapSnapshot(label);
      snapshotCount++;
      nextSnapshot += SNAPSHOT_INTERVAL_S * 1000;
    }
  }

  // Final GC + snapshot
  if (global.gc) { global.gc(); global.gc(); }
  await new Promise(r => setTimeout(r, 500));
  const finalStats = await getHeapStats();
  console.log(`  final: ${totalRequests} reqs, RSS=${finalStats.rss}MB heap=${finalStats.heapUsed}MB ext=${finalStats.external}MB arrayBuf=${finalStats.arrayBuffers}MB`);
  await takeHeapSnapshot(`${String(snapshotCount).padStart(2, '0')}-final`);

  // Allocation sampling profile
  console.log('  starting allocation sampling...');
  await inspector.post('HeapProfiler.startSampling', { samplingInterval: 512 });

  // Run another burst with sampling active
  for (let i = 0; i < 5000; i++) {
    await doRequest(session, `/sampled/${i}`);
  }

  const { profile } = await inspector.post('HeapProfiler.stopSampling');
  writeFileSync(join(RESULTS_DIR, 'allocation-profile.json'), JSON.stringify(profile, null, 2));
  console.log('  allocation profile saved');

  await session.close();
  await server.close();

  inspector.disconnect();

  console.log('\nDone. Load snapshots in Chrome DevTools (Memory tab) to diff.');
  process.exit(0);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
