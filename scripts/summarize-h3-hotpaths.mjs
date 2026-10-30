#!/usr/bin/env node
import { readFileSync, writeFileSync, readdirSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';

const dir = resolve(process.argv[2] ?? '');
if (!process.argv[2]) throw new Error('Usage: node scripts/summarize-h3-hotpaths.mjs PROFILE_DIR [WASM_FUNCTION_MAP]');
const read = file => JSON.parse(readFileSync(file, 'utf8'));
const symbols = new Map();
if (process.argv[3]) for (const line of readFileSync(process.argv[3], 'utf8').split('\n')) {
  const match = /^(\d+):(.*)$/.exec(line); if (match) symbols.set(match[1], match[2]);
}
function frameKey(frame) {
  const match = /wasm-function\[(\d+)\]/.exec(frame.functionName);
  if (match && symbols.has(match[1])) return `${symbols.get(match[1])} [wasm ${match[1]}]`;
  return `${frame.functionName || '(anonymous)'} ${frame.url ?? ''}:${frame.lineNumber + 1}`;
}
function cpu(file) {
  const profile = read(file), nodes = new Map(profile.nodes.map(node => [node.id, node]));
  const parents = new Map(), totals = new Map();
  for (const node of profile.nodes) for (const child of node.children ?? []) parents.set(child, node.id);
  let totalUs = 0, idleUs = 0;
  for (let i = 0; i < profile.samples.length; i++) {
    const weight = profile.timeDeltas[i], id = profile.samples[i]; totalUs += weight;
    if (nodes.get(id).callFrame.functionName === '(idle)') idleUs += weight;
    const seen = new Set();
    for (let current = id; current !== undefined; current = parents.get(current)) {
      const key = frameKey(nodes.get(current).callFrame);
      if (seen.has(key)) continue; seen.add(key);
      const entry = totals.get(key) ?? { frame: key, selfUs: 0, inclusiveUs: 0 };
      if (current === id) entry.selfUs += weight;
      entry.inclusiveUs += weight; totals.set(key, entry);
    }
  }
  const entries = [...totals.values()].map(entry => ({ ...entry,
    selfWallPct: 100 * entry.selfUs / totalUs,
    inclusiveWallPct: 100 * entry.inclusiveUs / totalUs,
    selfNonIdlePct: 100 * entry.selfUs / Math.max(1, totalUs - idleUs) }));
  return { totalUs, idlePct: 100 * idleUs / totalUs,
    self: entries.sort((a, b) => b.selfUs - a.selfUs).slice(0, 35),
    concerning: entries.filter(entry => /preserveFacade|readCredit|ReadCredit|suppress_pending_drains|flush_all_sends|fill_handles|set_deadline|flushSends|pump|Garbage|garbage|clock_time_get|dataView|defaultNowNs|hs_next_send/i.test(entry.frame))
      .sort((a, b) => b.inclusiveUs - a.inclusiveUs) };
}
function heap(file) {
  const totals = new Map(); let sampledBytes = 0;
  function visit(node) {
    sampledBytes += node.selfSize;
    const key = frameKey(node.callFrame);
    totals.set(key, (totals.get(key) ?? 0) + node.selfSize);
    for (const child of node.children ?? []) visit(child);
  }
  visit(read(file).head);
  return { estimatedAllocatedBytes: sampledBytes, top: [...totals].sort((a, b) => b[1] - a[1]).slice(0, 25).map(([frame, bytes]) => ({ frame, bytes, pct: 100 * bytes / sampledBytes })) };
}
const summary = [];
for (const name of readdirSync(dir)) {
  const path = join(dir, name); if (!existsSync(join(path, 'server.json'))) continue;
  const server = read(join(path, 'server.json')), client = read(join(path, 'client.json'));
  const counts = client.perConnection.map(item => item.requests), sum = counts.reduce((a, b) => a + b, 0);
  summary.push({ name, server, client, fairness: { minRequests: Math.min(...counts), maxRequests: Math.max(...counts),
    jainIndex: sum ** 2 / (counts.length * counts.reduce((a, b) => a + b ** 2, 0)),
    firstHalfRequests: counts.slice(0, counts.length / 2).reduce((a, b) => a + b, 0),
    secondHalfRequests: counts.slice(counts.length / 2).reduce((a, b) => a + b, 0) },
    serverCpu: cpu(join(path, 'server.cpuprofile')), clientCpu: cpu(join(path, 'client.cpuprofile')),
    serverHeap: heap(join(path, 'server.heapprofile')), clientHeap: heap(join(path, 'client.heapprofile')) });
}
writeFileSync(join(dir, 'summary.json'), JSON.stringify(summary, null, 2) + '\n');
for (const row of summary) {
  console.log(JSON.stringify({ name: row.name, requests: row.client.requests, rps: Math.round(row.client.requestsPerSecond),
    p99: row.client.latency.p99Ms, serverCpuCores: row.server.profile.cpuCores,
    heapStartMiB: row.server.profile.memoryStart.heapUsed / 1024 ** 2, heapEndMiB: row.server.profile.memoryEnd.heapUsed / 1024 ** 2,
    peakRssMiB: row.server.profile.peakRss / 1024 ** 2, activeStreams: row.server.activeStreams, fairness: row.fairness,
    serverTopCpu: row.serverCpu.self.slice(0, 9), concerning: row.serverCpu.concerning, serverTopAlloc: row.serverHeap.top.slice(0, 6) }));
}
