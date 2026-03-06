import { spawn } from 'node:child_process';

const maxDurationMs = Number.parseInt(process.env.HTTP3_CONCURRENCY_MAX_MS ?? '10000', 10);
const started = Date.now();

const child = spawn(
  process.execPath,
  ['--test', 'dist-test/test/worker-concurrency.test.js'],
  { stdio: 'inherit' },
);

child.on('exit', (code, signal) => {
  const elapsedMs = Date.now() - started;
  if (signal) {
    console.error(`concurrency gate aborted by signal: ${signal}`);
    process.exit(1);
  }
  if (code !== 0) {
    process.exit(code ?? 1);
  }
  if (elapsedMs > maxDurationMs) {
    console.error(
      `concurrency gate failed: elapsed ${elapsedMs}ms exceeds ${maxDurationMs}ms ` +
      '(set HTTP3_CONCURRENCY_MAX_MS to tune threshold)',
    );
    process.exit(1);
  }
  console.log(`concurrency gate passed in ${elapsedMs}ms (budget ${maxDurationMs}ms)`);
});
