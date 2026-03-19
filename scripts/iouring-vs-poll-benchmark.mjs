import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const RESULTS_DIR = 'results/iouring-vs-poll';

// Escalation knobs — each iteration multiplies these onto the previous level
// Level 0: 2 * 10 * 50 * 3 = 3,000 streams (~10K with overhead)
const INITIAL = { clients: 2, connections: 10, streams: 50, messageSize: '4KB' };
const ESCALATION = { clients: 1.3, connections: 1.4, streams: 1.5 };
const MESSAGE_SIZES = ['4KB', '4KB', '16KB', '16KB', '64KB', '64KB', '128KB', '128KB', '256KB'];
const ROUNDS = 3;

// Stop conditions
const P95_SPIKE_FACTOR = 3.0;   // p95 latency jumped 3x vs previous level
const ERROR_RATE_THRESHOLD = 0.02; // 2% of streams errored
const MAX_ITERATIONS = 20;

const PROTOCOLS = ['quic', 'h3'];
const RUNTIMES = ['fast', 'portable'];

function scriptForProtocol(protocol) {
  return join(ROOT_DIR, 'scripts', protocol === 'h3' ? 'h3-benchmark.mjs' : 'quic-benchmark.mjs');
}

function runBenchmark(protocol, level, runtimeMode) {
  const timeoutMs = Math.max(120_000, level.totalStreams * 0.5);
  const label = `level${level.iteration}-${runtimeMode}`;
  const args = [
    scriptForProtocol(protocol),
    '--profile', 'balanced',
    '--client-processes', String(level.clients),
    '--connections', String(level.connections),
    '--streams-per-connection', String(level.streams),
    '--message-size', level.messageSize,
    '--rounds', String(ROUNDS),
    '--timeout-ms', String(timeoutMs),
    '--runtime-mode', runtimeMode,
    '--fallback-policy', 'error',
    '--results-dir', RESULTS_DIR,
    '--label', label,
    '--json',
    '--allow-errors',
  ];

  const outerTimeout = timeoutMs + 90_000;

  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, args, {
      cwd: ROOT_DIR,
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    let stdout = '';
    let stderr = '';

    child.stdout.on('data', (data) => { stdout += data.toString(); });
    child.stderr.on('data', (data) => {
      stderr += data.toString();
      process.stderr.write(data);
    });

    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`Timeout after ${outerTimeout / 1000}s`));
    }, outerTimeout);

    child.on('error', (error) => { clearTimeout(timeout); reject(error); });

    child.on('exit', (code, signal) => {
      clearTimeout(timeout);
      if (signal) {
        reject(new Error(`killed by ${signal}\n${stderr}`.trim()));
        return;
      }

      const lines = stdout.trim().split('\n');
      for (let i = lines.length - 1; i >= 0; i--) {
        try {
          const result = JSON.parse(lines[i]);
          resolve(result);
          return;
        } catch { /* continue */ }
      }

      if (code !== 0) {
        reject(new Error(`exited ${code}\n${stderr}`.trim()));
      } else {
        reject(new Error(`no JSON output\n${stdout}`.trim()));
      }
    });
  });
}

function avg(values) {
  if (!values || values.length === 0) return 0;
  return values.reduce((s, v) => s + v, 0) / values.length;
}

function extractMetrics(result) {
  if (!result) return null;
  const totalStreams = result.totalStreams ?? 0;
  const errors = result.errors ?? 0;
  return {
    throughputMbps: result.throughputMbps ?? 0,
    streamsPerSecond: result.streamsPerSecond ?? 0,
    latencyMeanMs: result.clientStats?.streamMeanMs ?? 0,
    latencyP50Ms: avg(result.clientStats?.streamP50s),
    latencyP95Ms: avg(result.clientStats?.streamP95s),
    latencyP99Ms: avg(result.clientStats?.streamP99s),
    errors,
    totalStreams,
    errorRate: totalStreams > 0 ? errors / totalStreams : 0,
    wallElapsedMs: result.wallElapsedMs ?? 0,
    clientCpuPct: result.clientStats?.totalCpuUtilizationPct ?? 0,
    serverCpuPct: result.serverStats?.cpuUtilizationPct ?? 0,
    peakRssMB: result.serverStats?.peakRssMB ?? 0,
    serverDriver: result.serverStats?.runtimeInfo?.driver ?? 'unknown',
  };
}

function fmtNum(n, digits = 1) {
  if (n == null) return '-';
  return Number(n).toFixed(digits);
}

function buildLevel(iteration) {
  const factor = Math.pow(1, 0); // start from INITIAL, then escalate
  const clients = Math.round(INITIAL.clients * Math.pow(ESCALATION.clients, iteration));
  const connections = Math.round(INITIAL.connections * Math.pow(ESCALATION.connections, iteration));
  const streams = Math.round(INITIAL.streams * Math.pow(ESCALATION.streams, iteration));
  const messageSize = MESSAGE_SIZES[Math.min(iteration, MESSAGE_SIZES.length - 1)];
  const totalStreams = clients * connections * streams * ROUNDS;
  return { iteration, clients, connections, streams, messageSize, totalStreams };
}

function shouldStop(current, previous, runtimeMode) {
  if (!current || !previous) return null;

  if (current.errorRate >= ERROR_RATE_THRESHOLD) {
    return `${runtimeMode} error rate ${(current.errorRate * 100).toFixed(1)}% >= ${ERROR_RATE_THRESHOLD * 100}% threshold`;
  }

  if (previous.latencyP95Ms > 0 && current.latencyP95Ms / previous.latencyP95Ms >= P95_SPIKE_FACTOR) {
    return `${runtimeMode} p95 latency spiked ${fmtNum(current.latencyP95Ms / previous.latencyP95Ms)}x (${fmtNum(previous.latencyP95Ms, 2)}ms -> ${fmtNum(current.latencyP95Ms, 2)}ms)`;
  }

  return null;
}

function printSummaryLine(protocol, runtime, level, metrics, elapsed) {
  const streams = level.totalStreams.toLocaleString();
  console.log(
    `  ${runtime.padEnd(9)} ${elapsed}s | ${fmtNum(metrics.throughputMbps)} Mbps, ` +
    `${fmtNum(metrics.streamsPerSecond, 0)} s/s, ` +
    `p95=${fmtNum(metrics.latencyP95Ms, 2)}ms, p99=${fmtNum(metrics.latencyP99Ms, 2)}ms, ` +
    `err=${metrics.errors}/${metrics.totalStreams} (${(metrics.errorRate * 100).toFixed(2)}%), ` +
    `cpu=${fmtNum(metrics.serverCpuPct)}%, driver=${metrics.serverDriver}`,
  );
}

async function main() {
  console.log('io_uring vs poll — Escalating load benchmark');
  console.log(`Stop when: p95 latency spikes ${P95_SPIKE_FACTOR}x or error rate >= ${ERROR_RATE_THRESHOLD * 100}%`);
  console.log(`Max iterations: ${MAX_ITERATIONS}, Rounds per level: ${ROUNDS}`);
  console.log(`Results: ${RESULTS_DIR}/`);
  console.log('');

  mkdirSync(join(ROOT_DIR, RESULTS_DIR), { recursive: true });

  const history = {}; // protocol:runtime -> [metrics, ...]
  const allResults = [];

  for (const protocol of PROTOCOLS) {
    const stopped = {}; // runtime -> reason

    console.log(`\n${'═'.repeat(100)}`);
    console.log(`Protocol: ${protocol.toUpperCase()}`);
    console.log('═'.repeat(100));

    for (let i = 0; i < MAX_ITERATIONS; i++) {
      // Stop if both runtimes have hit their limits
      if (RUNTIMES.every((r) => stopped[r])) {
        console.log(`\n  Both runtimes hit their limits — moving on.`);
        break;
      }

      const level = buildLevel(i);
      console.log(
        `\n[Level ${i}] ${level.clients} clients × ${level.connections} conns × ` +
        `${level.streams} streams × ${ROUNDS} rounds = ${level.totalStreams.toLocaleString()} streams, ` +
        `msg=${level.messageSize}`,
      );

      for (const runtime of RUNTIMES) {
        if (stopped[runtime]) {
          console.log(`  ${runtime.padEnd(9)} STOPPED: ${stopped[runtime]}`);
          continue;
        }

        const hkey = `${protocol}:${runtime}`;
        if (!history[hkey]) history[hkey] = [];

        const startTime = Date.now();
        try {
          const result = await runBenchmark(protocol, level, runtime);
          const elapsed = ((Date.now() - startTime) / 1000).toFixed(1);
          const metrics = extractMetrics(result);

          printSummaryLine(protocol, runtime, level, metrics, elapsed);

          const prev = history[hkey].length > 0 ? history[hkey][history[hkey].length - 1] : null;
          const stopReason = shouldStop(metrics, prev, runtime);

          history[hkey].push(metrics);
          allResults.push({ protocol, runtime, level: i, ...level, metrics });

          if (stopReason) {
            stopped[runtime] = stopReason;
            console.log(`  >>> STOP ${runtime}: ${stopReason}`);
          }
        } catch (error) {
          const elapsed = ((Date.now() - startTime) / 1000).toFixed(1);
          const msg = error.message.split('\n')[0];
          console.log(`  ${runtime.padEnd(9)} CRASHED in ${elapsed}s: ${msg}`);
          stopped[runtime] = `crashed: ${msg}`;
          allResults.push({ protocol, runtime, level: i, ...level, metrics: null, error: msg });
        }
      }
    }

    // Print per-protocol summary
    console.log(`\n${'─'.repeat(100)}`);
    console.log(`${protocol.toUpperCase()} summary:`);
    for (const runtime of RUNTIMES) {
      const hkey = `${protocol}:${runtime}`;
      const h = history[hkey] || [];
      if (h.length === 0) continue;
      const peak = h.reduce((best, m) => m.throughputMbps > best.throughputMbps ? m : best, h[0]);
      console.log(
        `  ${runtime.padEnd(9)} peak: ${fmtNum(peak.throughputMbps)} Mbps, ${fmtNum(peak.streamsPerSecond, 0)} s/s | ` +
        `stopped: ${stopped[runtime] ?? 'max iterations reached'}`,
      );
    }
  }

  // Save artifact
  const artifact = {
    artifactType: 'iouring-vs-poll-escalation',
    generatedAt: new Date().toISOString(),
    stopConditions: { p95SpikeFactor: P95_SPIKE_FACTOR, errorRateThreshold: ERROR_RATE_THRESHOLD },
    rounds: ROUNDS,
    initial: INITIAL,
    escalation: ESCALATION,
    results: allResults,
  };

  const artifactPath = join(ROOT_DIR, RESULTS_DIR, `escalation-${new Date().toISOString().replace(/[:.]/g, '-')}.json`);
  writeFileSync(artifactPath, JSON.stringify(artifact, null, 2) + '\n');
  console.log(`\nArtifact: ${artifactPath}`);
}

await main();
