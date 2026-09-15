const { performance } = require('node:perf_hooks');
const binding = require('../../index.js');
const durations = {}, slow = [];
for (const name of ['NativeWorkerClient', 'NativeWorkerServer']) {
  const prototype = binding[name]?.prototype;
  if (!prototype) continue;
  for (const method of Object.getOwnPropertyNames(prototype)) {
    if (method === 'constructor') continue;
    const original = prototype[method];
    if (typeof original !== 'function') continue;
    prototype[method] = function (...args) {
      const start = performance.now();
      try { return original.apply(this, args); }
      finally {
        const elapsed = performance.now() - start;
        const key = `${name}.${method}`;
        const stats = durations[key] ??= { count: 0, totalMs: 0, maxMs: 0, over1: 0, over5: 0, over20: 0 };
        stats.count++; stats.totalMs += elapsed; stats.maxMs = Math.max(stats.maxMs, elapsed);
        stats.over1 += elapsed > 1; stats.over5 += elapsed > 5; stats.over20 += elapsed > 20;
        if (elapsed > 5) { slow.push({key, start, elapsed}); if(slow.length>100) slow.shift(); }
      }
    };
  }
}
let last = performance.now(), maxGapMs = 0, lastCpu = process.cpuUsage();
const stalls = [];
setInterval(() => {
 const now = performance.now(), cpu = process.cpuUsage(), gap = now-last;
 maxGapMs = Math.max(maxGapMs, gap);
 if (gap > 100) stalls.push({now, gap, cpuMs:(cpu.user+cpu.system-lastCpu.user-lastCpu.system)/1000, slow:slow.filter(s=>s.start>=last)});
 last=now; lastCpu=cpu;
}, 5).unref();
process.on('exit', () => console.error('DIAGNOSTICS', JSON.stringify({pid:process.pid, maxGapMs, durations, stalls, usage:process.resourceUsage()})));
