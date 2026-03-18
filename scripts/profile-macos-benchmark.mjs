import { existsSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
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
    if (arg === '--help' || arg === '--sample' || arg === '--xctrace') {
      flags.add(arg);
      continue;
    }

    if (
      arg === '--protocol' ||
      arg === '--results-dir' ||
      arg === '--label' ||
      arg === '--sample-seconds' ||
      arg === '--xctrace-template'
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
  console.log(`macOS benchmark profiler

Usage:
  node scripts/profile-macos-benchmark.mjs [options forwarded to the benchmark]

Examples:
  npm run perf:macos:quic -- --sample --profile throughput
  npm run perf:macos:h3 -- --xctrace --profile stress --rounds 2
  npm run perf:macos:h3 -- --sample --xctrace --profile stress

Options:
  --protocol quic|h3                    Benchmark protocol (default: quic)
  --results-dir DIR                     Artifact directory (default: perf-results)
  --label TEXT                          Optional artifact label suffix
  --sample                              Capture a sample profile
  --sample-seconds N                    sample duration in seconds (default: 10)
  --xctrace                             Capture an Instruments Time Profiler trace via xctrace
  --xctrace-template NAME               xctrace template name (default: Time Profiler)
  --help                                Show this help text

Notes:
  Remaining arguments are forwarded to the host benchmark runner.
  If no profiler flag is provided, the wrapper defaults to --sample.
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

  const distTestDir = resolve(ROOT_DIR, 'dist-test', 'test');
  if (!existsSync(distTestDir)) {
    throw new Error('Missing dist-test benchmark artifacts. Run `npm run build:test` first.');
  }

  const profilerModes = [];
  if (flags.has('--sample')) {
    requireBinary('sample');
    profilerModes.push('sample');
  }
  if (flags.has('--xctrace')) {
    requireBinary('xcrun');
    profilerModes.push('xctrace');
  }
  if (profilerModes.length === 0) {
    requireBinary('sample');
    profilerModes.push('sample');
  }

  const sampleSeconds = Number.parseInt(options.get('--sample-seconds') ?? '10', 10);
  if (!Number.isFinite(sampleSeconds) || sampleSeconds <= 0) {
    throw new Error(`--sample-seconds must be an integer > 0, got ${options.get('--sample-seconds')}`);
  }

  return {
    protocol,
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    profilerModes,
    forwardedArgs,
    sampleSeconds,
    xctraceTemplate: options.get('--xctrace-template') ?? 'Time Profiler',
  };
}

function benchmarkScriptFor(protocol) {
  return protocol === 'quic'
    ? 'scripts/quic-benchmark.mjs'
    : 'scripts/h3-benchmark.mjs';
}

function buildBenchmarkArgs(settings, runLabel) {
  return [
    benchmarkScriptFor(settings.protocol),
    '--results-dir',
    settings.resultsDir,
    '--label',
    runLabel,
    ...settings.forwardedArgs,
  ];
}

function createProfilerOutputPath(absoluteResultsDir, settings, profilerMode, extension) {
  const filename = [
    createArtifactStamp(),
    sanitizeArtifactFragment(`macos-${settings.protocol}-${profilerMode}`),
    settings.label ? sanitizeArtifactFragment(settings.label) : null,
  ].filter(Boolean).join('-') + `.${extension}`;
  return resolve(absoluteResultsDir, filename);
}

function waitForChildExit(child) {
  return new Promise((resolvePromise, rejectPromise) => {
    child.once('error', rejectPromise);
    child.once('exit', (code, signal) => {
      resolvePromise({ code: code ?? 0, signal: signal ?? null });
    });
  });
}

async function runSampleProfile(settings, absoluteResultsDir) {
  const runLabel = [settings.label ?? settings.protocol, 'host', 'sample'].join('-');
  const benchmarkArgs = buildBenchmarkArgs(settings, runLabel);
  const outputPath = createProfilerOutputPath(absoluteResultsDir, settings, 'sample', 'txt');
  const child = spawn(process.execPath, benchmarkArgs, {
    cwd: ROOT_DIR,
    stdio: 'inherit',
  });

  const childExitPromise = waitForChildExit(child);
  const samplePromise = new Promise((resolvePromise) => {
    const startTimer = setTimeout(() => {
      if (child.exitCode !== null) {
        resolvePromise({
          skipped: true,
          status: 0,
          signal: null,
        });
        return;
      }

      const sampleProc = spawn('sample', [
        String(child.pid),
        String(settings.sampleSeconds),
        '-file',
        outputPath,
      ], {
        cwd: ROOT_DIR,
        stdio: 'inherit',
      });

      sampleProc.once('error', (error) => {
        resolvePromise({
          skipped: false,
          status: 1,
          signal: null,
          error: String(error),
        });
      });
      sampleProc.once('exit', (code, signal) => {
        resolvePromise({
          skipped: false,
          status: code ?? 0,
          signal: signal ?? null,
        });
      });
    }, 250);

    child.once('exit', () => {
      clearTimeout(startTimer);
    });
  });

  const [benchmarkExit, sampleExit] = await Promise.all([childExitPromise, samplePromise]);
  return {
    profilerMode: 'sample',
    purpose: 'stack sample capture',
    command: 'sample',
    args: [String(child.pid), String(settings.sampleSeconds), '-file', relativeToRoot(ROOT_DIR, outputPath)],
    outputPath: sampleExit.skipped ? null : relativeToRoot(ROOT_DIR, outputPath),
    skipped: sampleExit.skipped,
    benchmarkStatus: benchmarkExit.code,
    benchmarkSignal: benchmarkExit.signal,
    status: benchmarkExit.code !== 0 ? benchmarkExit.code : sampleExit.status,
    signal: sampleExit.signal,
    error: sampleExit.error ?? null,
  };
}

function runXctraceProfile(settings, absoluteResultsDir) {
  const runLabel = [settings.label ?? settings.protocol, 'host', 'xctrace'].join('-');
  const benchmarkArgs = buildBenchmarkArgs(settings, runLabel);
  const outputPath = createProfilerOutputPath(absoluteResultsDir, settings, 'xctrace', 'trace');
  const args = [
    'xctrace',
    'record',
    '--template',
    settings.xctraceTemplate,
    '--output',
    outputPath,
    '--launch',
    '--',
    process.execPath,
    ...benchmarkArgs,
  ];
  const result = spawnSync('xcrun', args, {
    cwd: ROOT_DIR,
    stdio: 'inherit',
  });

  return {
    profilerMode: 'xctrace',
    purpose: 'Instruments Time Profiler capture',
    command: 'xcrun',
    args,
    outputPath: relativeToRoot(ROOT_DIR, outputPath),
    status: result.status ?? 0,
    signal: result.signal ?? null,
  };
}

async function main() {
  const settings = resolveSettings(process.argv.slice(2));
  const absoluteResultsDir = resolveResultsDir(ROOT_DIR, settings.resultsDir);
  const runs = [];
  let firstFailure = 0;

  for (const profilerMode of settings.profilerModes) {
    console.log(`\n=== ${profilerMode} ===`);
    const run = profilerMode === 'sample'
      ? await runSampleProfile(settings, absoluteResultsDir)
      : runXctraceProfile(settings, absoluteResultsDir);
    runs.push(run);
    if ((run.status ?? 0) !== 0 && firstFailure === 0) {
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
    platform: 'macos',
    protocol: settings.protocol,
    target: 'host',
    generatedAt: new Date().toISOString(),
    environment: captureEnvironmentMetadata({
      runner: 'scripts/profile-macos-benchmark.mjs',
      protocol: settings.protocol,
      target: 'host',
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
    prefix: `macos-profiler-${settings.protocol}-host`,
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

await main();
