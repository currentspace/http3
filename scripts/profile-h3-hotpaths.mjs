#!/usr/bin/env node
// Profile the current build. No historical checkout or pass/fail speed threshold.
import { fork, spawn, execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { Session } from 'node:inspector/promises';
import { createHistogram, monitorEventLoopDelay, performance, PerformanceObserver } from 'node:perf_hooks';
import { mkdirSync, readFileSync, writeFileSync, createWriteStream, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as sleep } from 'node:timers/promises';
import { captureEnvironmentMetadata } from './perf-artifacts.mjs';

const self = fileURLToPath(import.meta.url);
const root = resolve(dirname(self), '..');
const require = createRequire(import.meta.url);
const scenarios = {
  'wasm-fanout': { runtime: 'wasm', connections: 64, body: 256 * 1024 },
  'native-fanout': { runtime: 'portable', connections: 64, body: 256 * 1024 },
  'native-bulk': { runtime: 'portable', connections: 8, body: 4 * 1024 * 1024, upload: 1024 * 1024, slowReaders: true },
  'wasm-bulk': { runtime: 'wasm', connections: 8, body: 4 * 1024 * 1024, upload: 1024 * 1024, slowReaders: true },
  'native-upload-fanout': { runtime: 'portable', connections: 64, body: 1024, upload: 256 * 1024 },
  'express-small': { runtime: 'portable', connections: 16, body: 1024, express: true },
  'mixed-sse': { runtime: 'portable', connections: 32, body: 1024, sse: true },
};
const json = (file, value) => writeFileSync(file, JSON.stringify(value, null, 2) + '\n');
const histogram = () => createHistogram({ lowest: 1, highest: 120_000_000, figures: 3 });
const record = (hist, ms) => hist.record(Math.max(1, Math.min(120_000_000, Math.round(ms * 1000))));
const percentiles = hist => ({ count: hist.count, meanMs: hist.count ? hist.mean / 1000 : null,
  p50Ms: hist.count ? hist.percentile(50) / 1000 : null, p95Ms: hist.count ? hist.percentile(95) / 1000 : null,
  p99Ms: hist.count ? hist.percentile(99) / 1000 : null, maxMs: hist.count ? hist.max / 1000 : null });

async function startProfile(dir, role) {
  global.gc?.();
  const inspector = new Session(); inspector.connect();
  await inspector.post('Profiler.enable');
  await inspector.post('Profiler.setSamplingInterval', { interval: 1000 });
  await inspector.post('HeapProfiler.startSampling', { samplingInterval: 32768,
    includeObjectsCollectedByMajorGC: true, includeObjectsCollectedByMinorGC: true });
  await inspector.post('Profiler.start');
  const delay = monitorEventLoopDelay({ resolution: 10 }); delay.enable();
  const memoryStart = process.memoryUsage();
  let peakRss = memoryStart.rss, peakHeap = memoryStart.heapUsed;
  const poll = setInterval(() => { const m = process.memoryUsage(); peakRss = Math.max(peakRss, m.rss); peakHeap = Math.max(peakHeap, m.heapUsed); }, 100);
  let gcMs = 0, gcCount = 0;
  const observer = new PerformanceObserver(list => { for (const entry of list.getEntries()) { gcMs += entry.duration; gcCount++; } });
  observer.observe({ entryTypes: ['gc'] });
  const cpuStart = process.cpuUsage(), eluStart = performance.eventLoopUtilization(), start = performance.now();
  return async () => {
    const wallMs = performance.now() - start, cpu = process.cpuUsage(cpuStart);
    const elu = performance.eventLoopUtilization(eluStart);
    clearInterval(poll); delay.disable(); observer.disconnect();
    const memoryEnd = process.memoryUsage();
    const { profile } = await inspector.post('Profiler.stop');
    const heap = await inspector.post('HeapProfiler.stopSampling');
    inspector.disconnect();
    json(join(dir, `${role}.cpuprofile`), profile);
    json(join(dir, `${role}.heapprofile`), heap.profile);
    return { wallMs, cpu, cpuCores: (cpu.user + cpu.system) / (wallMs * 1000), eventLoopUtilization: elu.utilization,
      eventLoopDelayMs: { p50: delay.percentile(50) / 1e6, p99: delay.percentile(99) / 1e6, max: delay.max / 1e6 },
      gcMs, gcCount, memoryStart, memoryEnd, peakRss, peakHeap };
  };
}

function mailbox(child = process) {
  const queue = [], waiters = [];
  let failed;
  child.on('message', value => { const waiter = waiters.shift(); if (waiter) waiter.resolve(value); else queue.push(value); });
  const fail = error => { failed = error; while (waiters.length) waiters.shift().reject(error); };
  child.on('error', fail);
  if (child !== process) child.on('exit', (code, signal) => fail(new Error(`worker exited (${code}, ${signal})`)));
  return async () => {
    if (queue.length) return queue.shift();
    if (failed) throw failed;
    return new Promise((resolve, reject) => waiters.push({ resolve, reject }));
  };
}

async function serverWorker(config) {
  const next = mailbox();
  const { createSecureServer } = require('../dist-test/lib/server.js');
  const { binding } = require('../dist-test/lib/event-loop.js');
  const { generateTestCerts } = require('../dist-test/test/support/generate-certs.js');
  const readyStart = performance.now(), memoryBeforeListen = process.memoryUsage();
  const server = createSecureServer({ ...generateTestCerts(), runtimeMode: config.runtime,
    initialMaxStreamsBidi: 4096 });
  let requests = 0, bytesReceived = 0;
  const errors = {}, streams = new Set(), sessions = new Set();
  const chunk = Buffer.alloc(64 * 1024, 0x61);
  const onError = error => { const key = `${error.code ?? error.name}: ${error.message}`; errors[key] = (errors[key] ?? 0) + 1; console.error(key); };
  server.on('error', onError);
  server.on('session', session => { sessions.add(session); session.on('error', onError); session.once('close', () => sessions.delete(session)); });
  let expressHandler;
  if (config.express) {
    const app = require('express')();
    app.disable('x-powered-by');
    app.get('/', (_req, res) => { res.cookie('session', 'local-profile'); res.json({ data: 'x'.repeat(config.body) }); });
    expressHandler = require('../dist-test/lib/express-adapter.js').createExpressAdapter(app);
  }
  server.on('stream', (stream, headers, flags) => {
    requests++; streams.add(stream); stream.on('error', onError); stream.once('close', () => streams.delete(stream));
    if (headers[':path'] === '/sse') {
      stream.respond({ ':status': '200', 'content-type': 'text/event-stream' });
      let writable = true;
      stream.on('drain', () => { writable = true; });
      const tick = setInterval(() => { if (writable && !stream.destroyed) writable = stream.write('data: tick\n\n'); }, 100);
      stream.once('close', () => clearInterval(tick));
      return;
    }
    if (expressHandler) { expressHandler(stream, headers, flags); return; }
    const send = async () => {
      stream.respond({ ':status': '200' });
      for (let remaining = config.body; remaining > 0;) {
        const bytes = Math.min(remaining, chunk.length); remaining -= bytes;
        if (!stream.write(chunk.subarray(0, bytes))) await once(stream, 'drain');
      }
      stream.end();
    };
    if (config.upload) {
      stream.on('data', data => { bytesReceived += data.length; });
      stream.once('end', () => { send().catch(error => stream.destroy(error)); });
    } else send().catch(error => stream.destroy(error));
  });
  const listening = once(server, 'listening'); server.listen(0, '127.0.0.1'); await listening;
  console.error('listening', server.address(), server.runtimeInfo);
  process.send({ type: 'ready', port: server.address().port, pid: process.pid });
  const startup = { listenMs: performance.now() - readyStart, memoryBeforeListen, memoryAfterListen: process.memoryUsage() };
  if ((await next()).type !== 'profile') throw new Error('expected profile');
  const stopProfile = await startProfile(config.dir, 'server');
  requests = 0; bytesReceived = 0; binding.resetRuntimeTelemetry();
  process.send({ type: 'profiling' });
  if ((await next()).type !== 'stop') throw new Error('expected stop');
  const profile = await stopProfile();
  global.gc?.();
  json(join(config.dir, 'server.json'), { startup, profile, requests, bytesReceived, errors,
    activeStreams: streams.size, internalTrackedStreams: server._streams.size,
    postGcMemory: process.memoryUsage(), wasmMemoryBytes: server._eventLoop?.core?.exports?.memory?.buffer.byteLength,
    sessions: sessions.size, runtimeInfo: server.runtimeInfo, telemetry: binding.runtimeTelemetry() });
  process.send({ type: 'saved' });
  for (const stream of streams) stream.destroy();
  await server.close();
}

async function clientWorker(config) {
  const next = mailbox();
  const { connectAsync } = require('../dist-test/lib/client.js');
  const { binding } = require('../dist-test/lib/event-loop.js');
  const sessions = [], active = new Set(), errors = {};
  let running = true, measuring = false;
  const allLatency = histogram(), allHeaders = histogram(), sseGaps = histogram();
  let sseBytes = 0;
  const metrics = Array.from({ length: config.connections }, (_, index) => ({ index, requests: 0, bytes: 0,
    firstRequestMs: null, slowReader: Boolean(config.slowReaders && index % 2), latency: histogram(), headers: histogram() }));
  const uploadChunk = Buffer.alloc(64 * 1024, 0x62);
  const origin = `https://127.0.0.1:${config.port}`;
  const setupStart = performance.now();
  for (let i = 0; i < config.connections; i++) {
    const session = await connectAsync(origin, { rejectUnauthorized: false, servername: 'localhost', runtimeMode: 'portable' });
    session.on('error', error => { errors[`session: ${error.message}`] = (errors[`session: ${error.message}`] ?? 0) + 1; });
    sessions.push(session);
    console.error(`connected ${i + 1}/${config.connections}`);
  }
  const connectMs = performance.now() - setupStart;
  if (config.sse) {
    for (const session of sessions) {
      const stream = session.request({ ':path': '/sse' }, { endStream: true });
      active.add(stream); stream.on('error', () => {}); stream.once('close', () => active.delete(stream));
      let last = performance.now();
      stream.on('data', data => { const now = performance.now(); if (measuring) { sseBytes += data.length; record(sseGaps, now - last); } last = now; });
    }
  }
  async function request(session, metric) {
    const included = measuring, start = performance.now();
    const stream = session.request({ ':method': config.upload ? 'POST' : 'GET', ':path': '/' }, { endStream: !config.upload });
    active.add(stream); stream.once('close', () => active.delete(stream));
    let bytes = 0, headerMs = null, status, pauseTimer;
    const result = new Promise((resolve, reject) => {
      const timer = setTimeout(() => stream.destroy(new Error('profile request deadline')), 15000);
      const cleanup = () => { clearTimeout(timer); clearTimeout(pauseTimer); };
      stream.once('response', headers => { status = Number(headers[':status']); headerMs = performance.now() - start; });
      stream.on('data', data => {
        bytes += data.length;
        if (metric.slowReader) { stream.pause(); pauseTimer = setTimeout(() => stream.resume(), 3); }
      });
      stream.once('error', error => { cleanup(); reject(error); });
      stream.once('end', () => { cleanup(); resolve(); });
      stream.once('close', () => { if (!stream.readableEnded) { cleanup(); reject(new Error('closed before response end')); } });
    });
    // Observe rejection while the upload is still waiting for drain.
    result.catch(() => {});
    if (config.upload) {
      for (let remaining = config.upload; remaining > 0;) {
        const bytes = Math.min(remaining, uploadChunk.length); remaining -= bytes;
        if (!stream.write(uploadChunk.subarray(0, bytes))) await once(stream, 'drain');
      }
      stream.end();
    }
    await result;
    if (status !== 200) throw new Error(`response status ${status}, expected 200`);
    if (!config.express && bytes !== config.body) throw new Error(`response length ${bytes}, expected ${config.body}`);
    metric.firstRequestMs ??= performance.now() - start;
    if (included) {
      metric.requests++; metric.bytes += bytes;
      record(metric.latency, performance.now() - start); record(allLatency, performance.now() - start);
      if (headerMs !== null) { record(metric.headers, headerMs); record(allHeaders, headerMs); }
    }
  }
  const loops = sessions.map(async (session, index) => {
    while (running) {
      try { await request(session, metrics[index]); }
      catch (error) { errors[error.message] = (errors[error.message] ?? 0) + 1; running = false; }
    }
  });
  await sleep(config.warmupMs);
  if (!running) throw new Error(`warmup failed: ${JSON.stringify(errors)}`);
  process.send({ type: 'warm', connectMs });
  if ((await next()).type !== 'profile') throw new Error('expected profile');
  const stopProfile = await startProfile(config.dir, 'client');
  binding.resetRuntimeTelemetry(); measuring = true;
  const phaseStart = performance.now();
  process.send({ type: 'profiling' });
  await sleep(config.durationMs);
  const loadMs = performance.now() - phaseStart;
  running = false;
  await Promise.all(loops);
  measuring = false;
  const profile = await stopProfile();
  const perConnection = metrics.map(({ latency, headers, ...rest }) => ({ ...rest, latency: percentiles(latency), headers: percentiles(headers) }));
  const requests = perConnection.reduce((sum, item) => sum + item.requests, 0), bytes = perConnection.reduce((sum, item) => sum + item.bytes, 0);
  json(join(config.dir, 'client.json'), { profile, connectMs, loadMs, drainMs: profile.wallMs - loadMs, requests, bytes,
    requestsPerSecond: requests * 1000 / loadMs, mibPerSecond: bytes / 1024 ** 2 * 1000 / loadMs,
    latency: percentiles(allLatency), headers: percentiles(allHeaders), sse: { bytes: sseBytes, chunkArrivalGaps: percentiles(sseGaps) },
    errors, perConnection, telemetry: binding.runtimeTelemetry() });
  process.send({ type: 'saved' });
  if ((await next()).type !== 'close') throw new Error('expected close');
  for (const stream of active) stream.destroy();
  for (const session of sessions) await session.close();
}

function worker(role, config) {
  const child = fork(self, ['--worker', role, JSON.stringify(config)], { cwd: root,
    execArgv: ['--expose-gc'], stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
  const out = createWriteStream(join(config.dir, `${role}.log`)); child.stdout.pipe(out); child.stderr.pipe(out);
  const next = mailbox(child);
  const exited = new Promise(resolve => child.once('exit', (code, signal) => resolve({ code, signal })));
  return { child, next, exited };
}

function parse(argv) {
  const options = { cases: Object.keys(scenarios), durationMs: 10000, warmupMs: 2000,
    out: join(root, 'perf-results', `hotpaths-${new Date().toISOString().replace(/[:.]/g, '-')}`), sample: false };
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i];
    if (key === '--sample') { options.sample = true; continue; }
    if (key === '--help') { console.log('Usage: node scripts/profile-h3-hotpaths.mjs [--cases '+Object.keys(scenarios).join(',')+'] [--duration-ms 10000] [--warmup-ms 2000] [--out DIR] [--sample]\nProfiles the existing dist-test/native/WASM artifacts. Build current sources first. --sample captures macOS all-thread stacks.'); return null; }
    const value = argv[++i]; if (!value) throw new Error(`missing value for ${key}`);
    if (key === '--cases') options.cases = value.split(',');
    else if (key === '--duration-ms') options.durationMs = Number(value);
    else if (key === '--warmup-ms') options.warmupMs = Number(value);
    else if (key === '--out') options.out = resolve(value);
    else throw new Error(`unknown option ${key}`);
  }
  if (!options.cases.length || options.cases.some(name => !scenarios[name])) throw new Error('invalid cases');
  if (!Number.isInteger(options.durationMs) || options.durationMs < 1000 || !Number.isInteger(options.warmupMs) || options.warmupMs < 0) throw new Error('invalid duration');
  return options;
}

async function main() {
  const options = parse(process.argv.slice(2)); if (!options) return;
  mkdirSync(options.out, { recursive: true });
  if (existsSync(join(options.out, 'manifest.json'))) throw new Error('Output directory already contains a run; choose a new --out directory');
  const sha = file => existsSync(file) ? createHash('sha256').update(readFileSync(file)).digest('hex') : null;
  const sourceDiff = execFileSync('git', ['diff', '--binary', 'HEAD'], { cwd: root, encoding: 'utf8' });
  writeFileSync(join(options.out, 'source.diff'), sourceDiff);
  writeFileSync(join(options.out, 'harness.mjs.txt'), readFileSync(self));
  const metadata = captureEnvironmentMetadata({ runner: 'profile-h3-hotpaths', protocol: 'h3', extra: {
    commit: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(),
    status: execFileSync('git', ['status', '--short'], { cwd: root, encoding: 'utf8' }), options,
    nativeSha256: sha(join(root, `http3.${process.platform}-${process.arch}.node`)),
    wasmSha256: sha(join(root, 'dist/wasm/http3_client.wasm')),
    harnessSha256: sha(self),
    sourceDiffSha256: createHash('sha256').update(sourceDiff).digest('hex'),
    notes: ['Current-build profiling only; no historical baseline.', 'CPU/allocation sampling adds overhead; these are diagnostic load results, not capacity claims.',
      'Heap sampling estimates JS allocations including collected objects; it excludes Rust/C allocations.', 'V8 CPU sampling covers the main isolate; use macOS sample for Rust worker threads.',
      'Clients are native portable; requests use loopback with TLS verification disabled only for generated test certificates.'],
  } });
  json(join(options.out, 'manifest.json'), metadata);
  for (const name of options.cases) {
    const config = { ...scenarios[name], ...options, name, dir: join(options.out, name) };
    mkdirSync(config.dir, { recursive: true }); json(join(config.dir, 'config.json'), config);
    console.log(`Profiling ${name} (${config.connections} connections, ${config.durationMs} ms)`);
    const server = worker('server', config); let client, sampler;
    const timeout = setTimeout(() => { server.child.kill('SIGKILL'); client?.child.kill('SIGKILL'); }, config.durationMs + config.warmupMs + 90000);
    try {
      const ready = await server.next();
      if (ready.type !== 'ready') throw new Error('server did not become ready');
      client = worker('client', { ...config, port: ready.port });
      if ((await client.next()).type !== 'warm') throw new Error('client did not warm up');
      server.child.send({ type: 'profile' }); await server.next();
      if (options.sample && process.platform === 'darwin') {
        sampler = spawn('/usr/bin/sample', [String(server.child.pid), String(Math.max(1, Math.floor(config.durationMs / 1000) - 1)), '1', '-file', join(config.dir, 'server.sample.txt')], { stdio: 'ignore' });
      }
      client.child.send({ type: 'profile' }); await client.next();
      if ((await client.next()).type !== 'saved') throw new Error('client did not save results');
      server.child.send({ type: 'stop' }); await server.next();
      client.child.send({ type: 'close' });
      const exits = await Promise.all([client.exited, server.exited]);
      if (exits.some(exit => exit.code !== 0)) throw new Error(`worker exits: ${JSON.stringify(exits)}`);
      const result = JSON.parse(readFileSync(join(config.dir, 'client.json')));
      console.log(`${name}: ${result.requests} requests, p99 ${result.latency.p99Ms} ms, errors ${JSON.stringify(result.errors)}`);
    } finally {
      clearTimeout(timeout); sampler?.kill();
      if (server.child.exitCode === null) server.child.kill('SIGKILL');
      if (client?.child.exitCode === null) client.child.kill('SIGKILL');
    }
  }
  console.log(`Profiles: ${options.out}`);
}

if (process.argv[2] === '--worker') {
  const role = process.argv[3], config = JSON.parse(process.argv[4]);
  (role === 'server' ? serverWorker(config) : clientWorker(config)).then(() => process.disconnect(), error => { console.error(error); process.exit(1); });
} else main().catch(error => { console.error(error); process.exitCode = 1; });
