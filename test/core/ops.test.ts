import { describe, it } from 'node:test';
import assert from 'node:assert';
import { createHealthController, startHealthServer } from '../../lib/ops.js';

describe('ops helpers', () => {
  it('health controller toggles ready and shutdown state', () => {
    const health = createHealthController(false);
    let snapshot = health.snapshot();
    assert.strictEqual(snapshot.ready, false);
    assert.strictEqual(snapshot.shuttingDown, false);

    health.setReady(true);
    snapshot = health.snapshot();
    assert.strictEqual(snapshot.ready, true);
    assert.strictEqual(snapshot.shuttingDown, false);

    health.beginShutdown('SIGTERM');
    snapshot = health.snapshot();
    assert.strictEqual(snapshot.ready, false);
    assert.strictEqual(snapshot.shuttingDown, true);
    assert.strictEqual(snapshot.shutdownSignal, 'SIGTERM');
  });

  it('health server exposes /healthz and /readyz', async () => {
    const health = createHealthController(true);
    const server = await startHealthServer(health, { host: '127.0.0.1', port: 0 });
    try {
      const healthRes = await fetch(`http://127.0.0.1:${server.address.port}/healthz`);
      assert.strictEqual(healthRes.status, 200);
      const readyRes = await fetch(`http://127.0.0.1:${server.address.port}/readyz`);
      assert.strictEqual(readyRes.status, 200);

      health.beginShutdown('SIGTERM');
      const readyAfter = await fetch(`http://127.0.0.1:${server.address.port}/readyz`);
      assert.strictEqual(readyAfter.status, 503);
    } finally {
      await server.close();
    }
  });
});
