/**
 * Side-by-side H3 vs QUIC two-process profiling.
 * Same workloads, same measurement, head-to-head comparison.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { spawn } from 'node:child_process';
import { join } from 'node:path';

const DIST_TEST = join(__dirname, '..');

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

function startServer(script: string): Promise<{ port: number; kill: () => void }> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [join(DIST_TEST, script)], {
      stdio: ['pipe', 'pipe', 'inherit'],
    });
    const timeout = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('Server startup timed out')); }, 10_000);
    let buffer = '';
    child.stdout!.on('data', (data: Buffer) => {
      buffer += data.toString();
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line);
          if (msg.type === 'ready') {
            clearTimeout(timeout);
            resolve({ port: msg.port, kill: () => { child.kill('SIGTERM'); } });
          }
        } catch { /* skip */ }
      }
    });
    child.on('error', (err) => { clearTimeout(timeout); reject(err); });
    child.on('exit', (code) => { if (code && code !== 0) { clearTimeout(timeout); reject(new Error(`exit ${code}`)); } });
  });
}

function runClient(
  script: string,
  config: { port: number; connections: number; streamsPerConnection: number; messageSize: number; timeoutMs: number },
): Promise<BenchResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [join(DIST_TEST, script), JSON.stringify(config)], {
      stdio: ['pipe', 'pipe', 'inherit'],
    });
    const timeout = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('Client timed out')); }, config.timeoutMs + 10_000);
    let buffer = '';
    child.stdout!.on('data', (data: Buffer) => {
      buffer += data.toString();
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line);
          if (msg.type === 'result') { clearTimeout(timeout); resolve(msg); }
        } catch { /* skip */ }
      }
    });
    child.on('error', (err) => { clearTimeout(timeout); reject(err); });
    child.on('exit', (code) => { clearTimeout(timeout); if (code && code !== 0) reject(new Error(`exit ${code}`)); });
  });
}

function printComparison(label: string, h3: BenchResult, quic: BenchResult): void {
  console.log(`\n  ══ ${label} ══`);
  console.log(`  ${'Metric'.padEnd(24)} ${'H3'.padStart(14)} ${'QUIC'.padStart(14)} ${'Delta'.padStart(10)}`);
  console.log(`  ${'─'.repeat(62)}`);

  const row = (name: string, h3v: number, qv: number, unit: string) => {
    const delta = qv - h3v;
    const pct = h3v !== 0 ? ((delta / h3v) * 100).toFixed(0) : 'n/a';
    const sign = delta > 0 ? '+' : '';
    console.log(`  ${name.padEnd(24)} ${(h3v + unit).padStart(14)} ${(qv + unit).padStart(14)} ${(sign + pct + '%').padStart(10)}`);
  };

  row('Streams completed', h3.totalStreams, quic.totalStreams, '');
  row('Errors', h3.errors, quic.errors, '');
  row('Elapsed', h3.elapsedMs, quic.elapsedMs, 'ms');
  row('Throughput', h3.throughputMbps, quic.throughputMbps, 'Mbps');
  row('Streams/sec', h3.streamsPerSecond, quic.streamsPerSecond, '');
  row('Conn setup mean', h3.connEstablish.meanMs, quic.connEstablish.meanMs, 'ms');
  row('Conn setup p95', h3.connEstablish.p95Ms, quic.connEstablish.p95Ms, 'ms');
  row('Latency p50', h3.streamLatency.p50Ms, quic.streamLatency.p50Ms, 'ms');
  row('Latency p95', h3.streamLatency.p95Ms, quic.streamLatency.p95Ms, 'ms');
  row('Latency p99', h3.streamLatency.p99Ms, quic.streamLatency.p99Ms, 'ms');
  row('Latency max', h3.streamLatency.maxMs, quic.streamLatency.maxMs, 'ms');
  row('CPU user', h3.cpu.userMs, quic.cpu.userMs, 'ms');
  row('CPU system', h3.cpu.systemMs, quic.cpu.systemMs, 'ms');
  row('CPU util', h3.cpu.utilizationPct, quic.cpu.utilizationPct, '%');
  row('Heap delta', h3.memory.heapDeltaMB, quic.memory.heapDeltaMB, 'MB');
  row('RSS', h3.memory.rssMB, quic.memory.rssMB, 'MB');

  // Bottleneck analysis
  console.log('');
  for (const [name, r] of [['H3', h3], ['QUIC', quic]] as const) {
    if (r.cpu.utilizationPct > 80) {
      const where = r.cpu.systemMs > r.cpu.userMs ? 'kernel (UDP syscalls)' : 'userspace (crypto/dispatch)';
      console.log(`  ⚠ ${name}: CPU-bound ${r.cpu.utilizationPct}% — ${where}`);
    }
    if (r.streamLatency.p99Ms > r.streamLatency.p50Ms * 5) {
      console.log(`  ⚠ ${name}: Tail spike p99=${r.streamLatency.p99Ms}ms vs p50=${r.streamLatency.p50Ms}ms`);
    }
    if (r.memory.heapDeltaMB > 20) {
      console.log(`  ⚠ ${name}: High heap growth +${r.memory.heapDeltaMB}MB`);
    }
  }
}

async function runPair(
  label: string,
  config: { connections: number; streamsPerConnection: number; messageSize: number; timeoutMs: number },
): Promise<{ h3: BenchResult; quic: BenchResult }> {
  // Run H3
  const h3Server = await startServer('test/h3-bench-server.js');
  const h3Result = await runClient('test/h3-bench-client.js', { ...config, port: h3Server.port });
  h3Server.kill();

  // Run QUIC
  const quicServer = await startServer('test/quic-bench-server.js');
  const quicResult = await runClient('test/quic-bench-client.js', { ...config, port: quicServer.port });
  quicServer.kill();

  printComparison(label, h3Result, quicResult);
  return { h3: h3Result, quic: quicResult };
}

describe('H3 vs QUIC two-process profiling', () => {
  it('10 conns × 50 streams × 4KB', async () => {
    const { h3, quic } = await runPair('10×50×4KB', {
      connections: 10, streamsPerConnection: 50, messageSize: 4096, timeoutMs: 30_000,
    });
    assert.strictEqual(h3.errors, 0, `H3 had ${h3.errors} errors`);
    assert.strictEqual(quic.errors, 0, `QUIC had ${quic.errors} errors`);
  });

  it('1 conn × 200 streams × 1KB (burst)', async () => {
    const { h3, quic } = await runPair('1×200×1KB burst', {
      connections: 1, streamsPerConnection: 200, messageSize: 1024, timeoutMs: 30_000,
    });
    assert.strictEqual(h3.errors, 0, `H3 had ${h3.errors} errors`);
    assert.strictEqual(quic.errors, 0, `QUIC had ${quic.errors} errors`);
  });

  it('20 conns × 10 streams × 16KB', async () => {
    const { h3, quic } = await runPair('20×10×16KB', {
      connections: 20, streamsPerConnection: 10, messageSize: 16384, timeoutMs: 30_000,
    });
    assert.strictEqual(h3.errors, 0, `H3 had ${h3.errors} errors`);
    assert.strictEqual(quic.errors, 0, `QUIC had ${quic.errors} errors`);
  });
});
