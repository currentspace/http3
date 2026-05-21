import { after, afterEach } from 'node:test';

export function forceNativeTestExit(delayMs: number): void {
  let failed = false;

  afterEach((context) => {
    if ('passed' in context && !context.passed) {
      failed = true;
    }
  });

  after(() => {
    setTimeout(() => {
      process.exit(failed ? 1 : 0);
    }, delayMs).unref();
  });
}
