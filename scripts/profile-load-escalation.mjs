import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  createArtifactStamp,
  resolveResultsDir,
  writeJsonArtifact,
  writeTextArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const LOOPBACK_BINARY = resolve(ROOT_DIR, 'target', 'release', 'quic_loopback_profile');
const MOCK_SUSTAINED_BINARY = resolve(ROOT_DIR, 'target', 'release', 'quic_mock_sustained');

// ── Escalation tables ────────────────────────────────────────────────

const RUST_MOCK_LEVELS = [
  { streams: 10,   payloadBytes: 4096 },
  { streams: 50,   payloadBytes: 4096 },
  { streams: 100,  payloadBytes: 16384 },
  { streams: 200,  payloadBytes: 16384 },
  { streams: 500,  payloadBytes: 65536 },
  { streams: 1000, payloadBytes: 65536 },
];

const RUST_UDP_LEVELS = [
  { connections: 1,  streams: 10,  payloadBytes: 4096 },
  { connections: 2,  streams: 50,  payloadBytes: 4096 },
  { connections: 4,  streams: 100, payloadBytes: 16384 },
  { connections: 8,  streams: 200, payloadBytes: 16384 },
  { connections: 16, streams: 200, payloadBytes: 65536 },
  { connections: 32, streams: 200, payloadBytes: 65536 },
];

const NODE_FFI_LEVELS = [
  { connections: 5,  streams: 10,  messageSize: '4KB' },
  { connections: 10, streams: 50,  messageSize: '4KB' },
  { connections: 20, streams: 50,  messageSize: '16KB' },
  { connections: 20, streams: 100, messageSize: '16KB' },
  { connections: 40, streams: 100, messageSize: '64KB' },
  { connections: 40, streams: 200, messageSize: '64KB' },
];

const ALL_TIERS = ['rust-mock', 'rust-udp', 'node-ffi'];
const ALL_PROTOCOLS = ['quic', 'h3'];

// ── CLI parsing ──────────────────────────────────────────────────────

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '--skip-flamegraph') {
      flags.add(arg);
      continue;
    }

    if (
      arg === '--tiers' ||
      arg === '--protocols' ||
      arg === '--duration' ||
      arg === '--max-levels' ||
      arg === '--results-dir' ||
      arg === '--perf-frequency'
    ) {
      const value = argv[index + 1];
      if (!value || value.startsWith('--')) {
        throw new Error(`${arg} requires a value`);
      }
      options.set(arg, value);
      index += 1;
      continue;
    }

    throw new Error(`Unknown argument: ${arg}`);
  }

  return { options, flags };
}

function printHelp() {
  console.log(`Load escalation profiler — multi-tier perf + flamegraph capture

Usage:
  node scripts/profile-load-escalation.mjs [options]

Options:
  --tiers <list>         Comma-separated: rust-mock,rust-udp,node-ffi (default: all)
  --protocols <list>     Comma-separated: quic,h3 (default: quic,h3)
  --duration <secs>      Duration per level (default: 60)
  --max-levels <n>       Max escalation levels (default: 6)
  --results-dir <dir>    Output directory (default: results/load-escalation-<stamp>)
  --perf-frequency <hz>  Perf sampling frequency (default: 99)
  --skip-flamegraph      Skip flamegraph generation (just collect perf data)
  --help                 Show help

Examples:
  node scripts/profile-load-escalation.mjs --tiers rust-mock --protocols quic --duration 30
  node scripts/profile-load-escalation.mjs --tiers rust-mock,rust-udp --max-levels 4
  node scripts/profile-load-escalation.mjs --skip-flamegraph --perf-frequency 199
`);
}

function resolveSettings(argv) {
  const { options, flags } = parseArgs(argv);
  if (flags.has('--help')) {
    printHelp();
    process.exit(0);
  }

  const tierStr = options.get('--tiers') ?? ALL_TIERS.join(',');
  const tiers = tierStr.split(',').map((t) => t.trim());
  for (const t of tiers) {
    if (!ALL_TIERS.includes(t)) {
      throw new Error(`Unknown tier: ${t}. Valid tiers: ${ALL_TIERS.join(', ')}`);
    }
  }

  const protoStr = options.get('--protocols') ?? ALL_PROTOCOLS.join(',');
  const protocols = protoStr.split(',').map((p) => p.trim());
  for (const p of protocols) {
    if (!ALL_PROTOCOLS.includes(p)) {
      throw new Error(`Unknown protocol: ${p}. Valid protocols: ${ALL_PROTOCOLS.join(', ')}`);
    }
  }

  const duration = Number.parseInt(options.get('--duration') ?? '60', 10);
  if (!Number.isFinite(duration) || duration < 1) {
    throw new Error('--duration must be a positive integer');
  }

  const maxLevels = Number.parseInt(options.get('--max-levels') ?? '6', 10);
  if (!Number.isFinite(maxLevels) || maxLevels < 1) {
    throw new Error('--max-levels must be a positive integer');
  }

  const perfFrequency = Number.parseInt(options.get('--perf-frequency') ?? '99', 10);
  if (!Number.isFinite(perfFrequency) || perfFrequency < 1) {
    throw new Error('--perf-frequency must be a positive integer');
  }

  const defaultDir = `results/load-escalation-${createArtifactStamp()}`;
  const resultsDir = options.get('--results-dir') ?? defaultDir;

  return {
    tiers,
    protocols,
    duration,
    maxLevels,
    resultsDir,
    perfFrequency,
    skipFlamegraph: flags.has('--skip-flamegraph'),
  };
}

// ── Tool detection ───────────────────────────────────────────────────

function hasBinary(binary) {
  const result = spawnSync('which', [binary], { cwd: ROOT_DIR, stdio: 'ignore' });
  return result.status === 0;
}

function detectTooling() {
  const hasPerf = hasBinary('perf');
  const hasInferno = hasBinary('inferno-collapse-perf') && hasBinary('inferno-flamegraph');
  const hasOpenssl = hasBinary('openssl');
  if (!hasPerf) {
    console.log('WARNING: perf not found — profiling will be skipped, workloads run unprofiled');
  }
  if (!hasInferno) {
    console.log('WARNING: inferno-collapse-perf or inferno-flamegraph not found — flamegraph generation will be skipped');
  }
  return { hasPerf, hasInferno, hasOpenssl };
}

// ── Helpers ──────────────────────────────────────────────────────────

function runChecked(command, args, opts = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT_DIR,
    encoding: 'utf8',
    ...opts,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.slice(0, 3).join(' ')}... failed (status ${result.status})\n${result.stderr ?? ''}`);
  }
  return result;
}

function createTempDir(prefix) {
  return mkdtempSync(join(os.tmpdir(), prefix));
}

function generateCertPair(tempDir) {
  const keyPath = join(tempDir, 'server-key.pem');
  const certPath = join(tempDir, 'server-cert.pem');
  runChecked('openssl', [
    'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
    '-keyout', keyPath, '-out', certPath, '-days', '1', '-nodes', '-subj', '/CN=localhost',
  ]);
  return { keyPath, certPath };
}

function buildRustBinaries() {
  console.log('Building Rust release binaries...');
  runChecked('cargo', [
    'build', '--release',
    '--bin', 'quic_loopback_profile',
    '--bin', 'quic_multiclient_bench',
    '--no-default-features', '--features', 'profile-binary',
  ], { stdio: 'inherit' });
}

function ensureNodeBenchArtifacts() {
  const distTestDir = resolve(ROOT_DIR, 'dist-test', 'test');
  if (!existsSync(distTestDir)) {
    console.log('Building Node.js test artifacts (npm run build:test)...');
    runChecked('npm', ['run', 'build:test'], { stdio: 'inherit' });
  }
}

function fmtNum(n, digits = 1) {
  if (n == null) return '-';
  return Number(n).toFixed(digits);
}

function levelDir(baseDir, tier, protocol, levelIdx) {
  return join(baseDir, `${tier}-${protocol}`, `level-${levelIdx}`);
}

// ── JSON output parsing ──────────────────────────────────────────────

function parseJsonFromOutput(stdout) {
  const lines = stdout.trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    try {
      return JSON.parse(lines[i]);
    } catch { /* continue */ }
  }
  return null;
}

function parseResultLine(output) {
  const idx = output.indexOf('RESULT ');
  if (idx === -1) return null;
  const jsonStr = output.slice(idx + 'RESULT '.length).trim();
  return JSON.parse(jsonStr);
}

// ── Health checks (break conditions) ─────────────────────────────────

const ERROR_RATE_THRESHOLD = 0.02;
const THROUGHPUT_DROP_FACTOR = 0.50;

function checkHealth(metrics, prevMetrics) {
  if (!metrics) {
    return 'no metrics produced';
  }

  const errorCount = metrics.error_count ?? metrics.errors ?? metrics.errorCount ?? 0;
  const totalStreams = metrics.completed_streams ?? metrics.totalStreams ?? metrics.completedStreams ?? 0;
  const errorRate = totalStreams > 0 ? errorCount / totalStreams : 0;

  if (errorCount > 0 && errorRate > ERROR_RATE_THRESHOLD) {
    return `error rate ${(errorRate * 100).toFixed(1)}% > ${(ERROR_RATE_THRESHOLD * 100).toFixed(0)}% threshold (${errorCount} errors / ${totalStreams} streams)`;
  }

  if (prevMetrics) {
    const prevThroughput = prevMetrics.throughput_mbps ?? prevMetrics.throughputMbps ?? 0;
    const currThroughput = metrics.throughput_mbps ?? metrics.throughputMbps ?? 0;
    if (prevThroughput > 0 && currThroughput > 0 && currThroughput < prevThroughput * THROUGHPUT_DROP_FACTOR) {
      return `throughput dropped to ${fmtNum(currThroughput)} Mbps (${fmtNum((currThroughput / prevThroughput) * 100)}% of previous ${fmtNum(prevThroughput)} Mbps)`;
    }
  }

  return null;
}

// ── perf post-processing ─────────────────────────────────────────────

function generateFlamegraph(perfDataPath, outputSvg) {
  const collapse = spawnSync('sh', ['-c', `perf script -i "${perfDataPath}" | inferno-collapse-perf | inferno-flamegraph > "${outputSvg}"`], {
    cwd: ROOT_DIR,
    encoding: 'utf8',
    timeout: 300_000,
  });
  return collapse.status === 0;
}

function generatePerfReport(perfDataPath, outputTxt) {
  const result = spawnSync('perf', ['report', '--stdio', '--no-children', '-i', perfDataPath], {
    cwd: ROOT_DIR,
    encoding: 'utf8',
    timeout: 120_000,
  });
  if (result.status === 0) {
    writeFileSync(outputTxt, result.stdout);
    return true;
  }
  return false;
}

// ── Async process runner under perf record ───────────────────────────

function spawnUnderPerf(command, args, { perfDataPath, perfFrequency, timeoutMs, cwd = ROOT_DIR }) {
  const usePerf = Boolean(perfDataPath);
  let finalCommand;
  let finalArgs;

  if (usePerf) {
    finalCommand = 'perf';
    finalArgs = [
      'record', '-F', String(perfFrequency),
      '--call-graph', 'dwarf', '-g',
      '-o', perfDataPath,
      '--', command, ...args,
    ];
  } else {
    finalCommand = command;
    finalArgs = args;
  }

  return new Promise((resolvePromise, reject) => {
    const child = spawn(finalCommand, finalArgs, {
      cwd,
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    let stdout = '';
    let stderr = '';

    child.stdout.on('data', (data) => { stdout += data.toString(); });
    child.stderr.on('data', (data) => {
      stderr += data.toString();
      process.stderr.write(data);
    });

    const timer = setTimeout(() => {
      child.kill('SIGTERM');
      setTimeout(() => {
        try { child.kill('SIGKILL'); } catch { /* best effort */ }
      }, 5_000);
      reject(new Error(`Timeout after ${timeoutMs / 1000}s`));
    }, timeoutMs);

    child.on('error', (error) => { clearTimeout(timer); reject(error); });

    child.on('exit', (code, signal) => {
      clearTimeout(timer);
      if (signal) {
        reject(new Error(`killed by ${signal}\n${stderr}`.trim()));
        return;
      }
      resolvePromise({ code, stdout, stderr });
    });
  });
}

// ── Tier 1: rust-mock ────────────────────────────────────────────────

async function runRustMock(levelParams, protocol, settings, tools, outDir) {
  mkdirSync(outDir, { recursive: true });

  const perfDataPath = tools.hasPerf ? join(outDir, 'perf.data') : null;
  const timeoutMs = (settings.duration + 30) * 1000;

  // quic_mock_sustained runs continuous echo loops for the full duration (MockDriver, no real I/O)
  const args = [
    '--protocol', protocol === 'h3' ? 'h3' : 'quic',
    '--duration-secs', String(settings.duration),
    '--streams', String(levelParams.streams),
    '--payload-bytes', String(levelParams.payloadBytes),
    '--json',
  ];

  const result = await spawnUnderPerf(MOCK_SUSTAINED_BINARY, args, {
    perfDataPath,
    perfFrequency: settings.perfFrequency,
    timeoutMs,
  });

  const metrics = parseJsonFromOutput(result.stdout);
  return { metrics, exitCode: result.code, perfDataPath };
}

// ── Tier 2: rust-udp (server + client under perf) ────────────────────

async function runRustUdp(levelParams, protocol, settings, tools, outDir, certs) {
  mkdirSync(outDir, { recursive: true });

  const perfDataPath = tools.hasPerf ? join(outDir, 'perf.data') : null;
  const timeoutMs = (settings.duration + 30) * 1000;

  // Start server process (not under perf)
  const serverArgs = [
    'server',
    '--bind', '127.0.0.1:0',
    '--runtime-mode', 'fast',
    '--alpn', protocol === 'h3' ? 'h3' : 'quic',
    '--cert-path', certs.certPath,
    '--key-path', certs.keyPath,
  ];

  const server = spawn(LOOPBACK_BINARY, serverArgs, {
    cwd: ROOT_DIR,
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  let serverStdout = '';
  let serverStderr = '';
  server.stdout.setEncoding('utf8');
  server.stderr.setEncoding('utf8');
  server.stderr.on('data', (chunk) => { serverStderr += chunk; });

  // Wait for READY line
  const serverAddr = await new Promise((resolveAddr, rejectAddr) => {
    const readyTimer = setTimeout(() => {
      server.kill('SIGKILL');
      rejectAddr(new Error(`Server did not print READY within 15s\n${serverStderr}`));
    }, 15_000);

    server.stdout.on('data', (chunk) => {
      serverStdout += chunk;
      const lines = serverStdout.split(/\r?\n/u);
      for (const line of lines) {
        if (line.startsWith('READY ')) {
          clearTimeout(readyTimer);
          resolveAddr(line.slice('READY '.length).trim());
          return;
        }
      }
    });

    server.on('exit', (code) => {
      clearTimeout(readyTimer);
      rejectAddr(new Error(`Server exited before READY with code ${code}\n${serverStderr}`));
    });
  });

  // Run client under perf
  const clientArgs = [
    'client',
    '--server-addr', serverAddr,
    '--server-name', 'localhost',
    '--runtime-mode', 'fast',
    '--alpn', protocol === 'h3' ? 'h3' : 'quic',
    '--connections', String(levelParams.connections),
    '--streams-per-connection', String(levelParams.streams),
    '--payload-bytes', String(levelParams.payloadBytes),
    '--timeout-ms', String(settings.duration * 1000),
  ];

  let clientResult;
  try {
    clientResult = await spawnUnderPerf(LOOPBACK_BINARY, clientArgs, {
      perfDataPath,
      perfFrequency: settings.perfFrequency,
      timeoutMs,
    });
  } finally {
    // Shut down server gracefully
    server.stdin.write('stop\n');
    server.stdin.end();
    await new Promise((resolveExit) => {
      const killTimer = setTimeout(() => {
        try { server.kill('SIGKILL'); } catch { /* best effort */ }
      }, 10_000);
      server.on('exit', () => {
        clearTimeout(killTimer);
        resolveExit();
      });
    });
  }

  const metrics = parseResultLine(clientResult.stdout);
  return { metrics, exitCode: clientResult.code, perfDataPath };
}

// ── Tier 3: node-ffi ─────────────────────────────────────────────────

async function runNodeFfi(levelParams, protocol, settings, tools, outDir) {
  mkdirSync(outDir, { recursive: true });

  const perfDataPath = tools.hasPerf ? join(outDir, 'perf.data') : null;
  const timeoutMs = (settings.duration + 30) * 1000;

  const scriptName = protocol === 'h3' ? 'h3-benchmark.mjs' : 'quic-benchmark.mjs';
  const scriptPath = join(ROOT_DIR, 'scripts', scriptName);

  const args = [
    scriptPath,
    '--connections', String(levelParams.connections),
    '--streams-per-connection', String(levelParams.streams),
    '--message-size', levelParams.messageSize,
    '--runtime-mode', 'fast',
    '--json',
    '--allow-errors',
    '--timeout-ms', String(settings.duration * 1000),
  ];

  const result = await spawnUnderPerf(process.execPath, args, {
    perfDataPath,
    perfFrequency: settings.perfFrequency,
    timeoutMs,
  });

  const metrics = parseJsonFromOutput(result.stdout);
  return { metrics, exitCode: result.code, perfDataPath };
}

// ── Hotspot extraction from perf report ──────────────────────────────

function extractHotspots(reportTxt, topN = 10) {
  // Match lines like: "    12.34%  binary  libquiche.so  [.] quiche::stream::send"
  const pattern = /^\s*(\d+\.\d+)%\s+\S+\s+\S+\s+\[.\]\s+(.+)$/gmu;
  const hotspots = [];
  let match;
  while ((match = pattern.exec(reportTxt)) !== null) {
    hotspots.push({
      percentage: parseFloat(match[1]),
      function: match[2].trim(),
    });
    if (hotspots.length >= topN) break;
  }
  return hotspots;
}

function generateHotspotsMd(allSeriesResults, absoluteResultsDir) {
  const lines = ['# Hotspot Analysis\n'];
  lines.push(`Generated: ${new Date().toISOString()}\n`);

  for (const series of allSeriesResults) {
    const { tier, protocol, levels } = series;
    lines.push(`## ${tier} / ${protocol}\n`);

    const stableLevels = levels.filter((l) => !l.breakReason);
    const breakLevel = levels.find((l) => l.breakReason);

    if (stableLevels.length > 0) {
      const last = stableLevels[stableLevels.length - 1];
      if (last.hotspots && last.hotspots.length > 0) {
        lines.push(`### Last stable level (${last.levelIdx})\n`);
        lines.push('| % | Function |');
        lines.push('|---|----------|');
        for (const h of last.hotspots) {
          lines.push(`| ${h.percentage.toFixed(2)} | \`${h.function}\` |`);
        }
        lines.push('');
      }
    }

    if (breakLevel && breakLevel.hotspots && breakLevel.hotspots.length > 0) {
      lines.push(`### Break-point level (${breakLevel.levelIdx}) - ${breakLevel.breakReason}\n`);
      lines.push('| % | Function |');
      lines.push('|---|----------|');
      for (const h of breakLevel.hotspots) {
        lines.push(`| ${h.percentage.toFixed(2)} | \`${h.function}\` |`);
      }
      lines.push('');
    }

    if (levels.length === 0) {
      lines.push('_No levels completed._\n');
    }
  }

  const content = lines.join('\n') + '\n';
  const outputPath = join(absoluteResultsDir, 'hotspots.md');
  writeFileSync(outputPath, content);
  return outputPath;
}

// ── Main orchestrator ────────────────────────────────────────────────

function getLevelsForTier(tier, maxLevels) {
  let table;
  switch (tier) {
    case 'rust-mock': table = RUST_MOCK_LEVELS; break;
    case 'rust-udp':  table = RUST_UDP_LEVELS;  break;
    case 'node-ffi':  table = NODE_FFI_LEVELS;  break;
    default: throw new Error(`Unknown tier: ${tier}`);
  }
  return table.slice(0, maxLevels);
}

function describeLevelParams(tier, params) {
  if (tier === 'rust-mock') {
    return `streams=${params.streams}, payload=${params.payloadBytes}B`;
  }
  if (tier === 'rust-udp') {
    return `conns=${params.connections}, streams=${params.streams}, payload=${params.payloadBytes}B`;
  }
  return `conns=${params.connections}, streams=${params.streams}, msg=${params.messageSize}`;
}

async function runLevel(tier, protocol, levelIdx, levelParams, settings, tools, absoluteResultsDir, certs) {
  const outDir = levelDir(absoluteResultsDir, tier, protocol, levelIdx);
  mkdirSync(outDir, { recursive: true });

  let result;
  try {
    switch (tier) {
      case 'rust-mock':
        result = await runRustMock(levelParams, protocol, settings, tools, outDir);
        break;
      case 'rust-udp':
        result = await runRustUdp(levelParams, protocol, settings, tools, outDir, certs);
        break;
      case 'node-ffi':
        result = await runNodeFfi(levelParams, protocol, settings, tools, outDir);
        break;
      default:
        throw new Error(`Unknown tier: ${tier}`);
    }
  } catch (error) {
    return {
      levelIdx,
      params: levelParams,
      metrics: null,
      exitCode: null,
      breakReason: `workload failed: ${error.message.split('\n')[0]}`,
      hotspots: [],
      perfDataPath: null,
    };
  }

  // Non-zero exit is a break
  if (result.exitCode !== 0 && result.exitCode !== null) {
    return {
      levelIdx,
      params: levelParams,
      metrics: result.metrics,
      exitCode: result.exitCode,
      breakReason: `process exited with code ${result.exitCode}`,
      hotspots: [],
      perfDataPath: result.perfDataPath,
    };
  }

  // Write metrics.json
  const metricsPayload = { level: levelIdx, params: levelParams, output: result.metrics };
  writeFileSync(join(outDir, 'metrics.json'), JSON.stringify(metricsPayload, null, 2) + '\n');

  // Generate flamegraph
  let hotspots = [];
  if (result.perfDataPath && existsSync(result.perfDataPath)) {
    // perf report -> report.txt
    const reportPath = join(outDir, 'report.txt');
    if (generatePerfReport(result.perfDataPath, reportPath)) {
      const reportContent = readFileSync(reportPath, 'utf8');
      hotspots = extractHotspots(reportContent);
    }

    // flamegraph
    if (!settings.skipFlamegraph && tools.hasInferno) {
      const svgPath = join(outDir, 'flamegraph.svg');
      const ok = generateFlamegraph(result.perfDataPath, svgPath);
      if (ok) {
        console.log(`    flamegraph: ${svgPath}`);
      } else {
        console.log('    WARNING: flamegraph generation failed');
      }
    }
  }

  return {
    levelIdx,
    params: levelParams,
    metrics: result.metrics,
    exitCode: result.exitCode,
    breakReason: null,
    hotspots,
    perfDataPath: result.perfDataPath,
  };
}

async function main() {
  const settings = resolveSettings(process.argv.slice(2));
  const tools = detectTooling();
  const absoluteResultsDir = resolveResultsDir(ROOT_DIR, settings.resultsDir);
  mkdirSync(absoluteResultsDir, { recursive: true });

  console.log('Load escalation profiler');
  console.log(`  tiers:     ${settings.tiers.join(', ')}`);
  console.log(`  protocols: ${settings.protocols.join(', ')}`);
  console.log(`  duration:  ${settings.duration}s per level`);
  console.log(`  levels:    up to ${settings.maxLevels}`);
  console.log(`  perf freq: ${settings.perfFrequency} Hz`);
  console.log(`  results:   ${settings.resultsDir}`);
  console.log('');

  // Build prerequisites for selected tiers
  const needsRust = settings.tiers.some((t) => t === 'rust-mock' || t === 'rust-udp');
  const needsNode = settings.tiers.includes('node-ffi');

  if (needsRust) {
    buildRustBinaries();
  }
  if (needsNode) {
    ensureNodeBenchArtifacts();
  }

  // Generate certs for rust-udp tier
  let tempDir = null;
  let certs = null;
  if (settings.tiers.includes('rust-udp')) {
    if (!tools.hasOpenssl) {
      console.log('ERROR: openssl is required for rust-udp tier');
      process.exit(1);
    }
    tempDir = createTempDir('http3-escalation-');
    certs = generateCertPair(tempDir);
  }

  const allSeriesResults = [];
  const summaryRows = [];

  try {
    for (const tier of settings.tiers) {
      for (const protocol of settings.protocols) {
        const seriesKey = `${tier}/${protocol}`;
        const levels = getLevelsForTier(tier, settings.maxLevels);

        console.log(`\n${'='.repeat(80)}`);
        console.log(`${seriesKey} (${levels.length} levels)`);
        console.log('='.repeat(80));

        const seriesLevels = [];
        let prevMetrics = null;

        for (let i = 0; i < levels.length; i++) {
          const params = levels[i];
          console.log(`\n  [Level ${i}] ${describeLevelParams(tier, params)}`);

          const levelResult = await runLevel(
            tier, protocol, i, params, settings, tools, absoluteResultsDir, certs,
          );

          seriesLevels.push(levelResult);

          if (levelResult.metrics) {
            const throughput = levelResult.metrics.throughput_mbps ?? levelResult.metrics.throughputMbps ?? '-';
            const completed = levelResult.metrics.completed_streams ?? levelResult.metrics.totalStreams ?? levelResult.metrics.completedStreams ?? '-';
            console.log(`    throughput: ${fmtNum(throughput)} Mbps, streams: ${completed}`);
          }

          // Check break conditions
          if (levelResult.breakReason) {
            console.log(`    >>> BREAK: ${levelResult.breakReason}`);
            const breakPayload = {
              tier, protocol, level: i, params,
              reason: levelResult.breakReason,
              metrics: levelResult.metrics,
            };
            const breakPath = join(
              levelDir(absoluteResultsDir, tier, protocol, i),
              'break-point.json',
            );
            mkdirSync(dirname(breakPath), { recursive: true });
            writeFileSync(breakPath, JSON.stringify(breakPayload, null, 2) + '\n');
            break;
          }

          // Health check against previous level
          const healthIssue = checkHealth(levelResult.metrics, prevMetrics);
          if (healthIssue) {
            console.log(`    >>> BREAK: ${healthIssue}`);
            levelResult.breakReason = healthIssue;
            const breakPayload = {
              tier, protocol, level: i, params,
              reason: healthIssue,
              metrics: levelResult.metrics,
            };
            const breakPath = join(
              levelDir(absoluteResultsDir, tier, protocol, i),
              'break-point.json',
            );
            writeFileSync(breakPath, JSON.stringify(breakPayload, null, 2) + '\n');
            break;
          }

          prevMetrics = levelResult.metrics;
        }

        allSeriesResults.push({ tier, protocol, levels: seriesLevels });

        // Per-series summary line
        const completedLevels = seriesLevels.filter((l) => !l.breakReason);
        const breakLevel = seriesLevels.find((l) => l.breakReason);
        summaryRows.push({
          tier,
          protocol,
          completedLevels: completedLevels.length,
          totalLevels: seriesLevels.length,
          breakReason: breakLevel?.breakReason ?? null,
        });
      }
    }

    // Generate hotspots.md
    console.log(`\n${'='.repeat(80)}`);
    console.log('Post-processing');
    console.log('='.repeat(80));

    const hotspotsPath = generateHotspotsMd(allSeriesResults, absoluteResultsDir);
    console.log(`\n  hotspots: ${hotspotsPath}`);

    // Generate summary.json
    const summaryPayload = {
      artifactType: 'load-escalation',
      generatedAt: new Date().toISOString(),
      environment: captureEnvironmentMetadata({
        runner: 'scripts/profile-load-escalation.mjs',
        protocol: settings.protocols.join(','),
        target: 'multi-tier',
        extra: {
          tiers: settings.tiers,
          protocols: settings.protocols,
          durationSecs: settings.duration,
          maxLevels: settings.maxLevels,
          perfFrequency: settings.perfFrequency,
          skipFlamegraph: settings.skipFlamegraph,
        },
      }),
      series: allSeriesResults.map((s) => ({
        tier: s.tier,
        protocol: s.protocol,
        levels: s.levels.map((l) => ({
          levelIdx: l.levelIdx,
          params: l.params,
          breakReason: l.breakReason,
          hotspots: l.hotspots.slice(0, 5),
          metricsSnapshot: l.metrics ? {
            throughput_mbps: l.metrics.throughput_mbps ?? l.metrics.throughputMbps ?? null,
            completed_streams: l.metrics.completed_streams ?? l.metrics.totalStreams ?? l.metrics.completedStreams ?? null,
            error_count: l.metrics.error_count ?? l.metrics.errors ?? l.metrics.errorCount ?? null,
          } : null,
        })),
      })),
      summary: summaryRows,
    };

    const artifact = writeJsonArtifact({
      rootDir: ROOT_DIR,
      resultsDir: settings.resultsDir,
      prefix: 'load-escalation-summary',
      payload: summaryPayload,
    });

    if (artifact?.relativePath) {
      console.log(`  summary:  ${artifact.relativePath}`);
    }

    // Print final summary table
    console.log(`\n${'='.repeat(80)}`);
    console.log('Final summary');
    console.log('='.repeat(80));

    for (const row of summaryRows) {
      const status = row.breakReason
        ? `BREAK at level ${row.totalLevels - 1}: ${row.breakReason}`
        : `completed all ${row.completedLevels} levels`;
      console.log(`  ${row.tier}/${row.protocol}: ${status}`);
    }

    console.log(`\nResults: ${absoluteResultsDir}`);
  } finally {
    if (tempDir) {
      rmSync(tempDir, { recursive: true, force: true });
    }
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
