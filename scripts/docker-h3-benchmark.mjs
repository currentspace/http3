import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { captureEnvironmentMetadata, writeJsonArtifact } from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const IMAGE_TAG = 'currentspace/http3-runtime-tests:bench';

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();
  const forwardedArgs = [];

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '--include-privileged' || arg === '--no-build' || arg === '--json') {
      flags.add(arg);
      continue;
    }

    if (arg === '--platform' || arg === '--results-dir' || arg === '--label') {
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
  console.log(`Docker HTTP/3 benchmark matrix

Usage:
  npm run bench:h3:docker -- [options passed through to h3-benchmark]

Examples:
  npm run bench:h3:docker
  npm run bench:h3:docker -- --profile balanced --rounds 3
  npm run bench:h3:docker -- --connections 25 --streams-per-connection 30 --message-size 16KB
  npm run bench:h3:docker -- --include-privileged

Runner options:
  --platform VALUE         Docker platform override (defaults to DOCKER_RUNTIME_PLATFORM or host default)
  --include-privileged     Also benchmark a privileged fast-path lane
  --no-build               Reuse the existing runtime-test image
  --results-dir DIR        Write a timestamped matrix artifact under DIR
  --label TEXT             Optional artifact label suffix
  --json                   Print the full matrix as JSON
  --help                   Show this help text

Forwarded benchmark options:
  Any remaining args are forwarded to \`scripts/h3-benchmark.mjs\`.
  If no explicit \`--profile\` is provided, this runner defaults to \`--profile balanced --rounds 2\`.
`);
}

function ensureDefaultWorkloadArgs(args) {
  const finalArgs = [...args];
  const hasProfile = finalArgs.some((arg) => arg === '--profile' || arg.startsWith('--profile='));
  const hasRounds = finalArgs.some((arg) => arg === '--rounds' || arg.startsWith('--rounds='));

  if (!hasProfile) {
    finalArgs.push('--profile', 'balanced');
  }
  if (!hasRounds) {
    finalArgs.push('--rounds', '2');
  }

  return finalArgs;
}

function runDocker(args, { allowFailure = false } = {}) {
  const result = spawnSync('docker', args, {
    cwd: ROOT_DIR,
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  });

  if (!allowFailure && result.status !== 0) {
    const stderr = result.stderr.trim();
    const stdout = result.stdout.trim();
    throw new Error([
      `docker ${args.join(' ')} failed with status ${result.status}`,
      stdout,
      stderr,
    ].filter(Boolean).join('\n'));
  }

  return result;
}

function buildImage(platform) {
  const args = ['build', '-f', 'Dockerfile.runtime-test', '-t', IMAGE_TAG];
  if (platform) {
    args.push('--platform', platform);
  }
  args.push('.');

  console.log('\nBuilding Docker benchmark image...');
  const result = runDocker(args);
  if (result.stdout.trim()) {
    process.stdout.write(result.stdout);
  }
  if (result.stderr.trim()) {
    process.stderr.write(result.stderr);
  }
}

function extractJsonFromStdout(stdout) {
  const lines = stdout.split('\n').map((line) => line.trim()).filter(Boolean);
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    try {
      return JSON.parse(lines[index]);
    } catch {
      // Keep scanning for the final JSON payload.
    }
  }
  return null;
}

function formatRuntimeInfo(runtimeInfo) {
  if (!runtimeInfo) {
    return 'unknown';
  }

  const requestedMode = runtimeInfo.requestedMode ?? 'unknown';
  const selectedMode = runtimeInfo.selectedMode ?? 'unknown';
  const driver = runtimeInfo.driver ?? 'unknown';
  const fallback = runtimeInfo.fallbackOccurred ? 'fallback' : 'direct';
  return `${requestedMode}->${selectedMode}/${driver}/${fallback}`;
}

function formatCountSummary(counts) {
  const entries = Object.entries(counts ?? {}).sort((left, right) => left[0].localeCompare(right[0]));
  if (entries.length === 0) {
    return 'none';
  }
  return entries.map(([key, value]) => `${key} x${value}`).join(', ');
}

function formatRange(values, digits = 2, unit = 'ms') {
  if (!Array.isArray(values) || values.length === 0) {
    return `0${unit}`;
  }
  const min = Math.min(...values);
  const max = Math.max(...values);
  const minText = min.toFixed(digits);
  const maxText = max.toFixed(digits);
  if (Math.abs(max - min) < Number.EPSILON) {
    return `${maxText}${unit}`;
  }
  return `${minText}-${maxText}${unit}`;
}

function percentDelta(current, baseline) {
  if (!Number.isFinite(current) || !Number.isFinite(baseline) || baseline === 0) {
    return 'n/a';
  }

  const delta = ((current - baseline) / baseline) * 100;
  const sign = delta > 0 ? '+' : '';
  return `${sign}${delta.toFixed(1)}%`;
}

function summarizeFailure(output) {
  const text = `${output.stdout}\n${output.stderr}`;
  const lines = text.split('\n').map((line) => line.trim()).filter(Boolean);
  const interestingLines = lines.filter((line) => /ERR_HTTP3_|io_uring|Operation not permitted|fast-path-unavailable/u.test(line));
  const selectedLines = interestingLines.length > 0 ? interestingLines.slice(0, 5) : lines.slice(-4);
  return selectedLines.join(' | ');
}

function formatExpectedFailureSummary(summary) {
  if (/ERR_HTTP3_FAST_PATH_UNAVAILABLE/u.test(summary)) {
    return `fast path unavailable as expected under this container policy: ${summary}`;
  }
  return `expected diagnostic outcome: ${summary}`;
}

function runLane({ name, dockerFlags = [], benchmarkArgs = [], expectFailure = false }, platform, forwardedArgs, label) {
  const args = ['run', '--rm', '--init'];
  if (platform) {
    args.push('--platform', platform);
  }
  args.push(...dockerFlags, IMAGE_TAG, 'node', 'scripts/h3-benchmark.mjs', '--json');
  if (label) {
    args.push('--label', label);
  }
  args.push(...forwardedArgs, ...benchmarkArgs);

  console.log(`\n=== ${name} ===`);
  const result = runDocker(args, { allowFailure: true });

  if (result.status === 0) {
    const summary = extractJsonFromStdout(result.stdout);
    if (!summary) {
      throw new Error(`${name} completed without JSON output`);
    }
    console.log(
      `ok: ${summary.throughputMbps.toFixed(1)} Mbps, ${summary.streamsPerSecond.toFixed(0)} requests/s,` +
      ` client=${formatCountSummary(summary.clientStats.runtimeSelections)},` +
      ` server=${formatRuntimeInfo(summary.serverStats?.runtimeInfo)}`,
    );

    if (expectFailure) {
      return {
        name,
        type: 'unexpected_success',
        dockerFlags,
        benchmarkArgs,
        summary,
      };
    }

    return {
      name,
      type: 'success',
      dockerFlags,
      benchmarkArgs,
      summary,
    };
  }

  const failureSummary = summarizeFailure(result);
  console.log(expectFailure ? formatExpectedFailureSummary(failureSummary) : `failed: ${failureSummary}`);

  return {
    name,
    type: expectFailure ? 'expected_failure' : 'failure',
    dockerFlags,
    benchmarkArgs,
    status: result.status,
    output: {
      stdout: result.stdout,
      stderr: result.stderr,
      summary: failureSummary,
    },
  };
}

function printPerformanceTable(results) {
  const baseline = results.find((result) => result.name === 'ordinary portable');
  const header = [
    'Lane'.padEnd(36),
    'Throughput'.padStart(12),
    'Delta'.padStart(10),
    'Requests/s'.padStart(10),
    'P95'.padStart(12),
  ].join(' ');

  console.log('\nPerformance lanes');
  console.log(`  ${header}`);
  console.log(`  ${'-'.repeat(header.length)}`);

  for (const result of results) {
    if (result.type !== 'success') {
      continue;
    }

    const throughput = `${result.summary.throughputMbps.toFixed(1)} Mbps`;
    const delta = baseline ? percentDelta(result.summary.throughputMbps, baseline.summary.throughputMbps) : 'n/a';
    const streamsPerSecond = `${result.summary.streamsPerSecond.toFixed(0)}`;
    const p95 = formatRange(result.summary.clientStats.streamP95s);

    console.log(
      `  ${result.name.padEnd(36)} ${throughput.padStart(12)} ${delta.padStart(10)} ${streamsPerSecond.padStart(10)} ${p95.padStart(12)}`,
    );
    console.log(
      `  ${''.padEnd(36)} ${formatCountSummary(result.summary.clientStats.runtimeSelections)}`,
    );
    console.log(
      `  ${''.padEnd(36)} server=${formatRuntimeInfo(result.summary.serverStats?.runtimeInfo)}`,
    );
  }
}

function printAnalysis(results) {
  const successes = new Map(
    results
      .filter((result) => result.type === 'success')
      .map((result) => [result.name, result.summary]),
  );

  const notes = [];
  const ordinaryPortable = successes.get('ordinary portable');
  const ordinaryAuto = successes.get('ordinary auto fallback');
  const unconfinedPortable = successes.get('unconfined portable');
  const unconfinedFast = successes.get('unconfined fast');
  const serverFast = successes.get('unconfined server fast, client portable');
  const clientFast = successes.get('unconfined server portable, client fast');
  const privilegedFast = successes.get('privileged fast');

  if (ordinaryPortable && ordinaryAuto) {
    notes.push(
      `ordinary auto fallback tracked ${percentDelta(ordinaryAuto.throughputMbps, ordinaryPortable.throughputMbps)} vs explicit portable while selecting ${formatCountSummary(ordinaryAuto.clientStats.runtimeSelections)}`,
    );
  }

  if (unconfinedPortable && unconfinedFast) {
    notes.push(
      `switching both ends from portable to io_uring in an unconfined container changed throughput by ${percentDelta(unconfinedFast.throughputMbps, unconfinedPortable.throughputMbps)}`,
    );
  }

  if (unconfinedPortable && serverFast) {
    notes.push(
      `enabling only the server fast path changed throughput by ${percentDelta(serverFast.throughputMbps, unconfinedPortable.throughputMbps)}`,
    );
  }

  if (unconfinedPortable && clientFast) {
    notes.push(
      `enabling only the client fast path changed throughput by ${percentDelta(clientFast.throughputMbps, unconfinedPortable.throughputMbps)}`,
    );
  }

  if (unconfinedFast && privilegedFast) {
    notes.push(
      `privileged fast tracked ${percentDelta(privilegedFast.throughputMbps, unconfinedFast.throughputMbps)} vs unconfined fast, which shows whether broad privilege adds anything beyond allowing io_uring`,
    );
  }

  const expectedFailures = results.filter((result) => result.type === 'expected_failure');
  for (const result of expectedFailures) {
    notes.push(`${result.name} still failed as expected: ${result.output.summary}`);
  }

  if (notes.length === 0) {
    return;
  }

  console.log('\nObservations');
  for (const note of notes) {
    console.log(`- ${note}`);
  }
}

function createLaneMatrix(includePrivileged) {
  const lanes = [
    {
      name: 'ordinary portable',
      benchmarkArgs: ['--runtime-mode', 'portable', '--fallback-policy', 'error'],
    },
    {
      name: 'ordinary auto fallback',
      benchmarkArgs: ['--runtime-mode', 'auto', '--fallback-policy', 'warn-and-fallback'],
    },
    {
      name: 'ordinary fast unavailable (expected)',
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
      expectFailure: true,
    },
    {
      name: 'cap-add fast unavailable (expected)',
      dockerFlags: ['--cap-add', 'SYS_ADMIN'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
      expectFailure: true,
    },
    {
      name: 'unconfined portable',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: ['--runtime-mode', 'portable', '--fallback-policy', 'error'],
    },
    {
      name: 'unconfined fast',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
    },
    {
      name: 'unconfined server fast, client portable',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: [
        '--server-runtime-mode', 'fast',
        '--server-fallback-policy', 'error',
        '--client-runtime-mode', 'portable',
        '--client-fallback-policy', 'error',
      ],
    },
    {
      name: 'unconfined server portable, client fast',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: [
        '--server-runtime-mode', 'portable',
        '--server-fallback-policy', 'error',
        '--client-runtime-mode', 'fast',
        '--client-fallback-policy', 'error',
      ],
    },
  ];

  if (includePrivileged) {
    lanes.push({
      name: 'privileged fast',
      dockerFlags: ['--privileged'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
    });
  }

  return lanes;
}

function main() {
  const { options, flags, forwardedArgs } = parseArgs(process.argv.slice(2));
  if (flags.has('--help')) {
    printHelp();
    return;
  }

  const platform = options.get('--platform') ?? process.env.DOCKER_RUNTIME_PLATFORM;
  const resultsDir = options.get('--results-dir');
  const label = options.get('--label') ?? null;
  const workloadArgs = ensureDefaultWorkloadArgs(forwardedArgs);

  if (!flags.has('--no-build')) {
    buildImage(platform);
  }

  const results = [];
  for (const lane of createLaneMatrix(flags.has('--include-privileged'))) {
    results.push(runLane(lane, platform, workloadArgs, label));
  }

  const matrix = {
    artifactType: 'docker-benchmark-matrix',
    schemaVersion: 1,
    protocol: 'h3',
    target: 'docker',
    generatedAt: new Date().toISOString(),
    environment: captureEnvironmentMetadata({
      runner: 'scripts/docker-h3-benchmark.mjs',
      protocol: 'h3',
      target: 'docker',
      label,
      extra: {
        dockerPlatform: platform ?? null,
        image: IMAGE_TAG,
      },
    }),
    image: IMAGE_TAG,
    platform: platform ?? null,
    workloadArgs,
    results,
  };

  const artifact = writeJsonArtifact({
    rootDir: ROOT_DIR,
    resultsDir,
    prefix: 'benchmark-h3-docker-matrix',
    label,
    payload: matrix,
  });

  if (flags.has('--json')) {
    process.stdout.write(`${JSON.stringify(matrix)}\n`);
    return;
  }

  printPerformanceTable(results);
  printAnalysis(results);
  if (artifact?.relativePath) {
    console.log(`\nArtifact: ${artifact.relativePath}`);
  }

  const failures = results.filter((result) => result.type === 'failure' || result.type === 'unexpected_success');
  if (failures.length > 0) {
    process.exitCode = 1;
  }
}

main();
