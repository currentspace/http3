/**
 * FFI error propagation tests.
 * Verifies that Rust errors surface as proper JS exceptions through the
 * N-API boundary, with expected error messages intact.
 */

import { describe, it, after, before } from 'node:test';
import assert from 'node:assert/strict';
import { generateTestCerts } from '../support/generate-certs.js';

// Load the native binding directly (bypassing TS wrappers).
// eslint-disable-next-line @typescript-eslint/no-require-imports, @typescript-eslint/no-unsafe-assignment
const binding = require('../../../index.js');

const EVENT_SHUTDOWN_COMPLETE = 15;

function waitForShutdown(events: any[], timeoutMs = 5000): Promise<void> {
  return new Promise((resolve, reject) => {
    const start = Date.now();
    const check = (): void => {
      if (events.some((e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE)) {
        resolve();
        return;
      }
      if (Date.now() - start > timeoutMs) {
        resolve(); // resolve anyway to avoid test hangs
        return;
      }
      setTimeout(check, 20);
    };
    check();
  });
}

describe('FFI error propagation', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  // Error-path tests intentionally create native objects whose
  // ThreadsafeFunctions are never consumed (e.g. connect() throws
  // before taking the TSFN). Force a clean exit after all tests.
  after(() => {
    setTimeout(() => process.exit(0), 100).unref();
  });

  // ── Constructor validation errors ──────────────────────────────

  describe('constructor validation errors', () => {
    it('NativeQuicServer with invalid runtimeMode throws', () => {
      assert.throws(
        () => {
          new binding.NativeQuicServer(
            {
              key: certs.key,
              cert: certs.cert,
              runtimeMode: 'garbage',
            },
            (_err: Error | null, _events: any[]) => {},
          );
        },
        (err: Error) => {
          assert.ok(
            err.message.includes('invalid runtimeMode'),
            `expected "invalid runtimeMode" in: ${err.message}`,
          );
          return true;
        },
      );
    });

    it('NativeQuicClient with invalid runtimeMode throws', () => {
      assert.throws(
        () => {
          new binding.NativeQuicClient(
            {
              runtimeMode: 'garbage',
              rejectUnauthorized: false,
            },
            (_err: Error | null, _events: any[]) => {},
          );
        },
        (err: Error) => {
          assert.ok(
            err.message.includes('invalid runtimeMode'),
            `expected "invalid runtimeMode" in: ${err.message}`,
          );
          return true;
        },
      );
    });

    it('NativeWorkerServer with invalid key throws', () => {
      assert.throws(
        () => {
          new binding.NativeWorkerServer(
            {
              key: Buffer.from('not-a-key'),
              cert: certs.cert,
            },
            (_err: Error | null, _events: any[]) => {},
          );
        },
        (err: Error) => {
          // The Rust side returns a quiche TLS config error
          assert.ok(
            err.message.length > 0,
            `expected non-empty error message, got: "${err.message}"`,
          );
          return true;
        },
      );
    });

    it('NativeWorkerClient with invalid runtimeMode throws', () => {
      assert.throws(
        () => {
          new binding.NativeWorkerClient(
            {
              runtimeMode: 'garbage',
              rejectUnauthorized: false,
            },
            (_err: Error | null, _events: any[]) => {},
          );
        },
        (err: Error) => {
          assert.ok(
            err.message.includes('invalid runtimeMode'),
            `expected "invalid runtimeMode" in: ${err.message}`,
          );
          return true;
        },
      );
    });
  });

  // ── State errors ───────────────────────────────────────────────

  describe('state errors', () => {
    it('NativeQuicServer.listen() twice throws', async () => {
      const events: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );

      const addr = server.listen(0, '127.0.0.1');
      assert.ok(addr.port > 0, 'first listen should succeed');

      assert.throws(
        () => { server.listen(0, '127.0.0.1'); },
        (err: Error) => {
          assert.ok(
            err.message.includes('already listening'),
            `expected "already listening" in: ${err.message}`,
          );
          return true;
        },
      );

      server.shutdown();
      await waitForShutdown(events);
    });

    it('NativeQuicClient.connect() twice throws', async () => {
      // Need a server to connect to first.
      const serverEvents: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
        },
        (_err: Error | null, batch: any[]) => { serverEvents.push(...batch); },
      );
      const sAddr = server.listen(0, '127.0.0.1');

      const clientEvents: any[] = [];
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false },
        (_err: Error | null, batch: any[]) => { clientEvents.push(...batch); },
      );

      const addr = client.connect(`127.0.0.1:${sAddr.port}`, 'localhost');
      assert.ok(addr.port > 0, 'first connect should succeed');

      assert.throws(
        () => { client.connect(`127.0.0.1:${sAddr.port}`, 'localhost'); },
        (err: Error) => {
          assert.ok(
            err.message.includes('already connected'),
            `expected "already connected" in: ${err.message}`,
          );
          return true;
        },
      );

      client.shutdown();
      await waitForShutdown(clientEvents);
      server.shutdown();
      await waitForShutdown(serverEvents);
    });
  });

  // ── Post-shutdown errors ───────────────────────────────────────

  describe('post-shutdown errors', () => {
    it('NativeQuicClient methods throw after shutdown', async () => {
      const serverEvents: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
        },
        (_err: Error | null, batch: any[]) => { serverEvents.push(...batch); },
      );
      const sAddr = server.listen(0, '127.0.0.1');

      const clientEvents: any[] = [];
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false },
        (_err: Error | null, batch: any[]) => { clientEvents.push(...batch); },
      );
      client.connect(`127.0.0.1:${sAddr.port}`, 'localhost');

      client.shutdown();
      await waitForShutdown(clientEvents);

      // After shutdown, the handle is consumed. Methods that require
      // the handle should throw "quic client not running".
      assert.throws(
        () => { client.getSessionMetrics(); },
        (err: Error) => {
          assert.ok(
            err.message.includes('quic client not running'),
            `expected "quic client not running" in: ${err.message}`,
          );
          return true;
        },
      );

      assert.throws(
        () => { client.ping(); },
        (err: Error) => {
          assert.ok(
            err.message.includes('quic client not running'),
            `expected "quic client not running" in: ${err.message}`,
          );
          return true;
        },
      );

      server.shutdown();
      await waitForShutdown(serverEvents);
    });
  });

  // ── Address parsing errors ─────────────────────────────────────

  describe('address parsing errors', () => {
    it('NativeQuicClient.connect() with invalid address throws', async () => {
      const events: any[] = [];
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );

      assert.throws(
        () => { client.connect('not-a-valid-address', 'localhost'); },
        (err: Error) => {
          // Rust returns std::net::AddrParseError message
          assert.ok(
            err.message.length > 0,
            `expected non-empty error for invalid address, got: "${err.message}"`,
          );
          return true;
        },
      );

      // Client was never connected, shutdown is a no-op but safe to call.
      client.shutdown();
    });

    it('NativeQuicServer.listen() with invalid address throws', () => {
      const events: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );

      assert.throws(
        () => { server.listen(0, 'not-a-valid-host!!!'); },
        (err: Error) => {
          assert.ok(
            err.message.length > 0,
            `expected non-empty error for invalid host, got: "${err.message}"`,
          );
          return true;
        },
      );

      // Server was never listening; shutdown is safe.
      server.shutdown();
    });
  });
});
