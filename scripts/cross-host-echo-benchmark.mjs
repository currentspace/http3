#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const DEFAULTS = {
  remoteHost: 'remote-host.local',
  remoteRoot: '/tmp/nodejs_http3-crossbench',
  localHostForRemote: null,
  durationMs: 5000,
  warmupMs: 1000,
  connections: 8,
  inflight: 8,
  messageSize: 64 * 1024,
  timeoutMs: 30000,
  runtimeMode: 'fast',
  fallbackPolicy: 'warn-and-fallback',
};

const HTTP_SERVER = String.raw`
const http = require('node:http');
const cfg = JSON.parse(process.argv[1]);
let requests = 0;
let bytesIn = 0;
let bytesOut = 0;
const server = http.createServer((req, res) => {
  const chunks = [];
  let len = 0;
  req.on('data', (chunk) => {
    chunks.push(chunk);
    len += chunk.length;
  });
  req.on('end', () => {
    requests += 1;
    bytesIn += len;
    const body = chunks.length === 1 ? chunks[0] : Buffer.concat(chunks, len);
    bytesOut += body.length;
    res.writeHead(200, {
      'content-length': body.length,
      'connection': 'keep-alive',
    });
    res.end(body);
  });
});
function emit(type) {
  const mem = process.memoryUsage();
  const cpu = process.cpuUsage();
  process.stdout.write(JSON.stringify({
    type, timestamp: Date.now(), requests, bytesIn, bytesOut,
    rss: mem.rss, heapUsed: mem.heapUsed, cpuUser: cpu.user, cpuSystem: cpu.system,
  }) + '\n');
}
server.listen(cfg.port || 0, cfg.host || '0.0.0.0', () => {
  const addr = server.address();
  process.stdout.write(JSON.stringify({ type: 'ready', port: addr.port, address: addr.address }) + '\n');
});
const timer = setInterval(() => emit('stats'), cfg.statsIntervalMs || 1000);
timer.unref();
process.on('SIGTERM', () => {
  clearInterval(timer);
  server.close(() => {
    emit('summary');
    process.exit(0);
  });
});
process.stdin.resume();
`;

const HTTP_CLIENT = String.raw`
const http = require('node:http');
const { performance } = require('node:perf_hooks');
const cfg = JSON.parse(process.argv[1]);
const payload = Buffer.alloc(cfg.messageSize, 0xcc);
const totalWorkers = cfg.connections * cfg.maxInflightPerConnection;
const agent = new http.Agent({ keepAlive: true, maxSockets: totalWorkers });
const latencies = [];
let measuredStreams = 0;
let measuredBytes = 0;
let warmupStreams = 0;
let warmupBytes = 0;
let cooldownStreams = 0;
let cooldownBytes = 0;
let errors = 0;
function phaseAt(ms) {
  if (ms < cfg.warmupMs) return 'warmup';
  if (ms < cfg.warmupMs + cfg.durationMs) return 'measured';
  return 'cooldown';
}
function record(phase, bytes, ms) {
  if (phase === 'warmup') { warmupStreams += 1; warmupBytes += bytes; return; }
  if (phase === 'cooldown') { cooldownStreams += 1; cooldownBytes += bytes; return; }
  measuredStreams += 1;
  measuredBytes += bytes;
  latencies.push(ms);
}
function percentile(values, p) {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(Math.floor(sorted.length * p / 100), sorted.length - 1)];
}
function once() {
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: cfg.host,
      port: cfg.port,
      method: 'POST',
      path: '/echo',
      agent,
      headers: { 'content-length': payload.length },
      timeout: cfg.timeoutMs,
    }, (res) => {
      let got = 0;
      res.on('data', (chunk) => { got += chunk.length; });
      res.on('end', () => {
        if (got !== payload.length) reject(new Error('response length mismatch'));
        else resolve(got);
      });
    });
    req.on('timeout', () => req.destroy(new Error('request timed out')));
    req.on('error', reject);
    req.end(payload);
  });
}
async function worker(start) {
  const stopAt = cfg.warmupMs + cfg.durationMs;
  for (;;) {
    const opStart = performance.now();
    const elapsedAtStart = opStart - start;
    if (elapsedAtStart >= stopAt) return;
    const phase = phaseAt(elapsedAtStart);
    try {
      await once();
      record(phase, payload.length * 2, performance.now() - opStart);
    } catch {
      errors += 1;
    }
  }
}
(async () => {
  const cpuStart = process.cpuUsage();
  const start = performance.now();
  await Promise.all(Array.from({ length: totalWorkers }, () => worker(start)));
  agent.destroy();
  const measuredMs = Math.min(Math.max(performance.now() - start - cfg.warmupMs, 0), cfg.durationMs);
  const cpu = process.cpuUsage(cpuStart);
  const result = {
    type: 'result',
    protocol: 'http',
    totalStreams: measuredStreams,
    totalBytes: measuredBytes,
    errors,
    throughputMbps: measuredMs > 0 ? Number(((measuredBytes * 8) / (measuredMs / 1000) / 1e6).toFixed(1)) : 0,
    streamsPerSecond: measuredMs > 0 ? Number((measuredStreams / (measuredMs / 1000)).toFixed(0)) : 0,
    streamLatency: {
      count: latencies.length,
      meanMs: latencies.length ? Number((latencies.reduce((a, b) => a + b, 0) / latencies.length).toFixed(2)) : 0,
      p50Ms: Number(percentile(latencies, 50).toFixed(2)),
      p95Ms: Number(percentile(latencies, 95).toFixed(2)),
      p99Ms: Number(percentile(latencies, 99).toFixed(2)),
    },
    measurement: {
      mode: 'steady-state',
      warmupMs: cfg.warmupMs,
      targetDurationMs: cfg.durationMs,
      measuredMs: Math.round(measuredMs),
      maxInflightPerConnection: cfg.maxInflightPerConnection,
      warmupStreams,
      warmupBytes,
      cooldownStreams,
      cooldownBytes,
      overallStreams: measuredStreams + warmupStreams + cooldownStreams,
      overallBytes: measuredBytes + warmupBytes + cooldownBytes,
    },
    cpu: {
      userMs: Math.round(cpu.user / 1000),
      systemMs: Math.round(cpu.system / 1000),
    },
  };
  process.stdout.write(JSON.stringify(result) + '\n');
})();
`;

const TCP_SERVER = String.raw`
const net = require('node:net');
const cfg = JSON.parse(process.argv[1]);
let connections = 0;
let bytesIn = 0;
let bytesOut = 0;
const server = net.createServer((socket) => {
  connections += 1;
  socket.on('data', (chunk) => {
    bytesIn += chunk.length;
    bytesOut += chunk.length;
    socket.write(chunk);
  });
});
function emit(type) {
  const mem = process.memoryUsage();
  const cpu = process.cpuUsage();
  process.stdout.write(JSON.stringify({
    type, timestamp: Date.now(), connections, bytesIn, bytesOut,
    rss: mem.rss, heapUsed: mem.heapUsed, cpuUser: cpu.user, cpuSystem: cpu.system,
  }) + '\n');
}
server.listen(cfg.port || 0, cfg.host || '0.0.0.0', () => {
  const addr = server.address();
  process.stdout.write(JSON.stringify({ type: 'ready', port: addr.port, address: addr.address }) + '\n');
});
const timer = setInterval(() => emit('stats'), cfg.statsIntervalMs || 1000);
timer.unref();
process.on('SIGTERM', () => {
  clearInterval(timer);
  server.close(() => {
    emit('summary');
    process.exit(0);
  });
});
process.stdin.resume();
`;

const TCP_CLIENT = String.raw`
const net = require('node:net');
const { performance } = require('node:perf_hooks');
const cfg = JSON.parse(process.argv[1]);
const payload = Buffer.alloc(cfg.messageSize, 0xcc);
const totalWorkers = cfg.connections * cfg.maxInflightPerConnection;
const latencies = [];
let measuredStreams = 0;
let measuredBytes = 0;
let warmupStreams = 0;
let warmupBytes = 0;
let cooldownStreams = 0;
let cooldownBytes = 0;
let errors = 0;
function phaseAt(ms) {
  if (ms < cfg.warmupMs) return 'warmup';
  if (ms < cfg.warmupMs + cfg.durationMs) return 'measured';
  return 'cooldown';
}
function record(phase, bytes, ms) {
  if (phase === 'warmup') { warmupStreams += 1; warmupBytes += bytes; return; }
  if (phase === 'cooldown') { cooldownStreams += 1; cooldownBytes += bytes; return; }
  measuredStreams += 1;
  measuredBytes += bytes;
  latencies.push(ms);
}
function percentile(values, p) {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(Math.floor(sorted.length * p / 100), sorted.length - 1)];
}
function connectSocket() {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host: cfg.host, port: cfg.port });
    socket.setNoDelay(true);
    socket.once('connect', () => resolve(socket));
    socket.once('error', reject);
  });
}
function echoOnce(socket) {
  return new Promise((resolve, reject) => {
    let got = 0;
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error('tcp echo timed out'));
    }, cfg.timeoutMs);
    const cleanup = () => {
      clearTimeout(timer);
      socket.off('data', onData);
      socket.off('error', onError);
    };
    const onError = (err) => {
      cleanup();
      reject(err);
    };
    const onData = (chunk) => {
      got += chunk.length;
      if (got >= payload.length) {
        cleanup();
        if (got !== payload.length) reject(new Error('tcp response length mismatch'));
        else resolve(got);
      }
    };
    socket.on('data', onData);
    socket.once('error', onError);
    socket.write(payload);
  });
}
async function worker(start) {
  let socket;
  try {
    socket = await connectSocket();
    const stopAt = cfg.warmupMs + cfg.durationMs;
    for (;;) {
      const opStart = performance.now();
      const elapsedAtStart = opStart - start;
      if (elapsedAtStart >= stopAt) return;
      const phase = phaseAt(elapsedAtStart);
      try {
        await echoOnce(socket);
        record(phase, payload.length * 2, performance.now() - opStart);
      } catch {
        errors += 1;
        return;
      }
    }
  } catch {
    errors += 1;
  } finally {
    if (socket) socket.destroy();
  }
}
(async () => {
  const cpuStart = process.cpuUsage();
  const start = performance.now();
  await Promise.all(Array.from({ length: totalWorkers }, () => worker(start)));
  const measuredMs = Math.min(Math.max(performance.now() - start - cfg.warmupMs, 0), cfg.durationMs);
  const cpu = process.cpuUsage(cpuStart);
  const result = {
    type: 'result',
    protocol: 'tcp',
    totalStreams: measuredStreams,
    totalBytes: measuredBytes,
    errors,
    throughputMbps: measuredMs > 0 ? Number(((measuredBytes * 8) / (measuredMs / 1000) / 1e6).toFixed(1)) : 0,
    streamsPerSecond: measuredMs > 0 ? Number((measuredStreams / (measuredMs / 1000)).toFixed(0)) : 0,
    streamLatency: {
      count: latencies.length,
      meanMs: latencies.length ? Number((latencies.reduce((a, b) => a + b, 0) / latencies.length).toFixed(2)) : 0,
      p50Ms: Number(percentile(latencies, 50).toFixed(2)),
      p95Ms: Number(percentile(latencies, 95).toFixed(2)),
      p99Ms: Number(percentile(latencies, 99).toFixed(2)),
    },
    measurement: {
      mode: 'steady-state',
      warmupMs: cfg.warmupMs,
      targetDurationMs: cfg.durationMs,
      measuredMs: Math.round(measuredMs),
      maxInflightPerConnection: cfg.maxInflightPerConnection,
      warmupStreams,
      warmupBytes,
      cooldownStreams,
      cooldownBytes,
      overallStreams: measuredStreams + warmupStreams + cooldownStreams,
      overallBytes: measuredBytes + warmupBytes + cooldownBytes,
    },
    cpu: {
      userMs: Math.round(cpu.user / 1000),
      systemMs: Math.round(cpu.system / 1000),
    },
  };
  process.stdout.write(JSON.stringify(result) + '\n');
})();
`;

function parseArgs(argv) {
  const out = { ...DEFAULTS };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg.startsWith('--')) continue;
    const [key, inline] = arg.split(/=(.*)/s, 2);
    const value = inline ?? argv[++i];
    switch (key) {
      case '--remote-host': out.remoteHost = value; break;
      case '--remote-root': out.remoteRoot = value; break;
      case '--local-host-for-remote': out.localHostForRemote = value; break;
      case '--duration-ms': out.durationMs = Number(value); break;
      case '--warmup-ms': out.warmupMs = Number(value); break;
      case '--connections': out.connections = Number(value); break;
      case '--inflight': out.inflight = Number(value); break;
      case '--message-size': out.messageSize = parseByteSize(value); break;
      case '--runtime-mode': out.runtimeMode = value; break;
      case '--fallback-policy': out.fallbackPolicy = value; break;
      case '--timeout-ms': out.timeoutMs = Number(value); break;
      default: throw new Error(`unknown option ${key}`);
    }
  }
  if (!out.localHostForRemote) {
    throw new Error('--local-host-for-remote is required');
  }
  return out;
}

function parseByteSize(value) {
  const match = String(value).match(/^(\d+)([kKmMgG][bB]?)?$/u);
  if (!match) throw new Error(`invalid byte size ${value}`);
  const n = Number(match[1]);
  const suffix = (match[2] ?? '').toLowerCase();
  if (suffix.startsWith('k')) return n * 1024;
  if (suffix.startsWith('m')) return n * 1024 * 1024;
  if (suffix.startsWith('g')) return n * 1024 * 1024 * 1024;
  return n;
}

function shellQuote(value) {
  return `'${String(value).replaceAll("'", "'\\''")}'`;
}

function nodeEvalCommand(script, config) {
  return `node -e ${shellQuote(script)} ${shellQuote(JSON.stringify(config))}`;
}

function remoteCommand(options, command) {
  return ['ssh', options.remoteHost, command];
}

function localNodeScript(scriptPath, config) {
  return {
    cmd: process.execPath,
    args: [scriptPath, JSON.stringify(config)],
    cwd: ROOT,
  };
}

function remoteNodeScript(options, scriptPath, config) {
  const command = `cd ${shellQuote(options.remoteRoot)} && node ${shellQuote(scriptPath)} ${shellQuote(JSON.stringify(config))}`;
  const [cmd, ...args] = remoteCommand(options, command);
  return { cmd, args, cwd: ROOT };
}

function localEval(script, config) {
  return {
    cmd: process.execPath,
    args: ['-e', script, JSON.stringify(config)],
    cwd: ROOT,
  };
}

function remoteEval(options, script, config) {
  const [cmd, ...args] = remoteCommand(options, nodeEvalCommand(script, config));
  return { cmd, args, cwd: ROOT };
}

function spawnJsonProcess(spec) {
  return spawn(spec.cmd, spec.args, {
    cwd: spec.cwd,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function parseJsonLines(stream, onMessage, prefix) {
  let buffer = '';
  stream.on('data', (chunk) => {
    buffer += chunk.toString();
    const lines = buffer.split('\n');
    buffer = lines.pop() ?? '';
    for (const line of lines) {
      if (!line.trim()) continue;
      try {
        onMessage(JSON.parse(line));
      } catch {
        process.stderr.write(`${prefix}: ${line}\n`);
      }
    }
  });
}

async function startServer(label, spec) {
  const child = spawnJsonProcess(spec);
  let latest = null;
  let summary = null;
  let stderr = '';

  child.stderr.on('data', (chunk) => {
    stderr += chunk.toString();
    process.stderr.write(`${label} stderr: ${chunk}`);
  });

  const ready = await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`${label} server startup timed out`));
    }, 15000);

    parseJsonLines(child.stdout, (msg) => {
      if (msg.type === 'stats') latest = msg;
      if (msg.type === 'summary') {
        latest = msg;
        summary = msg;
      }
      if (msg.type === 'ready') {
        clearTimeout(timeout);
        resolve(msg);
      }
    }, `${label} server`);

    child.once('error', (err) => {
      clearTimeout(timeout);
      reject(err);
    });
    child.once('exit', (code, signal) => {
      if (code !== null && code !== 0) {
        clearTimeout(timeout);
        reject(new Error(`${label} server exited before ready: code ${code}, stderr: ${stderr}`));
      } else if (signal) {
        clearTimeout(timeout);
        reject(new Error(`${label} server exited before ready: signal ${signal}`));
      }
    });
  });

  return {
    ready,
    get latest() { return latest; },
    get summary() { return summary; },
    async stop() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill('SIGTERM');
      await Promise.race([
        new Promise((resolve) => child.once('exit', resolve)),
        new Promise((resolve) => setTimeout(resolve, 10000)),
      ]);
      if (child.exitCode === null && child.signalCode === null) {
        child.kill('SIGKILL');
      }
    },
  };
}

async function runClient(label, spec) {
  const child = spawnJsonProcess(spec);
  let result = null;
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk.toString();
    process.stderr.write(`${label} stderr: ${chunk}`);
  });
  parseJsonLines(child.stdout, (msg) => {
    if (msg.type === 'result') result = msg;
  }, `${label} client`);
  await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => {
      if (signal) reject(new Error(`${label} client exited via ${signal}`));
      else if (code !== 0) reject(new Error(`${label} client exited ${code}: ${stderr}`));
      else resolve();
    });
  });
  if (!result) throw new Error(`${label} client did not emit result`);
  return result;
}

function benchmarkScript(protocol, role) {
  const base = `dist-test/test/support/bench/${protocol}-bench-${role}.js`;
  return base;
}

function summarizeResult(protocol, direction, result, server) {
  return {
    protocol,
    direction,
    throughputMbps: result.throughputMbps,
    totalStreams: result.totalStreams,
    errors: result.errors,
    streamsPerSecond: result.streamsPerSecond,
    p50Ms: result.streamLatency?.p50Ms ?? 0,
    p95Ms: result.streamLatency?.p95Ms ?? 0,
    serverSummary: server.summary ?? server.latest ?? null,
    runtimeSelections: result.runtimeSelections ?? null,
  };
}

async function runProtocol(options, protocol, direction) {
  const macToLinux = direction === 'mac-to-linux';
  const serverConfig = {
    host: '0.0.0.0',
    port: 0,
    statsIntervalMs: 1000,
    runtimeMode: options.runtimeMode,
    fallbackPolicy: options.fallbackPolicy,
  };
  const clientConfig = {
    host: macToLinux ? options.remoteHost : options.localHostForRemote,
    port: 0,
    connections: options.connections,
    streamsPerConnection: 1,
    messageSize: options.messageSize,
    timeoutMs: options.timeoutMs,
    warmupMs: options.warmupMs,
    durationMs: options.durationMs,
    maxInflightPerConnection: options.inflight,
    runtimeMode: options.runtimeMode,
    fallbackPolicy: options.fallbackPolicy,
  };

  let serverSpec;
  let clientSpec;
  if (protocol === 'quic' || protocol === 'h3') {
    serverSpec = macToLinux
      ? remoteNodeScript(options, benchmarkScript(protocol, 'server'), serverConfig)
      : localNodeScript(join(ROOT, benchmarkScript(protocol, 'server')), serverConfig);
    clientSpec = (port) => macToLinux
      ? localNodeScript(join(ROOT, benchmarkScript(protocol, 'client')), { ...clientConfig, port })
      : remoteNodeScript(options, benchmarkScript(protocol, 'client'), { ...clientConfig, port });
  } else {
    const serverScript = protocol === 'http' ? HTTP_SERVER : TCP_SERVER;
    const clientScript = protocol === 'http' ? HTTP_CLIENT : TCP_CLIENT;
    serverSpec = macToLinux
      ? remoteEval(options, serverScript, serverConfig)
      : localEval(serverScript, serverConfig);
    clientSpec = (port) => macToLinux
      ? localEval(clientScript, { ...clientConfig, port })
      : remoteEval(options, clientScript, { ...clientConfig, port });
  }

  const label = `${protocol} ${direction}`;
  const server = await startServer(label, serverSpec);
  try {
    const result = await runClient(label, clientSpec(server.ready.port));
    return summarizeResult(protocol, direction, result, server);
  } finally {
    await server.stop();
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const protocols = ['quic', 'h3', 'http', 'tcp'];
  const directions = ['mac-to-linux', 'linux-to-mac'];
  const results = [];
  for (const direction of directions) {
    for (const protocol of protocols) {
      process.stderr.write(`Running ${protocol} ${direction}...\n`);
      results.push(await runProtocol(options, protocol, direction));
    }
  }
  const summary = {
    generatedAt: new Date().toISOString(),
    options: {
      remoteHost: options.remoteHost,
      localHostForRemote: options.localHostForRemote,
      durationMs: options.durationMs,
      warmupMs: options.warmupMs,
      connections: options.connections,
      inflight: options.inflight,
      messageSize: options.messageSize,
      runtimeMode: options.runtimeMode,
    },
    results,
  };
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
}

void main().catch((err) => {
  process.stderr.write(`${err.stack || err}\n`);
  process.exit(1);
});
