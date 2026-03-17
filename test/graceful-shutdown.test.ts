/**
 * Tests for installGracefulShutdown() from ops.ts.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { installGracefulShutdown, createHealthController } from '../lib/ops.js';
import { createSecureServer } from '../lib/server.js';
import { generateTestCerts } from './generate-certs.js';

describe('graceful shutdown', () => {
  it('close() should remove signal listeners', () => {
    const certs = generateTestCerts();
    const server = createSecureServer({ key: certs.key, cert: certs.cert });

    const beforeSigterm = process.listenerCount('SIGTERM');
    const beforeSigint = process.listenerCount('SIGINT');

    const handle = installGracefulShutdown(server);

    assert.strictEqual(process.listenerCount('SIGTERM'), beforeSigterm + 1);
    assert.strictEqual(process.listenerCount('SIGINT'), beforeSigint + 1);

    handle.close();

    assert.strictEqual(process.listenerCount('SIGTERM'), beforeSigterm);
    assert.strictEqual(process.listenerCount('SIGINT'), beforeSigint);
  });

  it('timeout should call onError callback', async () => {
    let receivedError: unknown = null;
    const server = {
      close: () => new Promise<void>(() => {}),
    } as unknown as Parameters<typeof installGracefulShutdown>[0];

    const handle = installGracefulShutdown(server, {
      signals: ['SIGUSR2'],
      timeoutMs: 25,
      onError: (err) => { receivedError = err; },
    });

    process.emit('SIGUSR2', 'SIGUSR2');
    await new Promise<void>((resolve) => { setTimeout(resolve, 100); });

    assert.ok(receivedError instanceof Error);
    assert.match((receivedError as Error).message, /graceful shutdown timed out/i);

    handle.close();
  });

  it('onSignal callback should receive signal name', async () => {
    const certs = generateTestCerts();
    const server = createSecureServer({ key: certs.key, cert: certs.cert });
    server.listen(0, '127.0.0.1');

    await new Promise<void>((resolve) => {
      server.on('listening', resolve);
    });

    let receivedSignal: string | null = null;

    const handle = installGracefulShutdown(server, {
      signals: ['SIGUSR2'],
      onSignal: (sig) => { receivedSignal = sig; },
    });

    process.emit('SIGUSR2', 'SIGUSR2');

    // Give async shutdown a tick to run
    await new Promise<void>((resolve) => { setTimeout(resolve, 200); });

    assert.strictEqual(receivedSignal, 'SIGUSR2');

    handle.close();
    // Server should already be closing, but ensure cleanup
    try { await server.close(); } catch { /* may already be closed */ }
  });

  it('health controller should transition on shutdown', async () => {
    const certs = generateTestCerts();
    const server = createSecureServer({ key: certs.key, cert: certs.cert });
    server.listen(0, '127.0.0.1');

    await new Promise<void>((resolve) => {
      server.on('listening', resolve);
    });

    const health = createHealthController(true);
    assert.strictEqual(health.snapshot().ready, true);
    assert.strictEqual(health.snapshot().shuttingDown, false);

    const handle = installGracefulShutdown(server, {
      signals: ['SIGUSR2'],
      health,
    });

    process.emit('SIGUSR2', 'SIGUSR2');
    await new Promise<void>((resolve) => { setTimeout(resolve, 200); });

    assert.strictEqual(health.snapshot().ready, false);
    assert.strictEqual(health.snapshot().shuttingDown, true);
    assert.strictEqual(health.snapshot().shutdownSignal, 'SIGUSR2');

    handle.close();
    try { await server.close(); } catch { /* may already be closed */ }
  });
});
