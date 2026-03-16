/**
 * Two-process QUIC profiling test.
 *
 * Spawns server and client in separate Node.js processes to:
 * 1. Isolate GC / event-loop interference between server and client
 * 2. Measure true latency distribution (p50/p95/p99)
 * 3. Identify bottlenecks (CPU, memory, connection establishment)
 * 4. Exercise OS network stack realistically (separate UDP sockets)
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { spawn } from 'node:child_process';
import { join } from 'node:path';
const DIST_TEST = join(__dirname, '..');

interface ServerReady {
  type: 'ready';
  port: number;
  address: string;
}

interface BenchResult {
  type: 'result';
  config: { connections: number; streamsPerConnection: number; messageSize: number };
  totalStreams: number;
  totalBytes: number;
  errors: number;
  elapsedMs: number;
  throughputMbps: number;
  streamsPerSecond: number;
  connEstablish: { count: number; meanMs: number; p50Ms: number; p95Ms: number; p99Ms: number; maxMs: number };
  streamLatency: { count: number; meanMs: number; p50Ms: number; p95Ms: number; p99Ms: number; maxMs: number; minMs: number };
  cpu: { userMs: number; systemMs: number; totalMs: number; utilizationPct: number };
  memory: { heapUsedStart: number; heapUsedEnd: number; heapDeltaMB: number; rssEnd: number; rssMB: number };
}

function startServer(): Promise<{ port: number; kill: () => void }> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [join(DIST_TEST, 'test/quic-bench-server.js')], {
      stdio: ['pipe', 'pipe', 'inherit'],
    });

    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error('Server startup timed out'));
    }, 10_000);

    let buffer = '';
    child.stdout!.on('data', (data: Buffer) => {
      buffer += data.toString();
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line) as ServerReady;
          if (msg.type === 'ready') {
            clearTimeout(timeout);
            resolve({
              port: msg.port,
              kill: () => { child.kill('SIGTERM'); },
            });
          }
        } catch { /* skip non-JSON lines */ }
      }
    });

    child.on('error', (err) => {
      clearTimeout(timeout);
      reject(err);
    });
    child.on('exit', (code) => {
      if (code && code !== 0) {
        clearTimeout(timeout);
        reject(new Error(`Server exited with code ${code}`));
      }
    });
  });
}

function runClient(config: {
  port: number;
  connections: number;
  streamsPerConnection: number;
  messageSize: number;
  timeoutMs: number;
}): Promise<BenchResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [join(DIST_TEST, 'test/quic-bench-client.js'), JSON.stringify(config)],
      { stdio: ['pipe', 'pipe', 'inherit'] },
    );

    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error('Client timed out'));
    }, config.timeoutMs + 10_000);

    let buffer = '';
    child.stdout!.on('data', (data: Buffer) => {
      buffer += data.toString();
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line) as BenchResult;
          if (msg.type === 'result') {
            clearTimeout(timeout);
            resolve(msg);
          }
        } catch { /* skip non-JSON */ }
      }
    });

    child.on('error', (err) => {
      clearTimeout(timeout);
      reject(err);
    });
    child.on('exit', (code) => {
      clearTimeout(timeout);
      if (code && code !== 0) {
        reject(new Error(`Client exited with code ${code}`));
      }
    });
  });
}

function printResult(label: string, r: BenchResult): void {
  console.log(`\n  ── ${label} ──`);
  console.log(`  Config:     ${r.config.connections} conns × ${r.config.streamsPerConnection} streams × ${(r.config.messageSize / 1024).toFixed(0)}KB`);
  console.log(`  Streams:    ${r.totalStreams} completed, ${r.errors} errors`);
  console.log(`  Duration:   ${r.elapsedMs}ms`);
  console.log(`  Throughput: ${r.throughputMbps} Mbps, ${r.streamsPerSecond} streams/s`);
  console.log(`  Conn setup: mean=${r.connEstablish.meanMs}ms p95=${r.connEstablish.p95Ms}ms max=${r.connEstablish.maxMs}ms`);
  console.log(`  Latency:    mean=${r.streamLatency.meanMs}ms p50=${r.streamLatency.p50Ms}ms p95=${r.streamLatency.p95Ms}ms p99=${r.streamLatency.p99Ms}ms max=${r.streamLatency.maxMs}ms`);
  console.log(`  CPU:        user=${r.cpu.userMs}ms sys=${r.cpu.systemMs}ms util=${r.cpu.utilizationPct}%`);
  console.log(`  Memory:     heap_delta=${r.memory.heapDeltaMB}MB rss=${r.memory.rssMB}MB`);

  // Bottleneck analysis
  if (r.cpu.utilizationPct > 80) {
    console.log(`  ⚠ BOTTLENECK: CPU-bound (${r.cpu.utilizationPct}% utilization)`);
    if (r.cpu.systemMs > r.cpu.userMs) {
      console.log(`    → Kernel syscalls dominate (system > user) — likely UDP send/recv`);
    } else {
      console.log(`    → Userspace dominates — likely quiche crypto or JS event dispatch`);
    }
  }
  if (r.streamLatency.p99Ms > r.streamLatency.p50Ms * 10) {
    console.log(`  ⚠ BOTTLENECK: Tail latency spike (p99=${r.streamLatency.p99Ms}ms vs p50=${r.streamLatency.p50Ms}ms)`);
    console.log(`    → Likely GC pauses or flow-control stalls`);
  }
  if (r.connEstablish.p95Ms > 100) {
    console.log(`  ⚠ BOTTLENECK: Slow connection setup (p95=${r.connEstablish.p95Ms}ms)`);
    console.log(`    → TLS handshake or socket bind contention`);
  }
  if (r.memory.heapDeltaMB > 50) {
    console.log(`  ⚠ BOTTLENECK: High memory growth (+${r.memory.heapDeltaMB}MB heap)`);
    console.log(`    → Possible stream/buffer leak`);
  }
}

describe('QUIC two-process profiling', () => {
  it('profile: 10 conns × 50 streams × 4KB', async () => {
    const server = await startServer();
    try {
      const result = await runClient({
        port: server.port,
        connections: 10,
        streamsPerConnection: 50,
        messageSize: 4096,
        timeoutMs: 30_000,
      });
      printResult('10×50×4KB', result);
      assert.strictEqual(result.errors, 0);
      assert.strictEqual(result.totalStreams, 500);
    } finally {
      server.kill();
    }
  });

  it('profile: 20 conns × 20 streams × 16KB', async () => {
    const server = await startServer();
    try {
      const result = await runClient({
        port: server.port,
        connections: 20,
        streamsPerConnection: 20,
        messageSize: 16384,
        timeoutMs: 60_000,
      });
      printResult('20×20×16KB', result);
      assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
      assert.strictEqual(result.totalStreams, 400);
    } finally {
      server.kill();
    }
  });

  it('profile: 1 conn × 500 streams × 1KB (single-conn burst)', async () => {
    const server = await startServer();
    try {
      const result = await runClient({
        port: server.port,
        connections: 1,
        streamsPerConnection: 500,
        messageSize: 1024,
        timeoutMs: 30_000,
      });
      printResult('1×500×1KB burst', result);
      assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
      assert.strictEqual(result.totalStreams, 500);
    } finally {
      server.kill();
    }
  });

  it('profile: 50 conns × 1 stream × 64KB (conn-heavy)', async () => {
    const server = await startServer();
    try {
      const result = await runClient({
        port: server.port,
        connections: 50,
        streamsPerConnection: 1,
        messageSize: 65536,
        timeoutMs: 60_000,
      });
      printResult('50×1×64KB conn-heavy', result);
      assert.strictEqual(result.errors, 0, `expected 0 errors, got ${result.errors}`);
      assert.strictEqual(result.totalStreams, 50);
    } finally {
      server.kill();
    }
  });

  it('profile: 50 conns × 5 streams × 4KB (high-conn diagnostic)', async () => {
    const server = await startServer();
    try {
      const totalExpected = 50 * 5;
      const result = await runClient({
        port: server.port,
        connections: 50,
        streamsPerConnection: 5,
        messageSize: 4096,
        timeoutMs: 60_000,
      });
      printResult('50x5x4KB high-conn', result);

      // Diagnostic breakdown
      const errorRate = result.errors / totalExpected;
      console.log(`  Error rate: ${(errorRate * 100).toFixed(1)}% (${result.errors}/${totalExpected})`);
      if (result.errors > 0) {
        console.log(`  Error categorization: check stderr output from client process`);
      }

      // Allow up to 5% error rate for OS-level packet loss at high conn counts
      const maxErrors = Math.ceil(totalExpected * 0.05);
      assert.ok(
        result.errors <= maxErrors,
        `error rate ${(errorRate * 100).toFixed(1)}% exceeds 5% tolerance (${result.errors} errors, max ${maxErrors})`,
      );
    } finally {
      server.kill();
    }
  });

  it('profile: 100 conns × 1 stream × 1KB (conn-establishment stress)', async () => {
    const server = await startServer();
    try {
      const totalExpected = 100;
      const result = await runClient({
        port: server.port,
        connections: 100,
        streamsPerConnection: 1,
        messageSize: 1024,
        timeoutMs: 60_000,
      });
      printResult('100x1x1KB conn-stress', result);

      const errorRate = result.errors / totalExpected;
      console.log(`  Error rate: ${(errorRate * 100).toFixed(1)}% (${result.errors}/${totalExpected})`);

      const maxErrors = Math.ceil(totalExpected * 0.05);
      assert.ok(
        result.errors <= maxErrors,
        `error rate ${(errorRate * 100).toFixed(1)}% exceeds 5% tolerance (${result.errors} errors, max ${maxErrors})`,
      );
    } finally {
      server.kill();
    }
  });
});
