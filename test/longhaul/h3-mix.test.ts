/**
 * H3 mixed workload long-haul test (5-minute sustained mixed scenarios).
 * Gated by HTTP3_LONGHAUL=1 environment variable.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  startScenarioServer,
  connectScenarioClient,
  doRequest,
  LatencyTracker,
  pickWeighted,
} from '../support/scenario-helpers.js';

function formatMB(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

describe('H3 mixed workload (5 minutes)', { skip: !process.env.HTTP3_LONGHAUL }, () => {
  it('sustained mixed H3 scenario traffic with latency tracking', { timeout: 330_000 }, async () => {
    const { server, port } = await startScenarioServer();
    let session = await connectScenarioClient(port);

    const tracker = new LatencyTracker();
    const DURATION_S = 300;
    const CHECKPOINT_INTERVAL_S = 30;
    let totalRequests = 0;
    let errors = 0;
    const start = Date.now();
    let lastCheckpoint = start;

    const scenarios: Array<[string, number]> = [
      ['/health', 40],
      ['/api/users', 20],
      ['/api/users POST', 15],
      ['/files/small.txt', 10],
      ['/headers/echo', 5],
      ['/api/upload', 5],
      ['/concurrent', 5],
    ];

    while ((Date.now() - start) / 1000 < DURATION_S) {
      const scenario = pickWeighted(scenarios);
      const startTs = Date.now();

      try {
        if (scenario === '/api/users POST') {
          await doRequest(session, 'POST', '/api/users', Buffer.from(JSON.stringify({ name: 'test', id: totalRequests })));
        } else if (scenario === '/api/upload') {
          await doRequest(session, 'POST', '/api/upload', Buffer.alloc(4096, 0xab));
        } else {
          await doRequest(session, 'GET', scenario);
        }
        tracker.record(Date.now() - startTs);
        totalRequests++;
      } catch {
        errors++;
        totalRequests++;
        // If session is dead, reconnect before continuing.
        try {
          await doRequest(session, 'GET', '/health');
        } catch {
          // Session is truly dead — reconnect.
          try { await session.close(); } catch { /* ignore */ }
          try {
            session = await connectScenarioClient(port);
          } catch {
            // Server may be overwhelmed — wait before retrying.
            await new Promise(r => setTimeout(r, 500));
            session = await connectScenarioClient(port);
          }
        }
      }

      // Log stats every 30s
      const now = Date.now();
      if ((now - lastCheckpoint) / 1000 >= CHECKPOINT_INTERVAL_S) {
        if (typeof global.gc === 'function') global.gc();
        const mem = process.memoryUsage();
        const elapsedS = (now - start) / 1000;
        const stats = tracker.stats();
        console.log(
          `  [h3-mix] ${elapsedS.toFixed(0)}s: ` +
          `${totalRequests} reqs (${(totalRequests / elapsedS).toFixed(1)} req/s), ` +
          `${errors} errors, ` +
          `p50=${stats.p50.toFixed(0)}ms p95=${stats.p95.toFixed(0)}ms p99=${stats.p99.toFixed(0)}ms, ` +
          `RSS=${formatMB(mem.rss)}MB heap=${formatMB(mem.heapUsed)}MB`,
        );
        lastCheckpoint = now;
      }
    }

    const elapsedS = (Date.now() - start) / 1000;
    if (typeof global.gc === 'function') global.gc();
    const finalMem = process.memoryUsage();
    const total = totalRequests;
    const errorRate = total > 0 ? (errors / total) * 100 : 0;
    const stats = tracker.stats();

    console.log(
      `  [h3-mix] DONE: ${totalRequests} reqs in ${elapsedS.toFixed(1)}s ` +
      `(${(totalRequests / elapsedS).toFixed(1)} req/s), ${errors} errors (${errorRate.toFixed(2)}%)`,
    );
    console.log(
      `  [h3-mix] latency: p50=${stats.p50.toFixed(0)}ms p95=${stats.p95.toFixed(0)}ms p99=${stats.p99.toFixed(0)}ms`,
    );
    console.log(
      `  [h3-mix] memory: RSS=${formatMB(finalMem.rss)}MB heap=${formatMB(finalMem.heapUsed)}MB`,
    );

    assert.ok(
      errorRate < 1,
      `error rate ${errorRate.toFixed(2)}% exceeds 1% limit (${errors}/${total})`,
    );
    assert.ok(
      finalMem.heapUsed < 100 * 1024 * 1024,
      `heap too large: ${formatMB(finalMem.heapUsed)}MB (limit 100MB)`,
    );

    await session.close();
    await server.close();
  });
});
