import { readdirSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(fileURLToPath(new URL('..', import.meta.url)));

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '--json') {
      flags.add(arg);
      continue;
    }

    if (arg === '--results-dir') {
      const value = argv[index + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('--results-dir requires a value');
      }
      options.set('--results-dir', value);
      index += 1;
      continue;
    }
  }

  return { options, flags };
}

function printHelp() {
  console.log(`Analyze persisted performance artifacts

Usage:
  npm run perf:analyze -- [--results-dir perf-results] [--json]

What it does:
  - reads host benchmark summaries and Docker benchmark matrices
  - reads Rust loopback, quiche-direct floor, and Node mock transport profiles
  - flattens them into comparable client/server role records
  - groups by protocol, role, harness, sink kind, backend, runtime mode, topology policy, and container policy
  - prints ranges for throughput, latency, CPU, and driver setup counts
`);
}

function walkJsonFiles(rootDir) {
  const files = [];
  const stack = [rootDir];

  while (stack.length > 0) {
    const current = stack.pop();
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const absolutePath = resolve(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(absolutePath);
      } else if (entry.isFile() && entry.name.endsWith('.json')) {
        files.push(absolutePath);
      }
    }
  }

  return files.sort();
}

function readJsonSafe(path) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

function parseRuntimeSelection(selection) {
  if (!selection) {
    return null;
  }

  const [modes, driver, fallback] = selection.split('/');
  if (!modes || !driver || !fallback) {
    return null;
  }
  const [requestedMode, selectedMode] = modes.split('->');
  if (!requestedMode || !selectedMode) {
    return null;
  }
  return {
    requestedMode,
    selectedMode,
    driver,
    fallbackOccurred: fallback === 'fallback',
  };
}

function summarizeClientRuntimeSelections(runtimeSelections) {
  const entries = Object.entries(runtimeSelections ?? {});
  if (entries.length === 0) {
    return {
      requestedMode: 'unknown',
      selectedMode: 'unknown',
      driver: 'unknown',
      fallbackOccurred: null,
      rawSelections: {},
    };
  }

  if (entries.length === 1) {
    const [[selection, count]] = entries;
    const parsed = parseRuntimeSelection(selection);
    if (parsed) {
      return {
        ...parsed,
        count,
        rawSelections: Object.fromEntries(entries),
      };
    }
  }

  const parsedSelections = entries
    .map(([selection, count]) => {
      const parsed = parseRuntimeSelection(selection);
      return {
        selection,
        count,
        requestedMode: parsed?.requestedMode ?? 'unknown',
        selectedMode: parsed?.selectedMode ?? 'unknown',
        driver: parsed?.driver ?? 'unknown',
      };
    });

  return {
    requestedMode: 'mixed',
    selectedMode: 'mixed',
    driver: parsedSelections.map((entry) => entry.driver).join(','),
    fallbackOccurred: null,
    rawSelections: Object.fromEntries(entries),
  };
}

function maxOf(values) {
  if (!Array.isArray(values) || values.length === 0) {
    return 0;
  }
  return Math.max(...values);
}

function rangeText(values, digits = 1, suffix = '') {
  const finite = values.filter((value) => Number.isFinite(value));
  if (finite.length === 0) {
    return 'n/a';
  }
  const min = Math.min(...finite);
  const max = Math.max(...finite);
  if (Math.abs(min - max) < Number.EPSILON) {
    return `${max.toFixed(digits)}${suffix}`;
  }
  return `${min.toFixed(digits)}-${max.toFixed(digits)}${suffix}`;
}

function classifyDockerPolicy(dockerLane) {
  if (!dockerLane) {
    return 'host';
  }
  if (dockerLane.includes('privileged')) {
    return 'privileged';
  }
  if (dockerLane.includes('unconfined')) {
    return 'seccomp=unconfined';
  }
  if (dockerLane.includes('cap-add')) {
    return 'cap-add';
  }
  if (dockerLane.includes('ordinary')) {
    return 'ordinary';
  }
  return dockerLane;
}

function deriveClientTopology(telemetry) {
  const sharedWorkers = (telemetry.rawQuicClientSharedWorkersCreated ?? 0) + (telemetry.h3ClientSharedWorkersCreated ?? 0);
  const dedicatedWorkers = (telemetry.rawQuicClientDedicatedWorkerSpawns ?? 0) + (telemetry.h3ClientDedicatedWorkerSpawns ?? 0);
  if (sharedWorkers > 0 || (telemetry.clientLocalPortReuseHits ?? 0) > 0) {
    return 'shared-per-port';
  }
  if (dedicatedWorkers > 0) {
    return 'dedicated-per-session';
  }
  return 'unknown';
}

function deriveServerTopology(telemetry) {
  const workers = (telemetry.rawQuicServerWorkerSpawns ?? 0) + (telemetry.h3ServerWorkerSpawns ?? 0);
  return workers > 0 ? 'one-worker-per-bound-port' : 'unknown';
}

function inferDriverFromTelemetry(telemetry) {
  if ((telemetry?.ioUringDriverSetupSuccesses ?? 0) > 0) {
    return 'io_uring';
  }
  if ((telemetry?.kqueueDriverSetupSuccesses ?? 0) > 0) {
    return 'kqueue';
  }
  if ((telemetry?.pollDriverSetupSuccesses ?? 0) > 0) {
    return 'poll';
  }
  if ((telemetry?.workerThreadSpawnsTotal ?? 0) > 0) {
    return 'mock';
  }
  return 'unknown';
}

function buildBackendSignals(telemetry, extra = {}) {
  return {
    ioUringPendingTxHighWatermark: telemetry?.ioUringPendingTxHighWatermark ?? 0,
    ioUringRetryableSendCompletions: telemetry?.ioUringRetryableSendCompletions ?? 0,
    ioUringSubmitCalls: telemetry?.ioUringSubmitCalls ?? 0,
    ioUringSubmitWithArgsCalls: telemetry?.ioUringSubmitWithArgsCalls ?? 0,
    ioUringSubmittedSqesTotal: telemetry?.ioUringSubmittedSqesTotal ?? 0,
    ioUringCompletionTotal: telemetry?.ioUringCompletionTotal ?? 0,
    ioUringCompletionBatchHighWatermark: telemetry?.ioUringCompletionBatchHighWatermark ?? 0,
    ioUringWakeCompletions: telemetry?.ioUringWakeCompletions ?? 0,
    ioUringWakeWrites: telemetry?.ioUringWakeWrites ?? 0,
    ioUringTimeoutPolls: telemetry?.ioUringTimeoutPolls ?? 0,
    ioUringRxDatagramsTotal: telemetry?.ioUringRxDatagramsTotal ?? 0,
    ioUringTxDatagramsSubmittedTotal: telemetry?.ioUringTxDatagramsSubmittedTotal ?? 0,
    ioUringTxDatagramsCompletedTotal: telemetry?.ioUringTxDatagramsCompletedTotal ?? 0,
    ioUringSqFullEvents: telemetry?.ioUringSqFullEvents ?? 0,
    rawQuicFinObservations: telemetry?.rawQuicFinObservations ?? 0,
    rawQuicFinishedEventEmits: telemetry?.rawQuicFinishedEventEmits ?? 0,
    rawQuicClientPendingWriteHighWatermark: telemetry?.rawQuicClientPendingWriteHighWatermark ?? 0,
    rawQuicClientReapsWithKnownStreams: telemetry?.rawQuicClientReapsWithKnownStreams ?? 0,
    rawQuicClientCloseByTimeout: telemetry?.rawQuicClientCloseByTimeout ?? 0,
    rawQuicClientCloseByRelease: telemetry?.rawQuicClientCloseByRelease ?? 0,
    kqueueUnsentHighWatermark: telemetry?.kqueueUnsentHighWatermark ?? 0,
    kqueueWouldBlockSends: telemetry?.kqueueWouldBlockSends ?? 0,
    kqueueWriteWakeups: telemetry?.kqueueWriteWakeups ?? 0,
    ...extra,
  };
}

function summarizeSinkKinds(sinks) {
  const kinds = Array.from(
    new Set(
      (sinks ?? [])
        .map(sink => sink?.stats?.sink_kind ?? null)
        .filter(Boolean),
    ),
  );
  return kinds.length > 0 ? kinds.join(',') : 'n/a';
}

function normalizeMockSinkKind(payload) {
  if (payload?.sinkMode === 'counting') {
    return 'counting';
  }
  if (payload?.sinkMode === 'tsfn' && payload?.callbackMode === 'noop') {
    return 'tsfn-noop-js';
  }
  if (payload?.sinkMode === 'tsfn' && payload?.callbackMode === 'touch-data') {
    return 'tsfn-real-js';
  }
  return payload?.summary?.sink_type ?? 'unknown';
}

function flattenSummary(summary, sourcePath, context = {}) {
  const common = {
    sourcePath,
    generatedAt: summary.generatedAt ?? null,
    protocol: summary.protocol ?? context.protocol ?? 'unknown',
    profile: summary.settings?.profileName ?? 'unknown',
    harnessKind: context.harnessKind ?? 'benchmark',
    sinkKind: context.sinkKind ?? 'n/a',
    transportKind: context.transportKind ?? 'real-udp',
    label: summary.settings?.label ?? summary.environment?.label ?? context.label ?? null,
    platform: summary.environment?.host?.platform ?? 'unknown',
    arch: summary.environment?.host?.arch ?? 'unknown',
    target: context.target ?? summary.target ?? 'host',
    dockerLane: context.dockerLane ?? null,
    dockerPolicy: classifyDockerPolicy(context.dockerLane ?? null),
    wallElapsedMs: summary.wallElapsedMs ?? 0,
    throughputMbps: summary.throughputMbps ?? 0,
  };

  const clientRuntime = summarizeClientRuntimeSelections(summary.clientStats?.runtimeSelections);
  const clientTelemetry = summary.clientStats?.reactorTelemetry ?? {};
  const records = [{
    ...common,
    role: 'client',
    requestedMode: clientRuntime.requestedMode,
    selectedMode: clientRuntime.selectedMode,
    driver: clientRuntime.driver,
    fallbackOccurred: clientRuntime.fallbackOccurred,
    topology: deriveClientTopology(clientTelemetry),
    latencyP95Ms: maxOf(summary.clientStats?.streamP95s ?? []),
    latencyP99Ms: maxOf(summary.clientStats?.streamP99s ?? []),
    cpuUtilizationPct: summary.clientStats?.totalCpuUtilizationPct ?? 0,
    driverSetupAttempts: clientTelemetry.driverSetupAttemptsTotal ?? 0,
    sharedWorkerReuses: (clientTelemetry.rawQuicClientSharedWorkerReuses ?? 0) + (clientTelemetry.h3ClientSharedWorkerReuses ?? 0),
    localPortReuseHits: clientTelemetry.clientLocalPortReuseHits ?? 0,
    backendSignals: buildBackendSignals(clientTelemetry),
  }];

  if (summary.serverStats) {
    const serverTelemetry = summary.serverStats.reactorTelemetry ?? {};
    records.push({
      ...common,
      role: 'server',
      requestedMode: summary.serverStats.runtimeInfo?.requestedMode ?? 'unknown',
      selectedMode: summary.serverStats.runtimeInfo?.selectedMode ?? 'unknown',
      driver: summary.serverStats.runtimeInfo?.driver ?? 'unknown',
      fallbackOccurred: summary.serverStats.runtimeInfo?.fallbackOccurred ?? null,
      topology: deriveServerTopology(serverTelemetry),
      latencyP95Ms: null,
      latencyP99Ms: null,
      cpuUtilizationPct: summary.serverStats.cpuUtilizationPct ?? 0,
      driverSetupAttempts: serverTelemetry.driverSetupAttemptsTotal ?? 0,
      sharedWorkerReuses: 0,
      localPortReuseHits: 0,
      sessionsObserved: summary.serverStats.final?.sessionCount ?? 0,
      sessionsClosed: summary.serverStats.final?.sessionsClosed ?? null,
      activeSessions: summary.serverStats.final?.activeSessions ?? null,
      maxSessions: summary.serverStats.maxSessions ?? 0,
      maxStreams: summary.serverStats.maxStreams ?? 0,
      backendSignals: buildBackendSignals(serverTelemetry),
    });
  }

  return records;
}

function flattenHarnessArtifact(payload, sourcePath) {
  const harness = payload.environment?.harness ?? payload.environment?.extra?.harness ?? payload.harness ?? null;
  const common = {
    sourcePath,
    generatedAt: payload.environment?.collectedAt ?? null,
    protocol: payload.environment?.protocol ?? 'quic',
    profile: 'harness',
    label: payload.environment?.label ?? null,
    platform: payload.environment?.host?.platform ?? 'unknown',
    arch: payload.environment?.host?.arch ?? 'unknown',
    dockerLane: null,
    dockerPolicy: 'host',
  };

  if (harness === 'quic-loopback' && payload.client && payload.server) {
    const clientTelemetry = payload.client.runtime_telemetry ?? {};
    const serverTelemetry = payload.server.runtime_telemetry ?? {};
    return [
      {
        ...common,
        role: 'client',
        harnessKind: 'real-loopback',
        sinkKind: summarizeSinkKinds(payload.client.sinks),
        transportKind: 'real-udp',
        target: 'rust-loopback',
        wallElapsedMs: payload.client.elapsed_ms ?? 0,
        throughputMbps: payload.client.throughput_mbps ?? 0,
        requestedMode: payload.client.runtime_mode ?? 'unknown',
        selectedMode: payload.client.runtime_mode ?? 'unknown',
        driver: inferDriverFromTelemetry(clientTelemetry),
        fallbackOccurred: false,
        topology: deriveClientTopology(clientTelemetry),
        latencyP95Ms: payload.client.latency_ms?.p95_ms ?? null,
        latencyP99Ms: payload.client.latency_ms?.p99_ms ?? null,
        cpuUtilizationPct: 0,
        driverSetupAttempts: clientTelemetry.driverSetupAttemptsTotal ?? 0,
        sharedWorkerReuses: (clientTelemetry.rawQuicClientSharedWorkerReuses ?? 0) + (clientTelemetry.h3ClientSharedWorkerReuses ?? 0),
        localPortReuseHits: clientTelemetry.clientLocalPortReuseHits ?? 0,
        backendSignals: buildBackendSignals(clientTelemetry),
      },
      {
        ...common,
        role: 'server',
        harnessKind: 'real-loopback',
        sinkKind: summarizeSinkKinds([payload.server.sink]),
        transportKind: 'real-udp',
        target: 'rust-loopback',
        wallElapsedMs: payload.server.elapsed_ms ?? 0,
        throughputMbps: payload.client.throughput_mbps ?? 0,
        requestedMode: payload.server.runtime_mode ?? 'unknown',
        selectedMode: payload.server.runtime_mode ?? 'unknown',
        driver: inferDriverFromTelemetry(serverTelemetry),
        fallbackOccurred: false,
        topology: deriveServerTopology(serverTelemetry),
        latencyP95Ms: null,
        latencyP99Ms: null,
        cpuUtilizationPct: 0,
        driverSetupAttempts: serverTelemetry.driverSetupAttemptsTotal ?? 0,
        sharedWorkerReuses: 0,
        localPortReuseHits: 0,
        sessionsObserved: payload.server.sessions_opened ?? null,
        sessionsClosed: payload.server.sessions_closed ?? null,
        activeSessions: null,
        maxSessions: null,
        maxStreams: payload.server.echoed_streams ?? null,
        backendSignals: buildBackendSignals(serverTelemetry),
      },
    ];
  }

  if (harness === 'quiche-direct-floor' && payload.result) {
    return [{
      ...common,
      role: 'pair',
      harnessKind: 'quiche-direct',
      sinkKind: 'none',
      transportKind: 'in-process',
      target: 'quiche-direct-floor',
      wallElapsedMs: payload.result.elapsed_ms ?? 0,
      throughputMbps: payload.result.throughput_mbps ?? 0,
      requestedMode: 'direct',
      selectedMode: 'direct',
      driver: 'quiche-direct',
      fallbackOccurred: false,
      topology: 'inline',
      latencyP95Ms: payload.result.latency_ms?.p95_ms ?? null,
      latencyP99Ms: payload.result.latency_ms?.p99_ms ?? null,
      cpuUtilizationPct: 0,
      driverSetupAttempts: 0,
      sharedWorkerReuses: 0,
      localPortReuseHits: 0,
      backendSignals: buildBackendSignals({}, {
        completedStreams: payload.result.completed_streams ?? 0,
      }),
    }];
  }

  if ((payload.harness ?? harness) === 'quic-mock' && payload.summary) {
    return [{
      ...common,
      role: 'pair',
      harnessKind: 'mock-transport',
      sinkKind: normalizeMockSinkKind(payload),
      transportKind: 'mock',
      target: 'node-mock-transport',
      wallElapsedMs: payload.summary.elapsed_ms ?? 0,
      throughputMbps: payload.summary.throughput_mbps ?? 0,
      requestedMode: 'mock',
      selectedMode: 'mock',
      driver: payload.summary.transport_driver ?? 'mock',
      fallbackOccurred: false,
      topology: 'mock-pair',
      latencyP95Ms: payload.summary.latency_ms?.p95_ms ?? null,
      latencyP99Ms: payload.summary.latency_ms?.p99_ms ?? null,
      cpuUtilizationPct: 0,
      driverSetupAttempts: payload.summary.runtime_telemetry?.driverSetupAttemptsTotal ?? 0,
      sharedWorkerReuses: 0,
      localPortReuseHits: 0,
      backendSignals: buildBackendSignals(payload.summary.runtime_telemetry ?? {}, {
        jsCallbackBatches: payload.jsCallback?.batches ?? 0,
        jsCallbackEvents: payload.jsCallback?.events ?? 0,
        jsCallbackBytesTouched: payload.jsCallback?.bytesTouched ?? 0,
      }),
    }];
  }

  return [];
}

function collectRecords(resultsDir) {
  const records = [];
  const sources = [];

  for (const path of walkJsonFiles(resultsDir)) {
    const payload = readJsonSafe(path);
    if (!payload || typeof payload !== 'object') {
      continue;
    }

    if (payload.artifactType === 'benchmark-summary') {
      sources.push({ path, artifactType: payload.artifactType });
      records.push(...flattenSummary(payload, path));
    } else if (payload.artifactType === 'docker-benchmark-matrix') {
      sources.push({ path, artifactType: payload.artifactType });
      for (const result of payload.results ?? []) {
        if (result.type !== 'success' || !result.summary) {
          continue;
        }
        records.push(...flattenSummary(result.summary, path, {
          target: 'docker',
          dockerLane: result.name,
          protocol: payload.protocol ?? null,
          label: payload.environment?.label ?? null,
        }));
      }
    } else if (
      payload.environment?.harness === 'quic-loopback' ||
      payload.environment?.harness === 'quiche-direct-floor' ||
      payload.environment?.extra?.harness === 'quic-loopback' ||
      payload.environment?.extra?.harness === 'quiche-direct-floor' ||
      payload.harness === 'quic-mock'
    ) {
      sources.push({
        path,
        artifactType: payload.harness ?? payload.environment?.extra?.harness ?? 'harness-profile',
      });
      records.push(...flattenHarnessArtifact(payload, path));
    }
  }

  return { records, sources };
}

function groupRecords(records) {
  const groups = new Map();

  for (const record of records) {
    const key = [
      record.protocol,
      record.role,
      record.profile,
      record.harnessKind,
      record.sinkKind,
      record.transportKind,
      record.platform,
      record.target,
      record.dockerPolicy,
      record.selectedMode,
      record.driver,
      record.topology,
    ].join('|');
    const group = groups.get(key) ?? [];
    group.push(record);
    groups.set(key, group);
  }

  return Array.from(groups.entries())
    .map(([key, recordsForKey]) => {
      const [
        protocol,
        role,
        profile,
        harnessKind,
        sinkKind,
        transportKind,
        platform,
        target,
        dockerPolicy,
        selectedMode,
        driver,
        topology,
      ] = key.split('|');
      return {
        key,
        protocol,
        role,
        profile,
        harnessKind,
        sinkKind,
        transportKind,
        platform,
        target,
        dockerPolicy,
        selectedMode,
        driver,
        topology,
        count: recordsForKey.length,
        throughputMbps: rangeText(recordsForKey.map((record) => record.throughputMbps), 1, ' Mbps'),
        latencyP95Ms: rangeText(recordsForKey.map((record) => record.latencyP95Ms).filter((value) => value !== null), 2, 'ms'),
        cpuUtilizationPct: rangeText(recordsForKey.map((record) => record.cpuUtilizationPct), 1, '%'),
        driverSetupAttempts: rangeText(recordsForKey.map((record) => record.driverSetupAttempts), 0, ''),
        sharedWorkerReuses: rangeText(recordsForKey.map((record) => record.sharedWorkerReuses), 0, ''),
        localPortReuseHits: rangeText(recordsForKey.map((record) => record.localPortReuseHits), 0, ''),
        sessionsClosed: rangeText(recordsForKey.map((record) => record.sessionsClosed).filter((value) => value !== null), 0, ''),
        activeSessions: rangeText(recordsForKey.map((record) => record.activeSessions).filter((value) => value !== null), 0, ''),
        backendSignals: {
          ioUringPendingTxHighWatermark: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringPendingTxHighWatermark), 0, ''),
          ioUringRetryableSendCompletions: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringRetryableSendCompletions), 0, ''),
          ioUringSubmitCalls: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringSubmitCalls), 0, ''),
          ioUringSubmittedSqesTotal: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringSubmittedSqesTotal), 0, ''),
          ioUringCompletionTotal: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringCompletionTotal), 0, ''),
          ioUringCompletionBatchHighWatermark: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringCompletionBatchHighWatermark), 0, ''),
          ioUringWakeCompletions: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringWakeCompletions), 0, ''),
          ioUringWakeWrites: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringWakeWrites), 0, ''),
          ioUringSqFullEvents: rangeText(recordsForKey.map((record) => record.backendSignals.ioUringSqFullEvents), 0, ''),
          rawQuicFinObservations: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicFinObservations), 0, ''),
          rawQuicFinishedEventEmits: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicFinishedEventEmits), 0, ''),
          rawQuicClientPendingWriteHighWatermark: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicClientPendingWriteHighWatermark), 0, ''),
          rawQuicClientReapsWithKnownStreams: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicClientReapsWithKnownStreams), 0, ''),
          rawQuicClientCloseByTimeout: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicClientCloseByTimeout), 0, ''),
          rawQuicClientCloseByRelease: rangeText(recordsForKey.map((record) => record.backendSignals.rawQuicClientCloseByRelease), 0, ''),
          kqueueUnsentHighWatermark: rangeText(recordsForKey.map((record) => record.backendSignals.kqueueUnsentHighWatermark), 0, ''),
          kqueueWouldBlockSends: rangeText(recordsForKey.map((record) => record.backendSignals.kqueueWouldBlockSends), 0, ''),
          kqueueWriteWakeups: rangeText(recordsForKey.map((record) => record.backendSignals.kqueueWriteWakeups), 0, ''),
          jsCallbackBatches: rangeText(recordsForKey.map((record) => record.backendSignals.jsCallbackBatches ?? 0), 0, ''),
          jsCallbackEvents: rangeText(recordsForKey.map((record) => record.backendSignals.jsCallbackEvents ?? 0), 0, ''),
          jsCallbackBytesTouched: rangeText(recordsForKey.map((record) => record.backendSignals.jsCallbackBytesTouched ?? 0), 0, ''),
        },
      };
    })
    .sort((left, right) => left.key.localeCompare(right.key));
}

function printGroups(groups) {
  if (groups.length === 0) {
    console.log('No persisted benchmark summaries were found.');
    return;
  }

  console.log('Comparable performance groups');
  for (const group of groups) {
    console.log(
      `- ${group.protocol} ${group.role} on ${group.platform} ` +
      `[profile=${group.profile}, harness=${group.harnessKind}, sink=${group.sinkKind}, transport=${group.transportKind}, target=${group.target}, docker=${group.dockerPolicy}, mode=${group.selectedMode}, driver=${group.driver}, topology=${group.topology}]`,
    );
    console.log(`  samples=${group.count}`);
    console.log(`  throughput=${group.throughputMbps}, p95=${group.latencyP95Ms}, cpu=${group.cpuUtilizationPct}`);
    console.log(`  driverSetups=${group.driverSetupAttempts}, sharedReuses=${group.sharedWorkerReuses}, portReuse=${group.localPortReuseHits}`);
    if (group.role === 'server') {
      console.log(`  serverCloseState: closed=${group.sessionsClosed}, active=${group.activeSessions}`);
    }
    console.log(
      `  backend: io_uring pending=${group.backendSignals.ioUringPendingTxHighWatermark},` +
      ` io_uring retries=${group.backendSignals.ioUringRetryableSendCompletions},` +
      ` io_uring submitCalls=${group.backendSignals.ioUringSubmitCalls},` +
      ` io_uring sqes=${group.backendSignals.ioUringSubmittedSqesTotal},` +
      ` io_uring completions=${group.backendSignals.ioUringCompletionTotal},` +
      ` io_uring cqBatchHw=${group.backendSignals.ioUringCompletionBatchHighWatermark},` +
      ` io_uring wakeRx=${group.backendSignals.ioUringWakeCompletions},` +
      ` io_uring wakeTx=${group.backendSignals.ioUringWakeWrites},` +
      ` io_uring sqFull=${group.backendSignals.ioUringSqFullEvents},` +
      ` raw fin=${group.backendSignals.rawQuicFinObservations},` +
      ` raw finished=${group.backendSignals.rawQuicFinishedEventEmits},` +
      ` raw pendingHw=${group.backendSignals.rawQuicClientPendingWriteHighWatermark},` +
      ` raw reapKnown=${group.backendSignals.rawQuicClientReapsWithKnownStreams},` +
      ` raw timeoutClose=${group.backendSignals.rawQuicClientCloseByTimeout},` +
      ` raw releaseClose=${group.backendSignals.rawQuicClientCloseByRelease},` +
      ` kqueue backlog=${group.backendSignals.kqueueUnsentHighWatermark},` +
      ` kqueue wouldBlock=${group.backendSignals.kqueueWouldBlockSends},` +
      ` kqueue writeWakeups=${group.backendSignals.kqueueWriteWakeups},` +
      ` js batches=${group.backendSignals.jsCallbackBatches},` +
      ` js events=${group.backendSignals.jsCallbackEvents},` +
      ` js bytes=${group.backendSignals.jsCallbackBytesTouched}`,
    );
  }
}

function main() {
  const { options, flags } = parseArgs(process.argv.slice(2));
  if (flags.has('--help')) {
    printHelp();
    return;
  }

  const resultsDir = resolve(ROOT_DIR, options.get('--results-dir') ?? 'perf-results');
  const { records, sources } = collectRecords(resultsDir);
  const groups = groupRecords(records);

  if (flags.has('--json')) {
    process.stdout.write(`${JSON.stringify({
      resultsDir,
      sourceCount: sources.length,
      recordCount: records.length,
      groups,
      records,
    })}\n`);
    return;
  }

  console.log(`Artifacts scanned: ${sources.length}`);
  console.log(`Role records: ${records.length}`);
  printGroups(groups);
}

main();
