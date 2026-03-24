/**
 * FFI boundary stress tests.
 * Exercises the Node.js-to-Rust N-API boundary under load: rapid lifecycle
 * creation/teardown, large buffer transfers, concurrent sessions, and
 * telemetry integrity after stress.
 *
 * All instances use runtimeMode:'portable' to avoid io_uring ring allocation
 * exhaustion under rapid create/destroy cycles.
 */

import { describe, it, after, before } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadBinding,
  createEventCollector,
  generateTestCerts,
  EVENT_HANDSHAKE_COMPLETE,
  EVENT_NEW_SESSION,
  EVENT_NEW_STREAM,
  EVENT_HEADERS,
  EVENT_DATA,
  EVENT_FINISHED,
  EVENT_SHUTDOWN_COMPLETE,
} from '../support/native-test-helpers.js';

const binding = loadBinding();

describe('FFI boundary stress', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  // Force clean exit for lingering ThreadsafeFunction refs.
  after(() => {
    setTimeout(() => process.exit(0), 500).unref();
  });

  // ── Rapid lifecycle: QUIC servers ─────────────────────────────

  it('rapid lifecycle: 10 QUIC servers created and shutdown sequentially', async () => {
    for (let i = 0; i < 10; i++) {
      const events = createEventCollector();
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        events.callback,
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();
      await events.waitForShutdown(5000);
    }
  });

  // ── Rapid lifecycle: H3 servers ───────────────────────────────

  it('rapid lifecycle: 10 H3 servers created and shutdown sequentially', async () => {
    for (let i = 0; i < 10; i++) {
      const events = createEventCollector();
      const server = new binding.NativeWorkerServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        events.callback,
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();
      await events.waitForShutdown(5000);
    }
  });

  // ── Rapid lifecycle: QUIC clients ─────────────────────────────

  it('rapid lifecycle: 10 QUIC clients to same server created and shutdown', async () => {
    const serverEvents = createEventCollector();
    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    for (let i = 0; i < 10; i++) {
      const clientEvents = createEventCollector();
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        clientEvents.callback,
      );
      client.connect(`127.0.0.1:${addr.port}`, 'localhost');

      await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

      try { client.close(0, 'done'); } catch { /* ok */ }
      client.shutdown();
      await clientEvents.waitForShutdown(5000);
    }

    server.shutdown();
    await serverEvents.waitForShutdown(5000);
  });

  // ── Large buffer transfer: H3 ────────────────────────────────

  it('large buffer transfer: 256KB body across H3 boundary', async () => {
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeWorkerServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeWorkerClient(
      { rejectUnauthorized: false, runtimeMode: 'portable' },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    // Client sends request headers (no body yet).
    const streamId = client.sendRequest(
      [
        { name: ':method', value: 'POST' },
        { name: ':path', value: '/large' },
        { name: ':authority', value: 'localhost' },
        { name: ':scheme', value: 'https' },
      ],
      false,
    );

    // Send a 256KB body.
    const largeBody = Buffer.alloc(256 * 1024, 0x42); // filled with 'B'
    client.streamSend(streamId, largeBody, true);

    // Wait for server to see the request.
    await serverEvents.waitForEvent(EVENT_NEW_SESSION, 5000);
    await serverEvents.waitForEvent(EVENT_HEADERS, 5000);

    // Collect all DATA events on the server until we see FIN.
    const collectData = (): Promise<void> => {
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('timeout collecting server data')), 15000);
        const check = (): void => {
          const hasFin = serverEvents.allEvents.some(
            (e: any) => (e.eventType === 4 && e.fin) || e.eventType === 5,
          );
          const hasData = serverEvents.allEvents.some((e: any) => e.eventType === 4 && e.data);
          if (hasFin && hasData) {
            clearTimeout(timeout);
            resolve();
          } else {
            setTimeout(check, 20);
          }
        };
        check();
      });
    };
    await collectData();

    // Collect cleanly from all DATA events.
    const allServerData = Buffer.concat(
      serverEvents.allEvents
        .filter((e: any) => e.eventType === 4 && e.data)
        .map((e: any) => Buffer.from(e.data)),
    );
    assert.ok(
      allServerData.length >= largeBody.length,
      `server should receive at least 256KB, got ${allServerData.length}`,
    );
    assert.ok(
      allServerData.subarray(0, largeBody.length).equals(largeBody),
      'server received data should match the sent buffer',
    );

    // Server responds with a 256KB body.
    const headersEvt = serverEvents.allEvents.find((e: any) => e.eventType === 3);
    const connHandle = headersEvt.connHandle;
    const sStreamId = headersEvt.streamId;

    server.sendResponseHeaders(
      connHandle,
      sStreamId,
      [{ name: ':status', value: '200' }],
      false,
    );
    server.streamSend(connHandle, sStreamId, largeBody, true);

    // Wait for client to receive HEADERS + DATA.
    await clientEvents.waitForEvent(EVENT_HEADERS, 5000);

    // Collect client data until FIN.
    const collectClientData = (): Promise<void> => {
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('timeout collecting client data')), 15000);
        const check = (): void => {
          const hasFin = clientEvents.allEvents.some(
            (e: any) => (e.eventType === 4 && e.fin) || e.eventType === 5,
          );
          const hasData = clientEvents.allEvents.some((e: any) => e.eventType === 4 && e.data);
          if (hasFin && hasData) {
            clearTimeout(timeout);
            resolve();
          } else {
            setTimeout(check, 20);
          }
        };
        check();
      });
    };
    await collectClientData();

    const allClientData = Buffer.concat(
      clientEvents.allEvents
        .filter((e: any) => e.eventType === 4 && e.data)
        .map((e: any) => Buffer.from(e.data)),
    );
    assert.ok(
      allClientData.length >= largeBody.length,
      `client should receive at least 256KB, got ${allClientData.length}`,
    );
    assert.ok(
      allClientData.subarray(0, largeBody.length).equals(largeBody),
      'client received data should match the sent buffer',
    );

    try { client.close(0, 'done'); } catch { /* ok */ }
    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── Large buffer transfer: QUIC ──────────────────────────────

  it('large buffer transfer: 256KB payload across QUIC boundary', async () => {
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      { rejectUnauthorized: false, runtimeMode: 'portable' },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    // Client sends 256KB on stream 0.
    const largePayload = Buffer.alloc(256 * 1024, 0x43); // filled with 'C'
    client.streamSend(0, largePayload, true);

    await serverEvents.waitForEvent(EVENT_NEW_SESSION, 5000);
    await serverEvents.waitForEvent(EVENT_NEW_STREAM, 5000);

    // Collect server data until FIN.
    const collectServerData = (): Promise<void> => {
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('timeout collecting server QUIC data')), 15000);
        const check = (): void => {
          const hasFin = serverEvents.allEvents.some(
            (e: any) =>
              ((e.eventType === 2 || e.eventType === 4) && e.fin) ||
              e.eventType === 5,
          );
          const hasData = serverEvents.allEvents.some(
            (e: any) => (e.eventType === 2 || e.eventType === 4) && e.data,
          );
          if (hasFin && hasData) {
            clearTimeout(timeout);
            resolve();
          } else {
            setTimeout(check, 20);
          }
        };
        check();
      });
    };
    await collectServerData();

    const allServerData = Buffer.concat(
      serverEvents.allEvents
        .filter((e: any) => (e.eventType === 2 || e.eventType === 4) && e.data)
        .map((e: any) => Buffer.from(e.data)),
    );
    assert.ok(
      allServerData.length >= largePayload.length,
      `server should receive at least 256KB, got ${allServerData.length}`,
    );
    assert.ok(
      allServerData.subarray(0, largePayload.length).equals(largePayload),
      'server received QUIC data should match sent buffer',
    );

    // Server echoes back.
    const streamEvt = serverEvents.allEvents.find(
      (e: any) => (e.eventType === 2 || e.eventType === 4) && e.data,
    );
    server.streamSend(streamEvt.connHandle, streamEvt.streamId, largePayload, true);

    // Client collects echo data.
    const collectClientData = (): Promise<void> => {
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('timeout collecting client QUIC data')), 15000);
        const check = (): void => {
          const hasFin = clientEvents.allEvents.some(
            (e: any) => (e.eventType === 4 && e.fin) || e.eventType === 5,
          );
          const hasData = clientEvents.allEvents.some((e: any) => e.eventType === 4 && e.data);
          if (hasFin && hasData) {
            clearTimeout(timeout);
            resolve();
          } else {
            setTimeout(check, 20);
          }
        };
        check();
      });
    };
    await collectClientData();

    const allClientData = Buffer.concat(
      clientEvents.allEvents
        .filter((e: any) => e.eventType === 4 && e.data)
        .map((e: any) => Buffer.from(e.data)),
    );
    assert.ok(
      allClientData.length >= largePayload.length,
      `client should receive at least 256KB, got ${allClientData.length}`,
    );
    assert.ok(
      allClientData.subarray(0, largePayload.length).equals(largePayload),
      'client received QUIC data should match sent buffer',
    );

    try { client.close(0, 'done'); } catch { /* ok */ }
    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── Concurrent sessions: H3 ──────────────────────────────────

  it('concurrent sessions: 5 H3 sessions doing request/response', async () => {
    const serverEvents = createEventCollector();

    const server = new binding.NativeWorkerServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    // Spawn a server-side response handler that auto-responds to HEADERS events.
    const respondToRequests = (): (() => void) => {
      const seen = new Set<string>();
      const check = (): void => {
        for (const evt of serverEvents.allEvents) {
          if (evt.eventType === 3 && evt.headers) {
            const key = `${evt.connHandle}:${evt.streamId}`;
            if (!seen.has(key)) {
              seen.add(key);
              try {
                server.sendResponseHeaders(
                  evt.connHandle,
                  evt.streamId,
                  [{ name: ':status', value: '200' }],
                  false,
                );
                server.streamSend(
                  evt.connHandle,
                  evt.streamId,
                  Buffer.from(`response-${key}`),
                  true,
                );
              } catch {
                // Connection may have closed; ignore.
              }
            }
          }
        }
      };
      const interval = setInterval(check, 20);
      return () => clearInterval(interval);
    };
    const stopResponder = respondToRequests();

    const doSession = async (index: number): Promise<void> => {
      const clientEvents = createEventCollector();
      const client = new binding.NativeWorkerClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        clientEvents.callback,
      );
      client.connect(`127.0.0.1:${addr.port}`, 'localhost');

      await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 10000);

      const streamId = client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: `/session-${index}` },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );
      assert.ok(typeof streamId === 'number', `session ${index}: sendRequest should return a stream ID`);

      // Wait for response headers + data.
      await clientEvents.waitForEvent(EVENT_HEADERS, 10000);
      await clientEvents.waitForEvent(EVENT_DATA, 10000);

      const dataEvents = clientEvents.allEvents.filter(
        (e: any) => e.eventType === 4 && e.data,
      );
      assert.ok(dataEvents.length > 0, `session ${index}: should receive response data`);

      try { client.close(0, 'done'); } catch { /* ok */ }
      client.shutdown();
      await clientEvents.waitForShutdown(5000);
    };

    // Run 5 sessions concurrently.
    await Promise.all(
      Array.from({ length: 5 }, (_, i) => doSession(i)),
    );

    stopResponder();
    server.shutdown();
    await serverEvents.waitForShutdown(5000);
  });

  // ── Concurrent sessions: QUIC ────────────────────────────────

  it('concurrent sessions: 5 QUIC sessions with streams', async () => {
    const serverEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    // Auto-echo server: respond to NEW_STREAM/DATA events.
    const respondToStreams = (): (() => void) => {
      const seen = new Set<string>();
      const check = (): void => {
        for (const evt of serverEvents.allEvents) {
          if ((evt.eventType === 2 || evt.eventType === 4) && evt.data) {
            const key = `${evt.connHandle}:${evt.streamId}`;
            if (!seen.has(key)) {
              seen.add(key);
              try {
                server.streamSend(
                  evt.connHandle,
                  evt.streamId,
                  Buffer.from(evt.data),
                  true,
                );
              } catch {
                // Connection may have closed; ignore.
              }
            }
          }
        }
      };
      const interval = setInterval(check, 20);
      return () => clearInterval(interval);
    };
    const stopResponder = respondToStreams();

    const doSession = async (index: number): Promise<void> => {
      const clientEvents = createEventCollector();
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        clientEvents.callback,
      );
      client.connect(`127.0.0.1:${addr.port}`, 'localhost');

      await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 10000);

      const payload = Buffer.from(`quic-session-${index}`);
      client.streamSend(0, payload, true);

      // Wait for echo.
      await clientEvents.waitForEvent(EVENT_DATA, 10000);

      const dataEvents = clientEvents.allEvents.filter(
        (e: any) => e.eventType === 4 && e.data,
      );
      assert.ok(dataEvents.length > 0, `session ${index}: should receive echoed data`);

      try { client.close(0, 'done'); } catch { /* ok */ }
      client.shutdown();
      await clientEvents.waitForShutdown(5000);
    };

    // Run 5 sessions concurrently.
    await Promise.all(
      Array.from({ length: 5 }, (_, i) => doSession(i)),
    );

    stopResponder();
    server.shutdown();
    await serverEvents.waitForShutdown(5000);
  });

  // ── Telemetry integrity ──────────────────────────────────────

  it('telemetry: zero dropped events after stress', async () => {
    binding.resetRuntimeTelemetry();

    // Run a burst of server create/shutdown cycles.
    for (let i = 0; i < 5; i++) {
      const events = createEventCollector();
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        events.callback,
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();
      await events.waitForShutdown(5000);
    }

    // Also do a few client connections to stress the event pipeline.
    const serverEvents = createEventCollector();
    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    for (let i = 0; i < 3; i++) {
      const clientEvents = createEventCollector();
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        clientEvents.callback,
      );
      client.connect(`127.0.0.1:${addr.port}`, 'localhost');
      await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);
      try { client.close(0, 'done'); } catch { /* ok */ }
      client.shutdown();
      await clientEvents.waitForShutdown(5000);
    }

    server.shutdown();
    await serverEvents.waitForShutdown(5000);

    const telemetry = binding.runtimeTelemetry();
    assert.strictEqual(
      telemetry.eventBatchDroppedEventsTotal,
      0,
      `expected zero dropped events, got ${telemetry.eventBatchDroppedEventsTotal}`,
    );
    assert.ok(
      telemetry.eventBatchDeliveredEventsTotal > 0,
      `expected some delivered events, got ${telemetry.eventBatchDeliveredEventsTotal}`,
    );
  });
});
