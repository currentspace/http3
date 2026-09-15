import { spawn } from 'node:child_process';

// --test-timeout bounds individual tests, but leaked sockets can keep the
// test process alive even after a failed test. Bound the whole process too.
const timeoutMs = Number(process.env.HTTP3_NODE_TEST_TIMEOUT_MS ?? 300_000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
  throw new Error('HTTP3_NODE_TEST_TIMEOUT_MS must be a positive integer');
}
const files = process.argv.slice(2);
const child = spawn(process.execPath, [
  '--test', '--test-reporter=tap', '--test-isolation=none', '--test-timeout=15000',
  ...(files.length ? files : [
    'dist-test/test/core/**/*.test.js',
    'dist-test/test/runtime/**/*.test.js',
    'dist-test/test/interop/**/*.test.js',
    'dist-test/test/release/**/*.test.js',
  ]),
], { stdio: 'inherit' });
let timedOut = false;
let killTimer;
const timer = setTimeout(() => {
  timedOut = true;
  console.error(`Node test process exceeded ${timeoutMs}ms; terminating leaked or stalled work.`);
  child.kill('SIGTERM');
  killTimer = setTimeout(() => { child.kill('SIGKILL'); }, 1000);
  killTimer.unref();
}, timeoutMs);
timer.unref();
child.on('error', (error) => {
  console.error(error);
  clearTimeout(timer);
  process.exitCode = 1;
});
child.on('exit', (code) => {
  clearTimeout(timer);
  clearTimeout(killTimer);
  process.exitCode = timedOut ? 124 : (code ?? 1);
});
