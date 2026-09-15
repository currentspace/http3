const { performance } = require('node:perf_hooks');
const binding = require('../../index.js');
const durations = {};
for (const name of ['NativeWorkerClient', 'NativeWorkerServer']) {
  for (const method of ['connect', 'sendRequest', 'getSessionMetrics', 'close', 'shutdown']) {
    const prototype = binding[name]?.prototype;
    const original = prototype?.[method];
    if (typeof original !== 'function') continue;
    prototype[method] = function (...args) {
      const start = performance.now();
      try { return original.apply(this, args); }
      finally {
        const elapsed = performance.now() - start;
        const key = `${name}.${method}`;
        const stats = durations[key] ??= { count: 0, totalMs: 0, maxMs: 0 };
        stats.count++; stats.totalMs += elapsed; stats.maxMs = Math.max(stats.maxMs, elapsed);
        if (elapsed > 100) console.error('SLOW_NATIVE', key, elapsed.toFixed(1));
      }
    };
  }
}
let last = performance.now(), maxGapMs = 0;
setInterval(() => { const now = performance.now(); maxGapMs = Math.max(maxGapMs, now-last); last=now; }, 10).unref();
process.on('exit', () => console.error('DIAGNOSTICS', JSON.stringify({ pid:process.pid, maxGapMs, durations, usage:process.resourceUsage() })));
