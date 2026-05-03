#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const DEFAULTS = {
  remoteHost: 'remote-host.local',
  remoteTargetHost: null,
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
  protocols: ['quic', 'h3', 'http1', 'http2', 'tcp'],
  directions: ['mac-to-linux', 'linux-to-mac'],
  resultsDir: null,
  label: 'cross-host-echo',
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
    protocol: 'http1',
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

const HTTP2_SERVER = String.raw`
const http2 = require('node:http2');
const { constants } = http2;
const cfg = JSON.parse(process.argv[1]);
let streams = 0;
let bytesIn = 0;
let bytesOut = 0;
const server = http2.createServer({
  settings: {
    initialWindowSize: 16 * 1024 * 1024,
    maxConcurrentStreams: 50000,
  },
});
server.on('stream', (stream) => {
  const chunks = [];
  let len = 0;
  stream.on('data', (chunk) => {
    chunks.push(chunk);
    len += chunk.length;
  });
  stream.on('end', () => {
    streams += 1;
    bytesIn += len;
    const body = chunks.length === 1 ? chunks[0] : Buffer.concat(chunks, len);
    bytesOut += body.length;
    stream.respond({
      [constants.HTTP2_HEADER_STATUS]: 200,
      [constants.HTTP2_HEADER_CONTENT_LENGTH]: body.length,
    });
    stream.end(body);
  });
  stream.on('error', () => {});
});
function emit(type) {
  const mem = process.memoryUsage();
  const cpu = process.cpuUsage();
  process.stdout.write(JSON.stringify({
    type, timestamp: Date.now(), streams, bytesIn, bytesOut,
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

const HTTP2_CLIENT = String.raw`
const http2 = require('node:http2');
const { constants } = http2;
const { performance } = require('node:perf_hooks');
const cfg = JSON.parse(process.argv[1]);
const payload = Buffer.alloc(cfg.messageSize, 0xcc);
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
function connectSession() {
  return new Promise((resolve, reject) => {
    const session = http2.connect('http://' + cfg.host + ':' + cfg.port, {
      settings: {
        initialWindowSize: 16 * 1024 * 1024,
        maxConcurrentStreams: 50000,
      },
    });
    const cleanup = () => {
      session.off('connect', onConnect);
      session.off('error', onError);
    };
    const onConnect = () => {
      cleanup();
      resolve(session);
    };
    const onError = (err) => {
      cleanup();
      reject(err);
    };
    session.once('connect', onConnect);
    session.once('error', onError);
  });
}
function closeSession(session) {
  return new Promise((resolve) => {
    if (session.closed || session.destroyed) {
      resolve();
      return;
    }
    const timer = setTimeout(() => {
      session.destroy();
      resolve();
    }, 2000);
    session.close(() => {
      clearTimeout(timer);
      resolve();
    });
  });
}
function once(session) {
  return new Promise((resolve, reject) => {
    const req = session.request({
      [constants.HTTP2_HEADER_METHOD]: 'POST',
      [constants.HTTP2_HEADER_PATH]: '/echo',
      [constants.HTTP2_HEADER_CONTENT_LENGTH]: payload.length,
    });
    let got = 0;
    const timer = setTimeout(() => {
      cleanup();
      req.close(constants.NGHTTP2_CANCEL);
      reject(new Error('http2 request timed out'));
    }, cfg.timeoutMs);
    const cleanup = () => {
      clearTimeout(timer);
      req.off('data', onData);
      req.off('end', onEnd);
      req.off('error', onError);
      req.off('aborted', onAborted);
    };
    const onData = (chunk) => {
      got += chunk.length;
    };
    const onEnd = () => {
      cleanup();
      if (got !== payload.length) reject(new Error('response length mismatch'));
      else resolve(got);
    };
    const onError = (err) => {
      cleanup();
      reject(err);
    };
    const onAborted = () => {
      cleanup();
      reject(new Error('http2 request aborted'));
    };
    req.on('data', onData);
    req.once('end', onEnd);
    req.once('error', onError);
    req.once('aborted', onAborted);
    req.end(payload);
  });
}
async function worker(session, start) {
  const stopAt = cfg.warmupMs + cfg.durationMs;
  for (;;) {
    const opStart = performance.now();
    const elapsedAtStart = opStart - start;
    if (elapsedAtStart >= stopAt) return;
    const phase = phaseAt(elapsedAtStart);
    try {
      await once(session);
      record(phase, payload.length * 2, performance.now() - opStart);
    } catch {
      errors += 1;
    }
  }
}
(async () => {
  const cpuStart = process.cpuUsage();
  const sessions = await Promise.all(Array.from({ length: cfg.connections }, () => connectSession()));
  const start = performance.now();
  await Promise.all(sessions.flatMap((session) =>
    Array.from({ length: cfg.maxInflightPerConnection }, () => worker(session, start))
  ));
  await Promise.all(sessions.map((session) => closeSession(session)));
  const measuredMs = Math.min(Math.max(performance.now() - start - cfg.warmupMs, 0), cfg.durationMs);
  const cpu = process.cpuUsage(cpuStart);
  const result = {
    type: 'result',
    protocol: 'http2',
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
      case '--remote-target-host': out.remoteTargetHost = value; break;
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
      case '--protocols': out.protocols = parseList(value).map(normalizeProtocol); break;
      case '--directions': out.directions = parseList(value).map(normalizeDirection); break;
      case '--results-dir': out.resultsDir = value; break;
      case '--label': out.label = value; break;
      default: throw new Error(`unknown option ${key}`);
    }
  }
  if (out.directions.some((direction) => direction === 'linux-to-mac') && !out.localHostForRemote) {
    throw new Error('--local-host-for-remote is required');
  }
  return out;
}

function parseList(value) {
  return String(value).split(',').map((entry) => entry.trim()).filter(Boolean);
}

function normalizeProtocol(value) {
  switch (value) {
    case 'http':
    case 'http1':
    case 'http1.1':
    case 'http/1.1':
      return 'http1';
    case 'h3':
    case 'http3':
    case 'http/3':
      return 'h3';
    case 'http2':
    case 'h2':
    case 'http/2':
      return 'http2';
    case 'quic':
    case 'tcp':
      return value;
    default:
      throw new Error(`unknown protocol ${value}`);
  }
}

function normalizeDirection(value) {
  switch (value) {
    case 'loopback':
    case 'local-loopback':
      return 'loopback';
    case 'mac-to-linux':
    case 'local-to-remote':
      return 'mac-to-linux';
    case 'linux-to-mac':
    case 'remote-to-local':
      return 'linux-to-mac';
    default:
      throw new Error(`unknown direction ${value}`);
  }
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
      let exitCleanup = () => {};
      let shutdownTimer;
      try {
        await Promise.race([
          new Promise((resolve) => {
            const onExit = () => resolve();
            exitCleanup = () => child.off('exit', onExit);
            child.once('exit', onExit);
          }),
          new Promise((resolve) => {
            shutdownTimer = setTimeout(resolve, 10000);
          }),
        ]);
      } finally {
        exitCleanup();
        if (shutdownTimer) clearTimeout(shutdownTimer);
      }
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

function summarizeResult(protocol, direction, result, server, hosts) {
  return {
    protocol,
    direction,
    ...hosts,
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
  const loopback = direction === 'loopback';
  const macToLinux = direction === 'mac-to-linux';
  const serverIsRemote = macToLinux;
  const clientIsRemote = direction === 'linux-to-mac';
  const clientHost = loopback
    ? '127.0.0.1'
    : (macToLinux ? (options.remoteTargetHost ?? options.remoteHost) : options.localHostForRemote);
  const serverConfig = {
    host: loopback ? '127.0.0.1' : '0.0.0.0',
    port: 0,
    statsIntervalMs: 1000,
    runtimeMode: options.runtimeMode,
    fallbackPolicy: options.fallbackPolicy,
  };
  const clientConfig = {
    host: clientHost,
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
    serverSpec = serverIsRemote
      ? remoteNodeScript(options, benchmarkScript(protocol, 'server'), serverConfig)
      : localNodeScript(join(ROOT, benchmarkScript(protocol, 'server')), serverConfig);
    clientSpec = (port) => clientIsRemote
      ? remoteNodeScript(options, benchmarkScript(protocol, 'client'), { ...clientConfig, port })
      : localNodeScript(join(ROOT, benchmarkScript(protocol, 'client')), { ...clientConfig, port });
  } else {
    let serverScript;
    let clientScript;
    switch (protocol) {
      case 'http1':
        serverScript = HTTP_SERVER;
        clientScript = HTTP_CLIENT;
        break;
      case 'http2':
        serverScript = HTTP2_SERVER;
        clientScript = HTTP2_CLIENT;
        break;
      case 'tcp':
        serverScript = TCP_SERVER;
        clientScript = TCP_CLIENT;
        break;
      default:
        throw new Error(`unknown protocol ${protocol}`);
    }
    serverSpec = serverIsRemote
      ? remoteEval(options, serverScript, serverConfig)
      : localEval(serverScript, serverConfig);
    clientSpec = (port) => clientIsRemote
      ? remoteEval(options, clientScript, { ...clientConfig, port })
      : localEval(clientScript, { ...clientConfig, port });
  }

  const label = `${protocol} ${direction}`;
  const server = await startServer(label, serverSpec);
  try {
    const result = await runClient(label, clientSpec(server.ready.port));
    return summarizeResult(protocol, direction, result, server, {
      serverHost: serverIsRemote ? options.remoteHost : 'local',
      clientHost: clientIsRemote ? options.remoteHost : 'local',
      targetHost: clientHost,
    });
  } finally {
    await server.stop();
  }
}

function formatMarkdown(summary) {
  const rows = [
    '| Direction | Protocol | Throughput Mbps | Streams/s | p50 ms | p95 ms | Errors | Runtime |',
    '| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |',
  ];
  for (const result of summary.results) {
    const runtime = result.runtimeSelections
      ? JSON.stringify(result.runtimeSelections)
      : '';
    rows.push(
      `| ${result.direction} | ${result.protocol} | ${result.throughputMbps} | ${result.streamsPerSecond} | ${result.p50Ms} | ${result.p95Ms} | ${result.errors} | ${runtime} |`,
    );
  }
  return `${rows.join('\n')}\n`;
}

function persistSummary(options, summary) {
  if (!options.resultsDir) return null;
  const dir = resolve(ROOT, options.resultsDir);
  mkdirSync(dir, { recursive: true });
  const stamp = new Date().toISOString().replaceAll(':', '').replace(/\.\d{3}Z$/u, 'Z');
  const base = `${options.label}-${stamp}`;
  const jsonPath = join(dir, `${base}.json`);
  const mdPath = join(dir, `${base}.md`);
  writeFileSync(jsonPath, `${JSON.stringify(summary, null, 2)}\n`);
  writeFileSync(mdPath, formatMarkdown(summary));
  return { jsonPath, mdPath };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const results = [];
  for (const direction of options.directions) {
    for (const protocol of options.protocols) {
      process.stderr.write(`Running ${protocol} ${direction}...\n`);
      results.push(await runProtocol(options, protocol, direction));
    }
  }
  const summary = {
    generatedAt: new Date().toISOString(),
    options: {
      remoteHost: options.remoteHost,
      remoteTargetHost: options.remoteTargetHost,
      localHostForRemote: options.localHostForRemote,
      durationMs: options.durationMs,
      warmupMs: options.warmupMs,
      connections: options.connections,
      inflight: options.inflight,
      messageSize: options.messageSize,
      runtimeMode: options.runtimeMode,
      fallbackPolicy: options.fallbackPolicy,
      protocols: options.protocols,
      directions: options.directions,
    },
    results,
  };
  const artifacts = persistSummary(options, summary);
  if (artifacts) {
    summary.artifacts = artifacts;
    process.stderr.write(`Wrote ${artifacts.jsonPath}\n`);
    process.stderr.write(`Wrote ${artifacts.mdPath}\n`);
  }
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
}

void main().catch((err) => {
  process.stderr.write(`${err.stack || err}\n`);
  process.exit(1);
});
