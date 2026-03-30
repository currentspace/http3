/**
 * FFI boundary tests for top-level module functions exported by the native
 * binding: version(), runtimeTelemetry(), resetRuntimeTelemetry(),
 * setLifecycleTraceEnabled(), resetLifecycleTrace(), lifecycleTraceSnapshot().
 *
 * These tests bypass the TypeScript wrapper layer and exercise the NAPI-RS
 * exports directly.
 */

import { describe, it, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { loadBinding, generateTestCerts } from '../support/native-test-helpers.js';

const binding = loadBinding();

describe('FFI module functions', () => {

  // ---- version() ----

  describe('version()', () => {
    it('returns a semver string', () => {
      const v: string = binding.version();
      assert.ok(typeof v === 'string', 'version() should return a string');
      // Semver: major.minor.patch with optional pre-release / build metadata
      assert.match(v, /^\d+\.\d+\.\d+/, `"${v}" should start with a semver triple`);
    });
  });

  // ---- runtimeTelemetry / resetRuntimeTelemetry ----

  describe('runtimeTelemetry()', () => {
    beforeEach(() => {
      binding.resetRuntimeTelemetry();
    });

    it('returns an object with numeric fields', () => {
      const snap = binding.runtimeTelemetry();
      assert.ok(typeof snap === 'object' && snap !== null, 'should return an object');
      // Spot-check a handful of well-known counters
      for (const key of [
        'driverSetupAttemptsTotal',
        'workerThreadSpawnsTotal',
        'shutdownCompleteEmittedTotal',
        'eventBatchFlushesTotal',
      ]) {
        assert.ok(key in snap, `telemetry should contain "${key}"`);
        assert.strictEqual(typeof snap[key], 'number', `"${key}" should be a number`);
      }
    });

    it('resetRuntimeTelemetry() zeros counters', () => {
      // The reset happened in beforeEach; verify counters are zero.
      const snap = binding.runtimeTelemetry();
      assert.strictEqual(snap.driverSetupAttemptsTotal, 0);
      assert.strictEqual(snap.workerThreadSpawnsTotal, 0);
      assert.strictEqual(snap.shutdownCompleteEmittedTotal, 0);
      assert.strictEqual(snap.eventBatchFlushesTotal, 0);
    });

    it('counters increment after server creation', () => {
      const certs = generateTestCerts();
      const noop = (_err: Error | null, _events: any[]): void => {};

      // Constructing a NativeQuicServer spawns a worker thread.
      const server = new binding.NativeQuicServer(
        { key: certs.key, cert: certs.cert, disableRetry: true },
        noop,
      );
      server.listen(0, '127.0.0.1');

      const snap = binding.runtimeTelemetry();
      assert.ok(
        snap.workerThreadSpawnsTotal > 0 || snap.driverSetupAttemptsTotal > 0,
        'at least one counter should have incremented after server creation',
      );

      // Cleanup
      server.shutdown();
    });
  });

  // ---- lifecycle trace ----

  describe('lifecycle trace', () => {
    beforeEach(() => {
      binding.setLifecycleTraceEnabled(false);
      binding.resetLifecycleTrace();
    });

    it('setLifecycleTraceEnabled(true) enables tracing', () => {
      binding.setLifecycleTraceEnabled(true);
      const snap = binding.lifecycleTraceSnapshot();
      assert.strictEqual(snap.enabled, true, 'trace should be enabled');
    });

    it('setLifecycleTraceEnabled(false) disables tracing', () => {
      binding.setLifecycleTraceEnabled(true);
      binding.setLifecycleTraceEnabled(false);
      const snap = binding.lifecycleTraceSnapshot();
      assert.strictEqual(snap.enabled, false, 'trace should be disabled');
    });

    it('resetLifecycleTrace() clears events', () => {
      binding.setLifecycleTraceEnabled(true);

      // Generate at least one trace event by creating + shutting down a server.
      const certs = generateTestCerts();
      const noop = (_err: Error | null, _events: any[]): void => {};
      const server = new binding.NativeQuicServer(
        { key: certs.key, cert: certs.cert, disableRetry: true },
        noop,
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();

      // Reset and verify events are gone.
      binding.resetLifecycleTrace();
      const snap = binding.lifecycleTraceSnapshot();
      assert.strictEqual(snap.eventCount, 0, 'eventCount should be 0 after reset');
      assert.ok(Array.isArray(snap.events), 'events should be an array');
      assert.strictEqual(snap.events.length, 0, 'events array should be empty after reset');
    });

    it('lifecycleTraceSnapshot() has expected structure', () => {
      const snap = binding.lifecycleTraceSnapshot();
      assert.ok(typeof snap === 'object' && snap !== null, 'should return an object');
      assert.strictEqual(typeof snap.enabled, 'boolean', 'enabled should be boolean');
      assert.strictEqual(typeof snap.capacity, 'number', 'capacity should be number');
      assert.strictEqual(typeof snap.droppedEvents, 'number', 'droppedEvents should be number');
      assert.strictEqual(typeof snap.eventCount, 'number', 'eventCount should be number');
      assert.ok(Array.isArray(snap.events), 'events should be an array');
    });
  });
});
