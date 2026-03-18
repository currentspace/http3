import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import inspector from 'node:inspector';
import os from 'node:os';
import { createRequire } from 'node:module';
import { monitorEventLoopDelay, performance } from 'node:perf_hooks';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  captureEnvironmentMetadata,
  writeJsonArtifact,
  writeTextArtifact,
} from './perf-artifacts.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const require = createRequire(import.meta.url);

const PRESETS = {
  smoke: {
    streamsPerConnection: 256,
    payloadBytes: 1024,
    responseFragmentCount: 1,
    responseFragmentSize: null,
    timeoutMs: 10_000,
    listenerFanout: 1,
  },
  'bridge-medium': {
    streamsPerConnection: 1_024,
    payloadBytes: 1_024,
    responseFragmentCount: 4,
    responseFragmentSize: null,
    timeoutMs: 15_000,
    listenerFanout: 1,
  },
  'bridge-heavy': {
    streamsPerConnection: 2_048,
    payloadBytes: 1_024,
    responseFragmentCount: 8,
    responseFragmentSize: null,
    timeoutMs: 20_000,
    listenerFanout: 2,
  },
  'bridge-soak': {
    streamsPerConnection: 4_096,
    payloadBytes: 512,
    responseFragmentCount: 8,
    responseFragmentSize: 256,
    timeoutMs: 30_000,
    listenerFanout: 4,
  },
};

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (
      arg === '--help' ||
      arg === '--no-build' ||
      arg === '--cpu-prof' ||
      arg === '--clone-buffers' ||
      arg === '--materialize-events'
    ) {
      flags.add(arg);
      continue;
    }

    if (
      arg === '--preset' ||
      arg === '--sink-mode' ||
      arg === '--callback-mode' ||
      arg === '--streams-per-connection' ||
      arg === '--payload-bytes' ||
      arg === '--response-fragment-count' ||
      arg === '--response-fragment-size' ||
      arg === '--listener-fanout' ||
      arg === '--timeout-ms' ||
      arg === '--poll-ms' ||
      arg === '--trace-input' ||
      arg === '--results-dir' ||
      arg === '--label' ||
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
  console.log(`Node mock QUIC profiler

Usage:
  node scripts/profile-node-mock-transport.mjs [options]

Options:
  --preset smoke|bridge-medium|bridge-heavy|bridge-soak
                                       Workload preset (default: smoke)
  --sink-mode counting|tsfn            Rust sink mode (default: counting)
  --callback-mode noop|touch-data      JS callback work level (default: noop)
  --streams-per-connection N           Override streams in the mock QUIC session
  --payload-bytes N                    Override bytes per stream payload
  --response-fragment-count N          Fragment each echoed response into N sends
  --response-fragment-size N           Fragment echoed responses into N-byte chunks
  --listener-fanout N                  Simulated JS listener fanout (default from preset)
  --clone-buffers                      Clone event data buffers in JS callback work
  --materialize-events                 Materialize plain JS objects per event
  --cpu-prof                           Capture a CPU profile for the run
  --timeout-ms N                       End-to-end timeout override
  --poll-ms N                          Result polling interval (default: 20)
  --trace-input PATH                   Replay a captured command/event trace
  --alpn VALUE                         ALPN string (default: quic)
  --server-name VALUE                  Mock SNI / hostname (default: localhost)
  --results-dir DIR                    Artifact directory (default: perf-results)
  --label TEXT                         Optional artifact label suffix
  --no-build                           Reuse the existing native binding
  --help                               Show this help

Suggested modes:
  counting baseline:
    node scripts/profile-node-mock-transport.mjs --preset bridge-medium --sink-mode counting --callback-mode noop

  tsfn-only overhead:
    node scripts/profile-node-mock-transport.mjs --preset bridge-medium --sink-mode tsfn --callback-mode noop

  JS listener overhead:
    node scripts/profile-node-mock-transport.mjs --preset bridge-heavy --sink-mode tsfn --callback-mode touch-data --listener-fanout 4 --clone-buffers --materialize-events
`);
}

function runChecked(command, args, { cwd = ROOT_DIR, input = null, encoding = 'utf8' } = {}) {
  const result = spawnSync(command, args, {
    cwd,
    encoding,
    input,
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed with status ${result.status}\n${result.stderr ?? ''}`);
  }
  return result;
}

function requireTool(binary) {
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

  requireTool('openssl');
  const preset = options.get('--preset') ?? 'smoke';
  if (!Object.hasOwn(PRESETS, preset)) {
    throw new Error(`Unknown preset: ${preset}`);
  }
  const presetSettings = PRESETS[preset];

  const sinkMode = options.get('--sink-mode') ?? 'counting';
  if (sinkMode !== 'counting' && sinkMode !== 'tsfn') {
    throw new Error('--sink-mode must be counting or tsfn');
  }

  const callbackMode = options.get('--callback-mode') ?? 'noop';
  if (callbackMode !== 'noop' && callbackMode !== 'touch-data') {
    throw new Error('--callback-mode must be noop or touch-data');
  }

  const listenerFanout = Number.parseInt(
    options.get('--listener-fanout') ?? String(presetSettings.listenerFanout),
    10,
  );
  if (!Number.isInteger(listenerFanout) || listenerFanout < 1) {
    throw new Error('--listener-fanout must be a positive integer');
  }

  const responseFragmentCount = Number.parseInt(
    options.get('--response-fragment-count') ?? String(presetSettings.responseFragmentCount),
    10,
  );
  if (!Number.isInteger(responseFragmentCount) || responseFragmentCount < 1) {
    throw new Error('--response-fragment-count must be a positive integer');
  }

  return {
    preset,
    sinkMode,
    callbackMode,
    streamsPerConnection: Number.parseInt(
      options.get('--streams-per-connection') ?? String(presetSettings.streamsPerConnection),
      10,
    ),
    payloadBytes: Number.parseInt(
      options.get('--payload-bytes') ?? String(presetSettings.payloadBytes),
      10,
    ),
    responseFragmentCount,
    responseFragmentSize: options.has('--response-fragment-size')
      ? Number.parseInt(options.get('--response-fragment-size'), 10)
      : presetSettings.responseFragmentSize,
    listenerFanout,
    cloneBuffers: flags.has('--clone-buffers'),
    materializeEvents: flags.has('--materialize-events'),
    cpuProf: flags.has('--cpu-prof'),
    timeoutMs: Number.parseInt(options.get('--timeout-ms') ?? String(presetSettings.timeoutMs), 10),
    pollMs: Number.parseInt(options.get('--poll-ms') ?? '20', 10),
    traceInput: options.get('--trace-input') ?? null,
    alpn: options.get('--alpn') ?? 'quic',
    serverName: options.get('--server-name') ?? 'localhost',
    resultsDir: options.get('--results-dir') ?? 'perf-results',
    label: options.get('--label') ?? null,
    noBuild: flags.has('--no-build'),
  };
}

function buildBinding() {
  runChecked('npm', ['run', 'build']);
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

function loadBinding() {
  const binding = require(resolve(ROOT_DIR, 'index.js'));
  if (typeof binding.NativeMockQuicProfiler !== 'function') {
    throw new Error('The loaded native binding is missing NativeMockQuicProfiler');
  }
  return binding;
}

function loadReplayTrace(traceInput) {
  if (!traceInput) {
    return null;
  }
  const payload = JSON.parse(readFileSync(resolve(ROOT_DIR, traceInput), 'utf8'));
  if (payload?.trace_type === 'mock-replay-trace') {
    return payload;
  }
  if (payload?.trace?.trace_type === 'mock-replay-trace') {
    return payload.trace;
  }
  throw new Error(`Trace file does not contain a mock replay trace: ${traceInput}`);
}

function sleep(ms) {
  return new Promise(resolvePromise => {
    setTimeout(resolvePromise, ms);
  });
}

function createCallbackStats(callbackMode) {
  return {
    callbackMode,
    listenerFanout: 1,
    batches: 0,
    events: 0,
    listenerDispatches: 0,
    bytesTouched: 0,
    headersTouched: 0,
    metaReads: 0,
    objectMaterializations: 0,
    bufferCopies: 0,
    bufferBytesCloned: 0,
    totalCallbackDurationMs: 0,
    maxCallbackDurationMs: 0,
    errors: [],
  };
}

function observeEvent(event, stats, settings) {
  stats.events += 1;
  for (let listenerIndex = 0; listenerIndex < settings.listenerFanout; listenerIndex += 1) {
    stats.listenerDispatches += 1;
    if (!callbackModeRequiresTouch(stats.callbackMode)) {
      continue;
    }
    if (event?.data) {
      stats.bytesTouched += event.data.length;
      if (settings.cloneBuffers) {
        const cloned = Buffer.from(event.data);
        stats.bufferCopies += 1;
        stats.bufferBytesCloned += cloned.length;
      }
    }
    if (Array.isArray(event?.headers)) {
      stats.headersTouched += event.headers.length;
    }
    if (event?.meta) {
      if (event.meta.errorReason) {
        stats.metaReads += event.meta.errorReason.length;
      }
      if (event.meta.serverName) {
        stats.metaReads += event.meta.serverName.length;
      }
      if (event.meta.runtimeDriver) {
        stats.metaReads += event.meta.runtimeDriver.length;
      }
    }
    if (settings.materializeEvents) {
      void ({
        eventType: event?.eventType ?? null,
        connHandle: event?.connHandle ?? null,
        streamId: event?.streamId ?? null,
        dataLength: event?.data?.length ?? 0,
        fin: event?.fin ?? null,
        headerCount: Array.isArray(event?.headers) ? event.headers.length : 0,
        reasonCode: event?.meta?.reasonCode ?? null,
      });
      stats.objectMaterializations += 1;
    }
  }
}

function callbackModeRequiresTouch(callbackMode) {
  return callbackMode === 'touch-data';
}

async function waitForResult(profiler, timeoutMs, pollMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const json = profiler.pollResultJson();
    if (typeof json === 'string' && json.length > 0) {
      return json;
    }
    await sleep(pollMs);
  }
  throw new Error(`Timed out waiting for mock profile result after ${timeoutMs}ms`);
}

function snapshotEventLoopDelay(histogram) {
  if (!histogram) {
    return null;
  }
  const nsToMs = (value) => Number(value) / 1_000_000;
  return {
    minMs: nsToMs(histogram.min),
    meanMs: nsToMs(histogram.mean),
    maxMs: nsToMs(histogram.max),
    stddevMs: nsToMs(histogram.stddev),
    p50Ms: nsToMs(histogram.percentile(50)),
    p95Ms: nsToMs(histogram.percentile(95)),
    p99Ms: nsToMs(histogram.percentile(99)),
  };
}

function snapshotMemoryDelta(startUsage, endUsage) {
  return {
    rssDeltaBytes: endUsage.rss - startUsage.rss,
    heapTotalDeltaBytes: endUsage.heapTotal - startUsage.heapTotal,
    heapUsedDeltaBytes: endUsage.heapUsed - startUsage.heapUsed,
    externalDeltaBytes: endUsage.external - startUsage.external,
    arrayBuffersDeltaBytes: endUsage.arrayBuffers - startUsage.arrayBuffers,
  };
}

function inspectorPost(session, method, params = {}) {
  return new Promise((resolvePromise, reject) => {
    session.post(method, params, (error, result) => {
      if (error) {
        reject(error);
        return;
      }
      resolvePromise(result ?? {});
    });
  });
}

async function startCpuProfiler(enabled) {
  if (!enabled) {
    return null;
  }
  const session = new inspector.Session();
  session.connect();
  await inspectorPost(session, 'Profiler.enable');
  await inspectorPost(session, 'Profiler.start');
  return {
    async stop() {
      const { profile } = await inspectorPost(session, 'Profiler.stop');
      session.disconnect();
      return profile;
    },
  };
}

async function main() {
  const settings = resolveSettings(process.argv.slice(2));
  if (!settings.noBuild) {
    buildBinding();
  }

  const tempDir = createTempDir('http3-mock-profile-');
  const certs = generateCertPair(tempDir);
  const binding = loadBinding();
  const replayTrace = loadReplayTrace(settings.traceInput);
  const stats = createCallbackStats(settings.callbackMode);
  stats.listenerFanout = settings.listenerFanout;
  const memoryStart = process.memoryUsage();
  const eventLoopDelay = monitorEventLoopDelay({ resolution: 20 });
  eventLoopDelay.enable();
  const eventLoopUtilizationStart = performance.eventLoopUtilization();
  const wallStart = performance.now();
  const cpuProfiler = await startCpuProfiler(settings.cpuProf);
  let profiler = null;

  try {
    profiler = new binding.NativeMockQuicProfiler(
      {
        key: readFileSync(certs.keyPath),
        cert: readFileSync(certs.certPath),
        ca: readFileSync(certs.certPath),
        alpn: [settings.alpn],
        rejectUnauthorized: false,
        serverName: settings.serverName,
        streamsPerConnection: settings.streamsPerConnection,
        payloadBytes: settings.payloadBytes,
        responseFragmentCount: settings.responseFragmentCount,
        responseFragmentSize: settings.responseFragmentSize ?? undefined,
        timeoutMs: settings.timeoutMs,
        sinkMode: settings.sinkMode,
        replayTraceJson: replayTrace ? JSON.stringify(replayTrace) : undefined,
      },
      (err, events) => {
        if (err) {
          stats.errors.push(err.message);
          return;
        }
        stats.batches += 1;
        const callbackStart = performance.now();
        for (const event of events) {
          observeEvent(event, stats, settings);
        }
        const callbackDurationMs = performance.now() - callbackStart;
        stats.totalCallbackDurationMs += callbackDurationMs;
        stats.maxCallbackDurationMs = Math.max(stats.maxCallbackDurationMs, callbackDurationMs);
      },
    );

    const resultJson = await waitForResult(
      profiler,
      settings.timeoutMs + 5_000,
      settings.pollMs,
    );
    const summary = JSON.parse(resultJson);
    const wallElapsedMs = performance.now() - wallStart;
    const memoryEnd = process.memoryUsage();
    const eventLoopUtilization = performance.eventLoopUtilization(eventLoopUtilizationStart);
    eventLoopDelay.disable();
    const cpuProfile = cpuProfiler ? await cpuProfiler.stop() : null;
    const cpuProfileArtifact = cpuProfile
      ? writeTextArtifact({
          rootDir: ROOT_DIR,
          resultsDir: settings.resultsDir,
          prefix: 'quic-mock-cpu-profile',
          label: settings.label ?? `${settings.preset}-${settings.sinkMode}-${settings.callbackMode}`,
          extension: 'cpuprofile',
          content: `${JSON.stringify(cpuProfile)}\n`,
        })
      : null;
    const payload = {
      harness: 'quic-mock',
      preset: settings.preset,
      sinkMode: settings.sinkMode,
      callbackMode: settings.callbackMode,
      environment: captureEnvironmentMetadata({
        runner: 'profile-node-mock-transport',
        protocol: 'quic',
        target: 'node-mock-transport',
        label: settings.label,
        extra: {
          mockTransport: {
            preset: settings.preset,
            sinkMode: settings.sinkMode,
            callbackMode: settings.callbackMode,
            configuredStreamsPerConnection: settings.streamsPerConnection,
            configuredPayloadBytes: settings.payloadBytes,
            effectiveStreamsPerConnection: summary.streams_per_connection,
            effectivePayloadBytes: summary.payload_bytes,
            responseFragmentCount: settings.responseFragmentCount,
            responseFragmentSize: settings.responseFragmentSize,
            listenerFanout: settings.listenerFanout,
            cloneBuffers: settings.cloneBuffers,
            materializeEvents: settings.materializeEvents,
            timeoutMs: settings.timeoutMs,
            traceInput: settings.traceInput,
          },
        },
      }),
      jsCallback: stats,
      jsRuntime: {
        wallElapsedMs,
        eventLoopUtilization,
        eventLoopDelay: snapshotEventLoopDelay(eventLoopDelay),
        memoryDelta: snapshotMemoryDelta(memoryStart, memoryEnd),
      },
      cpuProfileArtifact: cpuProfileArtifact?.relativePath ?? null,
      summary,
    };
    const artifact = writeJsonArtifact({
      rootDir: ROOT_DIR,
      resultsDir: settings.resultsDir,
      prefix: 'quic-mock-profile',
      label: settings.label ?? `${settings.sinkMode}-${settings.callbackMode}`,
      payload,
    });

    console.log(
      JSON.stringify(
        {
          harness: payload.harness,
          sinkMode: payload.sinkMode,
          callbackMode: payload.callbackMode,
          completedStreams: summary.completed_streams,
          requestedStreams: summary.requested_streams,
          throughputMbps: summary.throughput_mbps,
          cpuProfileArtifact: cpuProfileArtifact?.relativePath ?? null,
          artifact: artifact?.relativePath ?? null,
        },
        null,
        2,
      ),
    );
  } finally {
    if (cpuProfiler) {
      try {
        await cpuProfiler.stop();
      } catch {
        // Best-effort cleanup.
      }
    }
    eventLoopDelay.disable();
    if (profiler) {
      profiler.shutdown();
    }
    rmSync(tempDir, { recursive: true, force: true });
  }
}

await main();
