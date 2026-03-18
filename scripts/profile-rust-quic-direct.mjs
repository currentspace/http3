import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import os from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  writeJsonArtifact,
  writeTextArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BINARY_PATH = resolve(ROOT_DIR, 'target', 'release', 'quic_direct_profile');

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '--no-build' || arg === '--perf-stat' || arg === '--perf-record') {
      flags.add(arg);
      continue;
    }
    if (
      arg === '--connections' ||
      arg === '--streams-per-connection' ||
      arg === '--payload-bytes' ||
      arg === '--timeout-ms' ||
      arg === '--results-dir' ||
      arg === '--label' ||
      arg === '--server-name' ||
      arg === '--alpn' ||
      arg === '--perf-record-call-graph' ||
      arg === '--perf-record-frequency'
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
  console.log(`Rust quiche-direct profiler

Usage:
  node scripts/profile-rust-quic-direct.mjs [options]

Options:
  --connections N                    Number of in-memory pairs (default: 1)
  --streams-per-connection N         Streams per pair (default: 100)
  --payload-bytes N                  Bytes per request stream (default: 16384)
  --timeout-ms N                     End-to-end timeout (default: 30000)
  --server-name VALUE                SNI / hostname (default: localhost)
  --alpn VALUE                       ALPN string (default: quic)
  --results-dir DIR                  Artifact directory (default: perf-results)
  --label TEXT                       Optional artifact label suffix
  --perf-stat                        Wrap the run with perf stat
  --perf-record                      Wrap the run with perf record/report
  --perf-record-call-graph MODE      perf record call graph mode (default: dwarf)
  --perf-record-frequency N          perf record frequency (default: 199)
  --no-build                         Reuse the existing release binary
  --help                             Show this help
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
  const perfModes = [flags.has('--perf-stat'), flags.has('--perf-record')].filter(Boolean).length;
  if (perfModes > 1) {
    throw new Error('Choose either --perf-stat or --perf-record, not both.');
  }
  requireBinary('openssl');
  if (perfModes === 1) {
    requireBinary('perf');
  }
  return {
    connections: Number.parseInt(options.get('--connections') ?? '1', 10),
    streamsPerConnection: Number.parseInt(options.get('--streams-per-connection') ?? '100', 10),
    payloadBytes: Number.parseInt(options.get('--payload-bytes') ?? '16384', 10),
    timeoutMs: Number.parseInt(options.get('--timeout-ms') ?? '30000', 10),
    serverName: options.get('--server-name') ?? 'localhost',
    alpn: options.get('--alpn') ?? 'quic',
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    noBuild: flags.has('--no-build'),
    perfMode: flags.has('--perf-stat') ? 'perf-stat' : flags.has('--perf-record') ? 'perf-record' : null,
    perfRecordCallGraph: options.get('--perf-record-call-graph') ?? 'dwarf',
    perfRecordFrequency: Number.parseInt(options.get('--perf-record-frequency') ?? '199', 10),
  };
}

function runChecked(command, args, { encoding = 'utf8' } = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT_DIR,
    encoding,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed with status ${result.status}\n${result.stderr ?? ''}`);
  }
  return result;
}

function buildReleaseBinary() {
  runChecked('cargo', ['build', '--release', '--bin', 'quic_direct_profile', '--no-default-features', '--features', 'profile-binary']);
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

function parseResultLine(output) {
  const line = output
    .split(/\r?\n/u)
    .find((candidate) => candidate.startsWith('RESULT '));
  if (!line) {
    throw new Error(`Missing RESULT line in process output:\n${output}`);
  }
  return JSON.parse(line.slice('RESULT '.length));
}

function createPerfPaths(tempDir, perfMode) {
  if (!perfMode) {
    return null;
  }
  return {
    statPath: perfMode === 'perf-stat' ? join(tempDir, 'direct-perf-stat.txt') : null,
    dataPath: perfMode === 'perf-record' ? join(tempDir, 'direct-perf.data') : null,
  };
}

function runProfile(settings, certs, perfPaths) {
  const baseArgs = [
    '--cert-path',
    certs.certPath,
    '--key-path',
    certs.keyPath,
    '--connections',
    String(settings.connections),
    '--streams-per-connection',
    String(settings.streamsPerConnection),
    '--payload-bytes',
    String(settings.payloadBytes),
    '--timeout-ms',
    String(settings.timeoutMs),
    '--server-name',
    settings.serverName,
    '--alpn',
    settings.alpn,
  ];

  if (!settings.perfMode) {
    const result = spawnSync(BINARY_PATH, baseArgs, {
      cwd: ROOT_DIR,
      encoding: 'utf8',
    });
    if (result.status !== 0) {
      throw new Error(`Direct profile failed with status ${result.status}\n${result.stderr}`);
    }
    return {
      summary: parseResultLine(result.stdout),
      perfText: null,
    };
  }

  if (settings.perfMode === 'perf-stat') {
    const result = spawnSync('perf', ['stat', '-x,', '-o', perfPaths.statPath, '--', BINARY_PATH, ...baseArgs], {
      cwd: ROOT_DIR,
      encoding: 'utf8',
    });
    if (result.status !== 0) {
      throw new Error(`perf stat failed with status ${result.status}\n${result.stderr}`);
    }
    return {
      summary: parseResultLine(result.stdout),
      perfText: readFileSync(perfPaths.statPath, 'utf8'),
    };
  }

  const record = spawnSync(
    'perf',
    [
      'record',
      '-F',
      String(settings.perfRecordFrequency),
      '-g',
      settings.perfRecordCallGraph,
      '-o',
      perfPaths.dataPath,
      '--',
      BINARY_PATH,
      ...baseArgs,
    ],
    {
      cwd: ROOT_DIR,
      encoding: 'utf8',
    }
  );
  if (record.status !== 0) {
    throw new Error(`perf record failed with status ${record.status}\n${record.stderr}`);
  }
  const report = runChecked('perf', ['report', '--stdio', '-i', perfPaths.dataPath]).stdout;
  return {
    summary: parseResultLine(record.stdout),
    perfText: report,
  };
}

function main() {
  const settings = resolveSettings(process.argv.slice(2));
  if (!settings.noBuild) {
    buildReleaseBinary();
  }

  const tempDir = createTempDir('http3-direct-');
  try {
    const certs = generateCertPair(tempDir);
    const perfPaths = createPerfPaths(tempDir, settings.perfMode);
    const { summary, perfText } = runProfile(settings, certs, perfPaths);

    const payload = {
      environment: captureEnvironmentMetadata({
        runner: 'profile-rust-quic-direct',
        protocol: 'quic',
        target: 'quiche-direct-floor',
        label: settings.label,
        extra: {
          harness: 'quiche-direct-floor',
        },
      }),
      settings: {
        connections: settings.connections,
        streamsPerConnection: settings.streamsPerConnection,
        payloadBytes: settings.payloadBytes,
        timeoutMs: settings.timeoutMs,
        serverName: settings.serverName,
        alpn: settings.alpn,
        perfMode: settings.perfMode,
        perfRecordCallGraph: settings.perfRecordCallGraph,
        perfRecordFrequency: settings.perfRecordFrequency,
      },
      result: summary,
    };

    const jsonArtifact = writeJsonArtifact({
      rootDir: ROOT_DIR,
      resultsDir: settings.resultsDir,
      prefix: 'quic-direct-profile',
      label: settings.label,
      payload,
    });
    const perfArtifact = perfText
      ? writeTextArtifact({
          rootDir: ROOT_DIR,
          resultsDir: settings.resultsDir,
          prefix: settings.perfMode === 'perf-stat' ? 'quic-direct-perf-stat' : 'quic-direct-perf-report',
          label: settings.label,
          content: perfText,
        })
      : null;

    console.log(
      JSON.stringify(
        {
          artifact: jsonArtifact?.relativePath ?? null,
          perfArtifact: perfArtifact?.relativePath ?? null,
          completedStreams: summary.completed_streams,
          requestedStreams: summary.requested_streams,
          throughputMbps: summary.throughput_mbps,
          perfMode: settings.perfMode,
        },
        null,
        2
      )
    );
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
