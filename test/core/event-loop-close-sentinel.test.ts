/**
 * Regression: WorkerEventLoop / ClientEventLoop / QuicWorkerEventLoop /
 * QuicClientEventLoop `close()` must await the SHUTDOWN_COMPLETE sentinel
 * before calling joinWorker — otherwise late TSFN events can land on
 * already-cleared internal maps.
 *
 * Audit finding #33.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { WorkerEventLoop } from '../../lib/event-loop.js';

interface FakeBinding {
  requestShutdown: () => boolean;
  joinWorker: () => void;
  // Other ServerEventLoopLike methods are unused by close()'s sentinel
  // logic; they're not stubbed.
  [k: string]: unknown;
}

function fakeBinding(): FakeBinding {
  return {
    requestShutdown: () => true,
    joinWorker: () => undefined,
  };
}

describe('WorkerEventLoop.close awaits shutdown sentinel', () => {
  it('resolves only after _onShutdownSentinel fires', async () => {
    const loop = new WorkerEventLoop(fakeBinding() as never);

    let resolved = false;
    const closeP = loop.close().then(() => { resolved = true; });

    // Yield so any synchronous resolution would have settled.
    await new Promise<void>((r) => setImmediate(r));
    assert.equal(resolved, false, 'close should be pending until sentinel');

    loop._onShutdownSentinel();
    await closeP;
    assert.equal(resolved, true, 'close should resolve after sentinel');
  });

  it('resolves immediately if sentinel fired before close()', async () => {
    const loop = new WorkerEventLoop(fakeBinding() as never);
    loop._onShutdownSentinel();

    const start = Date.now();
    await loop.close();
    const elapsed = Date.now() - start;

    assert.ok(elapsed < 100, `close should be near-immediate, took ${String(elapsed)}ms`);
  });

  it('is idempotent', async () => {
    const loop = new WorkerEventLoop(fakeBinding() as never);
    loop._onShutdownSentinel();
    await loop.close();
    await loop.close(); // must not throw, must not hang
  });
});
