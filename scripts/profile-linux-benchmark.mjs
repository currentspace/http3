import { existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  createArtifactStamp,
  resolveResultsDir,
  relativeToRoot,
  sanitizeArtifactFragment,
  writeJsonArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PROTOCOLS = new Set(['quic', 'h3']);

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();
  const forwardedArgs = [];

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (
      arg === '--help' ||
      arg === '--docker' ||
      arg === '--include-privileged' ||
      arg === '--no-build' ||
      arg === '--perf-stat' ||
      arg === '--perf-record' ||
      arg === '--strace'
    ) {
      flags.add(arg);
      continue;
    }

    if (
      arg === '--protocol' ||
      arg === '--results-dir' ||
      arg === '--label' ||
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

    forwardedArgs.push(arg);
  }

  return { options, flags, forwardedArgs };
}

function printHelp() {
  console.log(`Linux benchmark profiler

Usage:
  node scripts/profile-linux-benchmark.mjs [options forwarded to the benchmark]

Examples:
  npm run perf:linux:quic -- --perf-stat --profile throughput
  npm run perf:linux:h3 -- --perf-record --strace --profile stress --rounds 2
  npm run perf:linux:quic -- --docker --strace --include-privileged --profile balanced

Options:
  --protocol quic|h3                    Benchmark protocol (default: quic)
  --docker                              Wrap the Docker matrix runner instead of the host benchmark
  --include-privileged                  Forward to the Docker runner when --docker is used
  --no-build                            Reuse the existing Docker image when --docker is used
  --results-dir DIR                     Artifact directory (default: perf-results)
  --label TEXT                          Optional artifact label suffix
  --perf-stat                           Run one perf stat capture
  --perf-record                         Run one perf record capture
  --perf-record-call-graph MODE         perf record call graph mode (default: dwarf)
  --perf-record-frequency N             perf record sampling frequency (default: 199)
  --strace                              Run one strace -fc validation capture
  --help                                Show this help text

Notes:
  Remaining arguments are forwarded to the selected benchmark runner.
  If no profiler flag is provided, the benchmark still runs once and persists artifacts.
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
  const { options, flags, forwardedArgs } = parseArgs(argv);
  if (flags.has('--help')) {
    printHelp();
    process.exit(0);
  }

  const protocol = options.get('--protocol') ?? 'quic';
  if (!PROTOCOLS.has(protocol)) {
    throw new Error(`--protocol must be one of ${Array.from(PROTOCOLS).join(', ')}, got ${protocol}`);
  }

  if (!flags.has('--docker')) {
    const distTestDir = resolve(ROOT_DIR, 'dist-test', 'test');
    if (!existsSync(distTestDir)) {
      throw new Error('Missing dist-test benchmark artifacts. Run `npm run build:test` first.');
    }
  }

  const profilerModes = [];
  if (flags.has('--perf-stat')) {
    requireBinary('perf');
    profilerModes.push('perf-stat');
  }
  if (flags.has('--perf-record')) {
    requireBinary('perf');
    profilerModes.push('perf-record');
  }
  if (flags.has('--strace')) {
    requireBinary('strace');
    profilerModes.push('strace');
  }
  if (profilerModes.length === 0) {
    profilerModes.push('unprofiled');
  }

  return {
    protocol,
    docker: flags.has('--docker'),
    includePrivileged: flags.has('--include-privileged'),
    noBuild: flags.has('--no-build'),
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    perfRecordCallGraph: options.get('--perf-record-call-graph') ?? 'dwarf',
    perfRecordFrequency: Number.parseInt(options.get('--perf-record-frequency') ?? '199', 10),
    profilerModes,
    forwardedArgs,
  };
}

function benchmarkScriptFor(settings) {
  if (settings.docker) {
    return settings.protocol === 'quic'
      ? 'scripts/docker-quic-benchmark.mjs'
      : 'scripts/docker-h3-benchmark.mjs';
  }
  return settings.protocol === 'quic'
    ? 'scripts/quic-benchmark.mjs'
    : 'scripts/h3-benchmark.mjs';
}

function buildBenchmarkArgs(settings, runLabel) {
  const args = [benchmarkScriptFor(settings)];
  if (settings.docker && settings.includePrivileged) {
    args.push('--include-privileged');
  }
  if (settings.docker && settings.noBuild) {
    args.push('--no-build');
  }
  args.push('--results-dir', settings.resultsDir, '--label', runLabel, ...settings.forwardedArgs);
  return args;
}

function createProfilerOutputPath(absoluteResultsDir, settings, profilerMode, extension) {
  const filename = [
    createArtifactStamp(),
    sanitizeArtifactFragment(`linux-${settings.protocol}-${settings.docker ? 'docker' : 'host'}-${profilerMode}`),
    settings.label ? sanitizeArtifactFragment(settings.label) : null,
  ].filter(Boolean).join('-') + `.${extension}`;
  return resolve(absoluteResultsDir, filename);
}

function runProfile(settings, profilerMode, absoluteResultsDir) {
  const runLabelParts = [
    settings.label ?? settings.protocol,
    settings.docker ? 'docker' : 'host',
    profilerMode,
  ];
  const runLabel = runLabelParts.filter(Boolean).join('-');
  const benchmarkArgs = buildBenchmarkArgs(settings, runLabel);
  const baseCommand = [process.execPath, ...benchmarkArgs];

  let command = process.execPath;
  let args = benchmarkArgs;
  let outputPath = null;
  let purpose = 'benchmark';

  if (profilerMode === 'perf-stat') {
    outputPath = createProfilerOutputPath(absoluteResultsDir, settings, profilerMode, 'csv');
    command = 'perf';
    args = ['stat', '-x', ',', '-o', outputPath, '--', ...baseCommand];
    purpose = 'aggregate CPU counter capture';
  } else if (profilerMode === 'perf-record') {
    outputPath = createProfilerOutputPath(absoluteResultsDir, settings, profilerMode, 'data');
    command = 'perf';
    args = [
      'record',
      '-o',
      outputPath,
      '--call-graph',
      settings.perfRecordCallGraph,
      '-F',
      String(settings.perfRecordFrequency),
      '--',
      ...baseCommand,
    ];
    purpose = 'sampled CPU profile';
  } else if (profilerMode === 'strace') {
    outputPath = createProfilerOutputPath(absoluteResultsDir, settings, profilerMode, 'txt');
    command = 'strace';
    args = ['-fc', '-o', outputPath, ...baseCommand];
    purpose = 'setup-cost and syscall mix validation';
  }

  console.log(`\n=== ${profilerMode} ===`);
  const result = spawnSync(command, args, {
    cwd: ROOT_DIR,
    stdio: 'inherit',
  });

  return {
    profilerMode,
    purpose,
    command,
    args,
    outputPath: relativeToRoot(ROOT_DIR, outputPath),
    status: result.status,
    signal: result.signal ?? null,
  };
}

function main() {
  const settings = resolveSettings(process.argv.slice(2));
  const absoluteResultsDir = resolveResultsDir(ROOT_DIR, settings.resultsDir);
  const runs = [];
  let firstFailure = 0;

  for (const profilerMode of settings.profilerModes) {
    const run = runProfile(settings, profilerMode, absoluteResultsDir);
    runs.push(run);
    if (run.status !== 0 && firstFailure === 0) {
      firstFailure = run.status ?? 1;
      break;
    }
    if (run.signal && firstFailure === 0) {
      firstFailure = 1;
      break;
    }
  }

  const manifest = {
    artifactType: 'profiler-manifest',
    schemaVersion: 1,
    platform: 'linux',
    protocol: settings.protocol,
    target: settings.docker ? 'docker' : 'host',
    generatedAt: new Date().toISOString(),
    environment: captureEnvironmentMetadata({
      runner: 'scripts/profile-linux-benchmark.mjs',
      protocol: settings.protocol,
      target: settings.docker ? 'docker' : 'host',
      label: settings.label,
      extra: {
        forwardedArgs: settings.forwardedArgs,
        profilerModes: settings.profilerModes,
      },
    }),
    runs,
  };

  const artifact = writeJsonArtifact({
    rootDir: ROOT_DIR,
    resultsDir: settings.resultsDir,
    prefix: `linux-profiler-${settings.protocol}-${settings.docker ? 'docker' : 'host'}`,
    label: settings.label,
    payload: manifest,
  });

  if (artifact?.relativePath) {
    console.log(`\nProfiler manifest: ${artifact.relativePath}`);
  }

  if (firstFailure !== 0) {
    process.exitCode = firstFailure;
  }
}

main();
