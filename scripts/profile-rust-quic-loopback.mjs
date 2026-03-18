import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import os from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  writeJsonArtifact,
  writeTextArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BINARY_PATH = resolve(ROOT_DIR, 'target', 'release', 'quic_loopback_profile');

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (
      arg === '--help' ||
      arg === '--no-build' ||
      arg === '--perf-stat' ||
      arg === '--perf-record' ||
      arg === '--capture-trace'
    ) {
      flags.add(arg);
      continue;
    }

    if (
      arg === '--runtime-mode' ||
      arg === '--connections' ||
      arg === '--streams-per-connection' ||
      arg === '--payload-bytes' ||
      arg === '--timeout-ms' ||
      arg === '--results-dir' ||
      arg === '--label' ||
      arg === '--profile-role' ||
      arg === '--perf-record-call-graph' ||
      arg === '--perf-record-frequency' ||
      arg === '--bind' ||
      arg === '--alpn' ||
      arg === '--server-name'
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
  console.log(`Linux Rust QUIC loopback profiler

Usage:
  node scripts/profile-rust-quic-loopback.mjs [options]

Options:
  --runtime-mode fast|portable         Driver mode for both roles (default: fast)
  --connections N                      Client session count (default: 1)
  --streams-per-connection N           Streams per session (default: 100)
  --payload-bytes N                    Bytes per request stream (default: 16384)
  --timeout-ms N                       Client timeout (default: 30000)
  --bind HOST:PORT                     Server bind address (default: 127.0.0.1:0)
  --alpn VALUE                         ALPN string (default: quic)
  --server-name VALUE                  Client SNI / hostname (default: localhost)
  --results-dir DIR                    Artifact directory (default: perf-results)
  --label TEXT                         Optional artifact label suffix
  --profile-role server|client         Which role to wrap with perf (default: none)
  --perf-stat                          Run perf stat on the selected role
  --perf-record                        Run perf record/report on the selected role
  --capture-trace                      Persist a replayable command/event trace
  --perf-record-call-graph MODE        perf record call graph mode (default: dwarf)
  --perf-record-frequency N            perf record frequency (default: 199)
  --no-build                           Reuse the existing release binary
  --help                               Show this help

Examples:
  node scripts/profile-rust-quic-loopback.mjs --runtime-mode fast --connections 10
  node scripts/profile-rust-quic-loopback.mjs --perf-record --profile-role client --label client-fast
`);
}

function requireBinary(binary) {
  const result = spawnSync('which', [binary], {
    cwd: ROOT_DIR,
    stdio: 'ignore',
  });
  if (result.status !== 0) {
    throw new Error(`Required tool not found on PATH: ${binary}`);
  }
}

function resolveSettings(argv) {
  const { options, flags } = parseArgs(argv);
  if (flags.has('--help')) {
    printHelp();
    process.exit(0);
  }

  if (process.platform !== 'linux') {
    throw new Error('This runner only supports Linux hosts.');
  }

  const perfModes = [flags.has('--perf-stat'), flags.has('--perf-record')].filter(Boolean).length;
  if (perfModes > 1) {
    throw new Error('Choose either --perf-stat or --perf-record, not both.');
  }

  const profileRole = options.get('--profile-role') ?? null;
  if (profileRole && profileRole !== 'server' && profileRole !== 'client') {
    throw new Error('--profile-role must be server or client');
  }
  if (perfModes === 1 && !profileRole) {
    throw new Error('--profile-role is required when perf profiling is enabled');
  }

  requireBinary('openssl');
  if (flags.has('--perf-stat') || flags.has('--perf-record')) {
    requireBinary('perf');
  }

  return {
    runtimeMode: options.get('--runtime-mode') ?? 'fast',
    connections: Number.parseInt(options.get('--connections') ?? '1', 10),
    streamsPerConnection: Number.parseInt(options.get('--streams-per-connection') ?? '100', 10),
    payloadBytes: Number.parseInt(options.get('--payload-bytes') ?? '16384', 10),
    timeoutMs: Number.parseInt(options.get('--timeout-ms') ?? '30000', 10),
    bindAddr: options.get('--bind') ?? '127.0.0.1:0',
    alpn: options.get('--alpn') ?? 'quic',
    serverName: options.get('--server-name') ?? 'localhost',
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    noBuild: flags.has('--no-build'),
    captureTrace: flags.has('--capture-trace'),
    profileRole,
    perfMode: flags.has('--perf-stat') ? 'perf-stat' : flags.has('--perf-record') ? 'perf-record' : null,
    perfRecordCallGraph: options.get('--perf-record-call-graph') ?? 'dwarf',
    perfRecordFrequency: Number.parseInt(options.get('--perf-record-frequency') ?? '199', 10),
  };
}

function runChecked(command, args, { input = null, encoding = 'utf8' } = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT_DIR,
    encoding,
    input,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed with status ${result.status}\n${result.stderr ?? ''}`);
  }
  return result;
}

function buildReleaseBinary() {
  runChecked('cargo', ['build', '--release', '--bin', 'quic_loopback_profile', '--no-default-features', '--features', 'profile-binary']);
}

function createTempDir(prefix) {
  return mkdtempSync(join(os.tmpdir(), prefix));
}

function generateCertPair(tempDir) {
  const keyPath = join(tempDir, 'server-key.pem');
  const certPath = join(tempDir, 'server-cert.pem');
  runChecked('openssl', [
    'req',
    '-x509',
    '-newkey',
    'ec',
    '-pkeyopt',
    'ec_paramgen_curve:prime256v1',
    '-keyout',
    keyPath,
    '-out',
    certPath,
    '-days',
    '1',
    '-nodes',
    '-subj',
    '/CN=localhost',
  ]);
  return { keyPath, certPath };
}

function buildServerArgs(settings, certs) {
  return [
    'server',
    '--bind',
    settings.bindAddr,
    '--runtime-mode',
    settings.runtimeMode,
    '--alpn',
    settings.alpn,
    '--cert-path',
    certs.certPath,
    '--key-path',
    certs.keyPath,
  ];
}

function buildClientArgs(settings, serverAddr, tracePath = null) {
  const args = [
    'client',
    '--server-addr',
    serverAddr,
    '--server-name',
    settings.serverName,
    '--runtime-mode',
    settings.runtimeMode,
    '--alpn',
    settings.alpn,
    '--connections',
    String(settings.connections),
    '--streams-per-connection',
    String(settings.streamsPerConnection),
    '--payload-bytes',
    String(settings.payloadBytes),
    '--timeout-ms',
    String(settings.timeoutMs),
  ];
  if (tracePath) {
    args.push('--trace-path', tracePath);
  }
  return args;
}

function createPerfPaths(tempDir, role, mode) {
  if (!mode) {
    return null;
  }
  return {
    statPath: mode === 'perf-stat' ? join(tempDir, `${role}-perf-stat.txt`) : null,
    dataPath: mode === 'perf-record' ? join(tempDir, `${role}-perf.data`) : null,
    reportPath: mode === 'perf-record' ? join(tempDir, `${role}-perf-report.txt`) : null,
  };
}

function buildWrappedCommand(settings, role, baseArgs, perfPaths) {
  const binaryArgs = [BINARY_PATH, ...baseArgs];
  if (settings.profileRole !== role || !settings.perfMode) {
    return { command: BINARY_PATH, args: baseArgs };
  }

  if (settings.perfMode === 'perf-stat') {
    return {
      command: 'perf',
      args: ['stat', '-x,', '-o', perfPaths.statPath, '--', ...binaryArgs],
    };
  }

  return {
    command: 'perf',
    args: [
      'record',
      '-F',
      String(settings.perfRecordFrequency),
      '-g',
      settings.perfRecordCallGraph,
      '-o',
      perfPaths.dataPath,
      '--',
      ...binaryArgs,
    ],
  };
}

function parseResultLine(output) {
  const line = output
    .split(/\r?\n/u)
    .find((candidate) => candidate.startsWith('RESULT '));
  if (!line) {
    throw new Error(`Missing RESULT line in process output:\n${output}`);
  }
  return JSON.parse(line.slice('RESULT '.length));
}

function parseReadyLine(line) {
  if (!line.startsWith('READY ')) {
    return null;
  }
  return line.slice('READY '.length).trim();
}

async function startServer(settings, certs, perfPaths) {
  const serverArgs = buildServerArgs(settings, certs);
  const wrapped = buildWrappedCommand(settings, 'server', serverArgs, perfPaths);
  const child = spawn(wrapped.command, wrapped.args, {
    cwd: ROOT_DIR,
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  let stdout = '';
  let stderr = '';
  let serverAddr = null;
  let serverResult = null;

  const readyPromise = new Promise((resolveReady, rejectReady) => {
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');

    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      const lines = stdout.split(/\r?\n/u);
      stdout = lines.pop() ?? '';
      for (const line of lines) {
        if (!serverAddr) {
          serverAddr = parseReadyLine(line);
        }
        if (line.startsWith('RESULT ')) {
          serverResult = JSON.parse(line.slice('RESULT '.length));
        }
      }
      if (serverAddr) {
        resolveReady(serverAddr);
      }
    });

    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });

    child.on('exit', (code) => {
      if (!serverAddr) {
        rejectReady(new Error(`Server exited before READY with code ${code}\n${stderr}`));
      }
    });
  });

  const exitPromise = new Promise((resolveExit, rejectExit) => {
    child.on('exit', (code) => {
      if (code !== 0) {
        rejectExit(new Error(`Server exited with code ${code}\n${stderr}`));
        return;
      }
      const trailing = stdout.trim();
      if (trailing.startsWith('RESULT ')) {
        serverResult = JSON.parse(trailing.slice('RESULT '.length));
      }
      resolveExit({ serverResult, stderr });
    });
  });

  return {
    child,
    serverAddr: await readyPromise,
    stop: async () => {
      child.stdin.write('stop\n');
      child.stdin.end();
      return exitPromise;
    },
  };
}

function runClient(settings, serverAddr, perfPaths, tracePath = null) {
  const clientArgs = buildClientArgs(settings, serverAddr, tracePath);
  const wrapped = buildWrappedCommand(settings, 'client', clientArgs, perfPaths);
  const result = spawnSync(wrapped.command, wrapped.args, {
    cwd: ROOT_DIR,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    throw new Error(`Client process failed with status ${result.status}\n${result.stderr}`);
  }
  return {
    summary: parseResultLine(result.stdout),
    stdout: result.stdout,
    stderr: result.stderr,
  };
}

function finalizePerfArtifacts(settings, perfPaths) {
  if (!settings.perfMode) {
    return {};
  }

  if (settings.perfMode === 'perf-stat') {
    return {
      perfStat: readFileSync(perfPaths.statPath, 'utf8'),
    };
  }

  const report = runChecked('perf', ['report', '--stdio', '-i', perfPaths.dataPath]).stdout;
  return {
    perfDataPath: perfPaths.dataPath,
    perfReport: report,
  };
}

async function main() {
  const settings = resolveSettings(process.argv.slice(2));
  if (!settings.noBuild) {
    buildReleaseBinary();
  }

  const tempDir = createTempDir('http3-loopback-');
  try {
    const certs = generateCertPair(tempDir);
    const perfPaths = createPerfPaths(tempDir, settings.profileRole ?? 'none', settings.perfMode);
    const tracePath = settings.captureTrace ? join(tempDir, 'loopback-trace.json') : null;
    const server = await startServer(settings, certs, perfPaths);
    const client = runClient(settings, server.serverAddr, perfPaths, tracePath);
    const serverExit = await server.stop();
    const perfArtifacts = finalizePerfArtifacts(settings, perfPaths);

    const environment = captureEnvironmentMetadata({
      runner: 'profile-rust-quic-loopback',
      protocol: 'quic',
      target: 'rust-loopback',
      label: settings.label,
      extra: {
        harness: 'quic-loopback',
      },
    });
    const payload = {
      environment,
      settings: {
        runtimeMode: settings.runtimeMode,
        connections: settings.connections,
        streamsPerConnection: settings.streamsPerConnection,
        payloadBytes: settings.payloadBytes,
        timeoutMs: settings.timeoutMs,
        bindAddr: settings.bindAddr,
        alpn: settings.alpn,
        serverName: settings.serverName,
        profileRole: settings.profileRole,
        perfMode: settings.perfMode,
        perfRecordCallGraph: settings.perfRecordCallGraph,
        perfRecordFrequency: settings.perfRecordFrequency,
      },
      server: serverExit.serverResult,
      client: client.summary,
    };

    const jsonArtifact = writeJsonArtifact({
      rootDir: ROOT_DIR,
      resultsDir: settings.resultsDir,
      prefix: 'quic-loopback-profile',
      label: settings.label,
      payload,
    });
    const traceArtifact = settings.captureTrace
      ? writeJsonArtifact({
          rootDir: ROOT_DIR,
          resultsDir: settings.resultsDir,
          prefix: 'quic-loopback-trace',
          label: settings.label,
          payload: JSON.parse(readFileSync(tracePath, 'utf8')),
        })
      : null;

    let perfArtifact = null;
    if (perfArtifacts.perfStat) {
      perfArtifact = writeTextArtifact({
        rootDir: ROOT_DIR,
        resultsDir: settings.resultsDir,
        prefix: 'quic-loopback-perf-stat',
        label: settings.label,
        content: perfArtifacts.perfStat,
      });
    } else if (perfArtifacts.perfReport) {
      perfArtifact = writeTextArtifact({
        rootDir: ROOT_DIR,
        resultsDir: settings.resultsDir,
        prefix: 'quic-loopback-perf-report',
        label: settings.label,
        content: perfArtifacts.perfReport,
      });
    }

    const summary = {
      artifact: jsonArtifact?.relativePath ?? null,
      perfArtifact: perfArtifact?.relativePath ?? null,
      traceArtifact: traceArtifact?.relativePath ?? null,
      serverAddr: server.serverAddr,
      completedStreams: client.summary.completed_streams,
      requestedStreams: client.summary.requested_streams,
      throughputMbps: client.summary.throughput_mbps,
      runtimeMode: settings.runtimeMode,
      profileRole: settings.profileRole,
      perfMode: settings.perfMode,
    };
    console.log(JSON.stringify(summary, null, 2));
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
