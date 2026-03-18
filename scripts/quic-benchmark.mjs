import { existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST_TEST_DIR = join(ROOT_DIR, 'dist-test', 'test');
const BENCH_SERVER_PATH = join(DIST_TEST_DIR, 'quic-bench-server.js');
const BENCH_CLIENT_PATH = join(DIST_TEST_DIR, 'quic-bench-client.js');

const PROFILES = {
  smoke: {
    clientProcesses: 1,
    rounds: 1,
    connections: 5,
    streamsPerConnection: 10,
    messageSize: 1024,
    timeoutMs: 15_000,
    pauseMs: 50,
  },
  balanced: {
    clientProcesses: 1,
    rounds: 1,
    connections: 10,
    streamsPerConnection: 50,
    messageSize: 4096,
    timeoutMs: 30_000,
    pauseMs: 100,
  },
  throughput: {
    clientProcesses: 1,
    rounds: 1,
    connections: 20,
    streamsPerConnection: 20,
    messageSize: 16 * 1024,
    timeoutMs: 60_000,
    pauseMs: 100,
  },
  stress: {
    clientProcesses: 2,
    rounds: 3,
    connections: 10,
    streamsPerConnection: 50,
    messageSize: 16 * 1024,
    timeoutMs: 45_000,
    pauseMs: 250,
  },
};

const RUNTIME_MODES = new Set(['auto', 'fast', 'portable']);
const FALLBACK_POLICIES = new Set(['error', 'warn-and-fallback']);

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg.startsWith('--') && arg.includes('=')) {
      const [key, value] = arg.split(/=(.*)/su, 2);
      options.set(key, value);
      continue;
    }

    if (arg.startsWith('--')) {
      const next = argv[index + 1];
      if (next && !next.startsWith('--')) {
        options.set(arg, next);
        index += 1;
      } else {
        flags.add(arg);
      }
    }
  }

  return { options, flags };
}

function requireBuildArtifacts() {
  if (existsSync(BENCH_SERVER_PATH) && existsSync(BENCH_CLIENT_PATH)) {
    return;
  }

  throw new Error(
    'Missing dist-test benchmark artifacts. Run `npm run build:test` first, or use `npm run bench:quic`.',
  );
}

function parseInteger(name, value, { min = 0 } = {}) {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed) || parsed < min) {
    throw new Error(`${name} must be an integer >= ${min}, got ${value}`);
  }
  return parsed;
}

function parseByteSize(name, value) {
  const match = value.match(/^(\d+)([kKmMgG][bB]?)?$/u);
  if (!match) {
    throw new Error(`${name} must be a byte count like 4096, 16k, or 64KB; got ${value}`);
  }

  const amount = Number.parseInt(match[1], 10);
  const suffix = match[2]?.toLowerCase() ?? '';
  if (suffix.startsWith('k')) {
    return amount * 1024;
  }
  if (suffix.startsWith('m')) {
    return amount * 1024 * 1024;
  }
  if (suffix.startsWith('g')) {
    return amount * 1024 * 1024 * 1024;
  }
  return amount;
}

function parseEnum(name, value, allowed) {
  if (value === undefined) {
    return undefined;
  }
  if (!allowed.has(value)) {
    throw new Error(`${name} must be one of ${Array.from(allowed).join(', ')}, got ${value}`);
  }
  return value;
}

function sleep(ms) {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function sumBy(values, selector) {
  return values.reduce((total, value) => total + selector(value), 0);
}

function weightedMean(values, valueSelector, countSelector) {
  const totalCount = sumBy(values, countSelector);
  if (totalCount === 0) {
    return 0;
  }
  return sumBy(values, (value) => valueSelector(value) * countSelector(value)) / totalCount;
}

function rangeOf(values) {
  if (values.length === 0) {
    return { min: 0, max: 0 };
  }
  return {
    min: Math.min(...values),
    max: Math.max(...values),
  };
}

function formatBytes(bytes) {
  if (bytes >= 1024 * 1024) {
    return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${bytes} B`;
}

function formatRange(values, digits = 2, unit = 'ms') {
  const { min, max } = rangeOf(values);
  const minText = min.toFixed(digits);
  const maxText = max.toFixed(digits);
  if (Math.abs(max - min) < Number.EPSILON) {
    return `${maxText}${unit}`;
  }
  return `${minText}-${maxText}${unit}`;
}

function mergeCountObjects(target, source) {
  for (const [key, value] of Object.entries(source ?? {})) {
    target[key] = (target[key] ?? 0) + value;
  }
  return target;
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

function printHelp() {
  console.log(`Raw QUIC benchmark harness

Usage:
  npm run bench:quic -- [options]

Examples:
  npm run bench:quic
  npm run bench:quic:stress
  npm run bench:quic -- --profile throughput --connections 25 --streams-per-connection 40 --message-size 16KB
  npm run bench:quic -- --client-processes 2 --rounds 3 --runtime-mode portable

Options:
  --profile smoke|balanced|throughput|stress  Named preset (default: balanced)
  --client-processes, --clients N             Concurrent benchmark client processes
  --connections N                             QUIC sessions opened per client process
  --streams-per-connection N                  Streams opened on each QUIC session
  --message-size SIZE                         Payload bytes per stream (e.g. 4096, 16k, 64KB)
  --rounds N                                  Re-run the same load against one server
  --pause-ms N                                Delay between rounds (default from profile)
  --timeout-ms N                              Per-client timeout budget
  --host HOST                                 Bind/connect host (default: 127.0.0.1)
  --runtime-mode MODE                         Apply runtime mode to server and clients
  --server-runtime-mode MODE                  Override server runtime mode
  --client-runtime-mode MODE                  Override client runtime mode
  --fallback-policy POLICY                    Apply fallback policy to server and clients
  --server-fallback-policy POLICY             Override server fallback policy
  --client-fallback-policy POLICY             Override client fallback policy
  --stats-interval-ms N                       Server stats emission interval
  --allow-errors                              Exit 0 even if the benchmark sees stream errors
  --json                                      Print machine-readable JSON summary
  --help                                      Show this help text
`);
}

function resolveSettings(argv) {
  const { options, flags } = parseArgs(argv);
  if (flags.has('--help')) {
    printHelp();
    process.exit(0);
  }

  const profileName = options.get('--profile') ?? 'balanced';
  const profile = PROFILES[profileName];
  if (!profile) {
    throw new Error(`Unknown profile ${profileName}. Expected one of ${Object.keys(PROFILES).join(', ')}`);
  }

  const sharedRuntimeMode = parseEnum('runtime mode', options.get('--runtime-mode'), RUNTIME_MODES);
  const sharedFallbackPolicy = parseEnum('fallback policy', options.get('--fallback-policy'), FALLBACK_POLICIES);
  const serverRuntimeMode = parseEnum(
    'server runtime mode',
    options.get('--server-runtime-mode') ?? sharedRuntimeMode,
    RUNTIME_MODES,
  );
  const clientRuntimeMode = parseEnum(
    'client runtime mode',
    options.get('--client-runtime-mode') ?? sharedRuntimeMode,
    RUNTIME_MODES,
  );
  const serverFallbackPolicy = parseEnum(
    'server fallback policy',
    options.get('--server-fallback-policy') ?? sharedFallbackPolicy,
    FALLBACK_POLICIES,
  );
  const clientFallbackPolicy = parseEnum(
    'client fallback policy',
    options.get('--client-fallback-policy') ?? sharedFallbackPolicy,
    FALLBACK_POLICIES,
  );

  const clientProcesses = options.get('--client-processes') ?? options.get('--clients');

  return {
    profileName,
    host: options.get('--host') ?? '127.0.0.1',
    clientProcesses: clientProcesses === undefined
      ? profile.clientProcesses
      : parseInteger('--client-processes', clientProcesses, { min: 1 }),
    connections: options.get('--connections') === undefined
      ? profile.connections
      : parseInteger('--connections', options.get('--connections'), { min: 1 }),
    streamsPerConnection: options.get('--streams-per-connection') === undefined
      ? profile.streamsPerConnection
      : parseInteger('--streams-per-connection', options.get('--streams-per-connection'), { min: 1 }),
    messageSize: options.get('--message-size') === undefined
      ? profile.messageSize
      : parseByteSize('--message-size', options.get('--message-size')),
    rounds: options.get('--rounds') === undefined
      ? profile.rounds
      : parseInteger('--rounds', options.get('--rounds'), { min: 1 }),
    pauseMs: options.get('--pause-ms') === undefined
      ? profile.pauseMs
      : parseInteger('--pause-ms', options.get('--pause-ms'), { min: 0 }),
    timeoutMs: options.get('--timeout-ms') === undefined
      ? profile.timeoutMs
      : parseInteger('--timeout-ms', options.get('--timeout-ms'), { min: 1 }),
    statsIntervalMs: options.get('--stats-interval-ms') === undefined
      ? 1000
      : parseInteger('--stats-interval-ms', options.get('--stats-interval-ms'), { min: 50 }),
    serverRuntimeMode,
    clientRuntimeMode,
    serverFallbackPolicy,
    clientFallbackPolicy,
    allowErrors: flags.has('--allow-errors'),
    json: flags.has('--json'),
  };
}

function createServerTelemetry() {
  return {
    latest: null,
    summary: null,
    runtimeInfo: null,
    maxSessions: 0,
    maxStreams: 0,
    maxBytesEchoed: 0,
    peakRss: 0,
    peakHeapUsed: 0,
  };
}

function updateServerTelemetry(telemetry, message) {
  telemetry.latest = message;
  if (message.type === 'summary') {
    telemetry.summary = message;
  }
  if (message.runtimeInfo) {
    telemetry.runtimeInfo = message.runtimeInfo;
  }
  telemetry.maxSessions = Math.max(telemetry.maxSessions, message.sessionCount ?? 0);
  telemetry.maxStreams = Math.max(telemetry.maxStreams, message.streamCount ?? 0);
  telemetry.maxBytesEchoed = Math.max(telemetry.maxBytesEchoed, message.bytesEchoed ?? 0);
  telemetry.peakRss = Math.max(telemetry.peakRss, message.rss ?? 0);
  telemetry.peakHeapUsed = Math.max(telemetry.peakHeapUsed, message.heapUsed ?? 0);
}

function startServer(config) {
  return new Promise((resolve, reject) => {
    const telemetry = createServerTelemetry();
    const child = spawn(
      process.execPath,
      [BENCH_SERVER_PATH, JSON.stringify(config)],
      { cwd: ROOT_DIR, stdio: ['pipe', 'pipe', 'inherit'] },
    );

    let readyResolved = false;
    let readySettled = false;
    let exitSettled = false;
    let stdoutBuffer = '';

    let resolveExit;
    let rejectExit;
    const exitPromise = new Promise((resolvePromise, rejectPromise) => {
      resolveExit = resolvePromise;
      rejectExit = rejectPromise;
    });

    const failReady = (error) => {
      if (readySettled) {
        return;
      }
      readySettled = true;
      clearTimeout(startupTimeout);
      reject(error);
    };

    const settleExit = (error) => {
      if (exitSettled) {
        return;
      }
      exitSettled = true;
      if (error) {
        rejectExit(error);
      } else {
        resolveExit();
      }
    };

    const startupTimeout = setTimeout(() => {
      child.kill('SIGKILL');
      const error = new Error('QUIC benchmark server startup timed out');
      failReady(error);
      settleExit(error);
    }, 15_000);

    child.stdout.on('data', (data) => {
      stdoutBuffer += data.toString();
      const lines = stdoutBuffer.split('\n');
      stdoutBuffer = lines.pop() ?? '';

      for (const line of lines) {
        if (!line.trim()) {
          continue;
        }

        try {
          const message = JSON.parse(line);
          if (message.type === 'stats' || message.type === 'summary') {
            updateServerTelemetry(telemetry, message);
            continue;
          }

          if (message.type === 'ready' && !readySettled) {
            readySettled = true;
            readyResolved = true;
            clearTimeout(startupTimeout);
            if (message.runtimeInfo) {
              telemetry.runtimeInfo = message.runtimeInfo;
            }
            resolve({
              port: message.port,
              address: message.address,
              telemetry,
              async stop() {
                if (child.exitCode === null && child.signalCode === null) {
                  child.kill('SIGTERM');
                }

                await Promise.race([
                  exitPromise,
                  new Promise((_, rejectPromise) => {
                    setTimeout(() => {
                      rejectPromise(new Error('QUIC benchmark server shutdown timed out'));
                    }, 10_000);
                  }),
                ]);
              },
            });
          }
        } catch {
          // Ignore non-JSON output from the benchmark server.
        }
      }
    });

    child.on('error', (error) => {
      failReady(error);
      settleExit(error);
    });

    child.on('exit', (code, signal) => {
      clearTimeout(startupTimeout);

      if (signal) {
        const error = new Error(`QUIC benchmark server exited via signal ${signal}`);
        if (!readyResolved) {
          failReady(error);
        }
        settleExit(error);
        return;
      }

      if (code !== 0) {
        const error = new Error(`QUIC benchmark server exited with code ${code}`);
        if (!readyResolved) {
          failReady(error);
        }
        settleExit(error);
        return;
      }

      settleExit(null);
    });
  });
}

function runClient(config) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [BENCH_CLIENT_PATH, JSON.stringify(config)],
      { cwd: ROOT_DIR, stdio: ['ignore', 'pipe', 'pipe'] },
    );

    let stdoutBuffer = '';
    let stderrBuffer = '';
    let result = null;

    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`QUIC benchmark client ${config.clientId ?? '?'} timed out`));
    }, config.timeoutMs + 10_000);

    child.stdout.on('data', (data) => {
      stdoutBuffer += data.toString();
      const lines = stdoutBuffer.split('\n');
      stdoutBuffer = lines.pop() ?? '';

      for (const line of lines) {
        if (!line.trim()) {
          continue;
        }

        try {
          const message = JSON.parse(line);
          if (message.type === 'result') {
            result = message;
          }
        } catch {
          // Ignore non-JSON output from the benchmark client.
        }
      }
    });

    child.stderr.on('data', (data) => {
      const text = data.toString();
      stderrBuffer += text;
      process.stderr.write(text);
    });

    child.on('error', (error) => {
      clearTimeout(timeout);
      reject(error);
    });

    child.on('exit', (code, signal) => {
      clearTimeout(timeout);

      if (signal) {
        reject(new Error(`QUIC benchmark client ${config.clientId ?? '?'} exited via signal ${signal}`));
        return;
      }

      if (code !== 0) {
        reject(
          new Error(
            `QUIC benchmark client ${config.clientId ?? '?'} exited with code ${code}\n${stderrBuffer}`.trim(),
          ),
        );
        return;
      }

      if (!result) {
        reject(new Error(`QUIC benchmark client ${config.clientId ?? '?'} exited without a result`));
        return;
      }

      resolve(result);
    });
  });
}

function aggregateProcessResults(results, wallElapsedMs) {
  const connectionP95s = results.map((result) => result.connEstablish.p95Ms);
  const streamP50s = results.map((result) => result.streamLatency.p50Ms);
  const streamP95s = results.map((result) => result.streamLatency.p95Ms);
  const streamP99s = results.map((result) => result.streamLatency.p99Ms);
  const runtimeSelections = {};

  const totalStreams = sumBy(results, (result) => result.totalStreams);
  const totalBytes = sumBy(results, (result) => result.totalBytes);
  const errors = sumBy(results, (result) => result.errors);
  const totalCpuUserMs = sumBy(results, (result) => result.cpu.userMs);
  const totalCpuSystemMs = sumBy(results, (result) => result.cpu.systemMs);

  for (const result of results) {
    mergeCountObjects(runtimeSelections, result.runtimeSelections);
  }

  return {
    totalStreams,
    totalBytes,
    errors,
    wallElapsedMs,
    throughputMbps: wallElapsedMs > 0 ? (totalBytes * 8) / (wallElapsedMs / 1000) / 1_000_000 : 0,
    streamsPerSecond: wallElapsedMs > 0 ? totalStreams / (wallElapsedMs / 1000) : 0,
    connectionCount: sumBy(results, (result) => result.connEstablish.count),
    connectionMeanMs: weightedMean(
      results,
      (result) => result.connEstablish.meanMs,
      (result) => result.connEstablish.count,
    ),
    connectionP95s,
    streamMeanMs: weightedMean(
      results,
      (result) => result.streamLatency.meanMs,
      (result) => result.streamLatency.count,
    ),
    streamP50s,
    streamP95s,
    streamP99s,
    runtimeSelections,
    totalCpuUserMs,
    totalCpuSystemMs,
    totalCpuUtilizationPct: wallElapsedMs > 0
      ? ((totalCpuUserMs + totalCpuSystemMs) / wallElapsedMs) * 100
      : 0,
  };
}

function printSummary(summary) {
  const {
    settings,
    requestedStreams,
    totalStreams,
    errors,
    wallElapsedMs,
    throughputMbps,
    streamsPerSecond,
    clientStats,
    rounds,
    serverStats,
  } = summary;

  console.log('Raw QUIC benchmark');
  console.log(`  Profile: ${settings.profileName}`);
  console.log(
    `  Load: ${settings.clientProcesses} client processes x ${settings.connections} connections` +
    ` x ${settings.streamsPerConnection} streams x ${formatBytes(settings.messageSize)} x ${settings.rounds} rounds`,
  );
  console.log(
    `  Runtime: server=${settings.serverRuntimeMode ?? 'default'}/${settings.serverFallbackPolicy ?? 'default'}` +
    ` client=${settings.clientRuntimeMode ?? 'default'}/${settings.clientFallbackPolicy ?? 'default'}`,
  );
  console.log(`  Requested streams: ${requestedStreams}`);
  console.log(`  Completed streams: ${totalStreams}`);
  console.log(`  Errors: ${errors}`);
  console.log(`  Wall time: ${wallElapsedMs}ms`);
  console.log(`  Throughput: ${throughputMbps.toFixed(1)} Mbps`);
  console.log(`  Streams/sec: ${streamsPerSecond.toFixed(0)}`);
  console.log(`  Connection setup mean: ${clientStats.connectionMeanMs.toFixed(2)}ms weighted`);
  console.log(`  Connection setup p95: ${formatRange(clientStats.connectionP95s)}`);
  console.log(`  Stream latency mean: ${clientStats.streamMeanMs.toFixed(2)}ms weighted`);
  console.log(`  Stream latency p50: ${formatRange(clientStats.streamP50s)}`);
  console.log(`  Stream latency p95: ${formatRange(clientStats.streamP95s)}`);
  console.log(`  Stream latency p99: ${formatRange(clientStats.streamP99s)}`);
  console.log(
    `  Client CPU: user=${clientStats.totalCpuUserMs}ms sys=${clientStats.totalCpuSystemMs}ms` +
    ` util=${clientStats.totalCpuUtilizationPct.toFixed(1)}%`,
  );
  console.log(`  Client runtime selections: ${formatCountSummary(clientStats.runtimeSelections)}`);

  if (serverStats) {
    console.log(`  Server runtime selected: ${formatRuntimeInfo(serverStats.runtimeInfo)}`);
    console.log(
      `  Server observed: sessions=${serverStats.final.sessionCount} (peak ${serverStats.maxSessions}),` +
      ` streams=${serverStats.final.streamCount} (peak ${serverStats.maxStreams}),` +
      ` echoed=${formatBytes(serverStats.final.bytesEchoed)}`,
    );
    console.log(
      `  Server memory: peak rss=${serverStats.peakRssMB.toFixed(1)}MB` +
      ` peak heap=${serverStats.peakHeapUsedMB.toFixed(1)}MB`,
    );
    console.log(
      `  Server CPU: user=${serverStats.cpuUserMs}ms sys=${serverStats.cpuSystemMs}ms` +
      ` util=${serverStats.cpuUtilizationPct.toFixed(1)}%`,
    );
  }

  console.log('  Rounds:');
  for (const round of rounds) {
    console.log(
      `    ${round.round}. ${round.elapsedMs}ms, ${round.totalStreams}/${round.expectedStreams} ok,` +
      ` ${round.errors} err, ${round.throughputMbps.toFixed(1)} Mbps, ${round.streamsPerSecond.toFixed(0)} s/s`,
    );
  }
}

async function main() {
  requireBuildArtifacts();
  const settings = resolveSettings(process.argv.slice(2));

  const serverConfig = {
    host: settings.host,
    port: 0,
    statsIntervalMs: settings.statsIntervalMs,
    runtimeMode: settings.serverRuntimeMode,
    fallbackPolicy: settings.serverFallbackPolicy,
  };
  const clientConfig = {
    host: settings.host,
    connections: settings.connections,
    streamsPerConnection: settings.streamsPerConnection,
    messageSize: settings.messageSize,
    timeoutMs: settings.timeoutMs,
    runtimeMode: settings.clientRuntimeMode,
    fallbackPolicy: settings.clientFallbackPolicy,
  };

  const server = await startServer(serverConfig);
  const wallStart = Date.now();
  const roundSummaries = [];
  const allResults = [];
  const requestedStreamsPerRound = settings.clientProcesses * settings.connections * settings.streamsPerConnection;

  try {
    for (let round = 0; round < settings.rounds; round += 1) {
      const roundStart = Date.now();
      const results = await Promise.all(
        Array.from({ length: settings.clientProcesses }, (_, index) => runClient({
          ...clientConfig,
          port: server.port,
          clientId: round * settings.clientProcesses + index + 1,
        })),
      );
      const roundElapsedMs = Date.now() - roundStart;
      const aggregate = aggregateProcessResults(results, roundElapsedMs);
      allResults.push(...results);
      roundSummaries.push({
        round: round + 1,
        expectedStreams: requestedStreamsPerRound,
        totalStreams: aggregate.totalStreams,
        errors: aggregate.errors,
        elapsedMs: roundElapsedMs,
        throughputMbps: aggregate.throughputMbps,
        streamsPerSecond: aggregate.streamsPerSecond,
      });

      if (round < settings.rounds - 1 && settings.pauseMs > 0) {
        await sleep(settings.pauseMs);
      }
    }
  } finally {
    await server.stop();
  }

  const wallElapsedMs = Date.now() - wallStart;
  const clientStats = aggregateProcessResults(allResults, wallElapsedMs);
  const requestedStreams = requestedStreamsPerRound * settings.rounds;
  const finalServerSnapshot = server.telemetry.summary ?? server.telemetry.latest;
  const serverRuntimeInfo = finalServerSnapshot?.runtimeInfo ?? server.telemetry.runtimeInfo ?? null;

  const summary = {
    settings,
    requestedStreams,
    totalStreams: clientStats.totalStreams,
    errors: clientStats.errors,
    wallElapsedMs,
    throughputMbps: clientStats.throughputMbps,
    streamsPerSecond: clientStats.streamsPerSecond,
    clientStats,
    rounds: roundSummaries,
    serverStats: finalServerSnapshot
      ? {
          final: finalServerSnapshot,
          runtimeInfo: serverRuntimeInfo,
          maxSessions: server.telemetry.maxSessions,
          maxStreams: server.telemetry.maxStreams,
          maxBytesEchoed: server.telemetry.maxBytesEchoed,
          peakRssMB: server.telemetry.peakRss / 1_000_000,
          peakHeapUsedMB: server.telemetry.peakHeapUsed / 1_000_000,
          cpuUserMs: Math.round((finalServerSnapshot.cpuUser ?? 0) / 1000),
          cpuSystemMs: Math.round((finalServerSnapshot.cpuSystem ?? 0) / 1000),
          cpuUtilizationPct: wallElapsedMs > 0
            ? (((finalServerSnapshot.cpuUser ?? 0) + (finalServerSnapshot.cpuSystem ?? 0)) / 1000 / wallElapsedMs) * 100
            : 0,
        }
      : null,
  };

  if (settings.json) {
    process.stdout.write(`${JSON.stringify(summary)}\n`);
  } else {
    printSummary(summary);
  }

  if (!settings.allowErrors && summary.errors > 0) {
    process.exitCode = 1;
  }
}

await main();
