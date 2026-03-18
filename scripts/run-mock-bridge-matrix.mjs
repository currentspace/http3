import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  writeJsonArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PROFILE_SCRIPT = resolve(ROOT_DIR, 'scripts', 'profile-node-mock-transport.mjs');

const PRESETS = ['bridge-medium', 'bridge-heavy', 'bridge-soak'];
const MODES = [
  {
    name: 'counting',
    args: ['--sink-mode', 'counting', '--callback-mode', 'noop'],
  },
  {
    name: 'tsfn-noop-js',
    args: ['--sink-mode', 'tsfn', '--callback-mode', 'noop'],
  },
  {
    name: 'tsfn-real-js',
    args: [
      '--sink-mode',
      'tsfn',
      '--callback-mode',
      'touch-data',
      '--clone-buffers',
      '--materialize-events',
    ],
  },
];

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '--no-build') {
      flags.add(arg);
      continue;
    }
    if (arg === '--results-dir' || arg === '--label') {
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
  console.log(`Run the mock bridge scale matrix

Usage:
  node scripts/run-mock-bridge-matrix.mjs [options]

Options:
  --results-dir DIR   Artifact directory (default: perf-results)
  --label TEXT        Optional artifact label suffix
  --no-build          Reuse the existing native binding
  --help              Show this help
`);
}

function resolveSettings(argv) {
  const { options, flags } = parseArgs(argv);
  if (flags.has('--help')) {
    printHelp();
    process.exit(0);
  }

  return {
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    noBuild: flags.has('--no-build'),
  };
}

function runProfile(settings, preset, mode, buildDone) {
  const args = [PROFILE_SCRIPT];
  if (settings.noBuild || buildDone) {
    args.push('--no-build');
  }
  args.push(
    '--preset',
    preset,
    '--results-dir',
    settings.resultsDir,
    '--label',
    `${settings.label ?? 'mock-bridge'}-${preset}-${mode.name}`,
    ...mode.args,
  );
  const result = spawnSync(process.execPath, args, {
    cwd: ROOT_DIR,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    throw new Error(
      `${preset}/${mode.name} failed with status ${result.status}\n${result.stderr ?? ''}`,
    );
  }
  const summary = JSON.parse(result.stdout);
  const artifact = JSON.parse(
    readFileSync(resolve(ROOT_DIR, summary.artifact), 'utf8'),
  );
  return {
    ...summary,
    wallElapsedMs: artifact.jsRuntime?.wallElapsedMs ?? null,
    rustElapsedMs: artifact.summary?.elapsed_ms ?? null,
    latencyP95Ms: artifact.summary?.latency_ms?.p95_ms ?? null,
    jsBatches: artifact.jsCallback?.batches ?? 0,
    jsEvents: artifact.jsCallback?.events ?? 0,
    listenerDispatches: artifact.jsCallback?.listenerDispatches ?? 0,
    jsCallbackDurationMs: artifact.jsCallback?.totalCallbackDurationMs ?? 0,
    eventLoopUtilization: artifact.jsRuntime?.eventLoopUtilization?.utilization ?? null,
    heapUsedDeltaBytes: artifact.jsRuntime?.memoryDelta?.heapUsedDeltaBytes ?? null,
  };
}

function main() {
  const settings = resolveSettings(process.argv.slice(2));
  const results = [];
  let buildDone = false;

  for (const preset of PRESETS) {
    for (const mode of MODES) {
      const summary = runProfile(settings, preset, mode, buildDone);
      buildDone = true;
      results.push({
        preset,
        mode: mode.name,
        summary,
      });
    }
  }

  const payload = {
    harness: 'quic-mock-matrix',
    environment: captureEnvironmentMetadata({
      runner: 'run-mock-bridge-matrix',
      protocol: 'quic',
      target: 'node-mock-transport',
      label: settings.label,
      extra: {
        presets: PRESETS,
        modes: MODES.map(mode => mode.name),
      },
    }),
    results,
  };
  const artifact = writeJsonArtifact({
    rootDir: ROOT_DIR,
    resultsDir: settings.resultsDir,
    prefix: 'quic-mock-bridge-matrix',
    label: settings.label,
    payload,
  });

  console.log(JSON.stringify({
    artifact: artifact?.relativePath ?? null,
    runs: results.length,
  }, null, 2));
}

main();
