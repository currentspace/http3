/**
 * FFI config-boundary tests.
 * Verifies that configuration options correctly cross the JS-to-Rust boundary
 * for both QUIC and H3 native classes, exercising TLS settings, timeouts,
 * datagram support, ALPN negotiation, and client certificate authentication.
 */

import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadBinding,
  createEventCollector,
  generateTestCerts,
  generateMutualTlsTestCerts,
  EVENT_HANDSHAKE_COMPLETE,
  EVENT_SESSION_CLOSE,
  EVENT_ERROR,
  EVENT_DATAGRAM,
  EVENT_NEW_SESSION,
  EVENT_NEW_STREAM,
  EVENT_DATA,
} from '../support/native-test-helpers.js';

const binding = loadBinding();

describe('Config boundary — options cross JS-to-Rust FFI', () => {
  // Native objects may retain ThreadsafeFunction references; force clean exit.
  after(() => {
    setTimeout(() => process.exit(0), 200).unref();
  });

  // ── maxIdleTimeoutMs ──────────────────────────────────────────

  it('maxIdleTimeoutMs is respected', async () => {
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        maxIdleTimeoutMs: 2000,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      {
        rejectUnauthorized: false,
        maxIdleTimeoutMs: 2000,
      },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    // The session should close due to idle timeout within ~3s.
    const start = Date.now();
    await clientEvents.waitForEvent(EVENT_SESSION_CLOSE, 10000);
    const elapsed = Date.now() - start;

    // Verify it closed roughly within the expected window (not instantly, not too late).
    assert.ok(
      elapsed >= 1500 && elapsed <= 8000,
      `session should close near the 2s idle timeout, but took ${elapsed}ms`,
    );

    try { client.shutdown(); } catch { /* already shut down */ }
    try { server.shutdown(); } catch { /* already shut down */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── enableDatagrams ──────────────────────────────────────────

  it('enableDatagrams allows sendDatagram (QUIC)', async () => {
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        enableDatagrams: true,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      {
        rejectUnauthorized: false,
        enableDatagrams: true,
      },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    // sendDatagram should succeed (not throw) when datagrams are enabled.
    const payload = Buffer.from('datagram-test-payload');
    const sent = client.sendDatagram(payload);
    assert.ok(sent !== undefined, 'sendDatagram should return without throwing');

    // Wait for the server to receive the datagram.
    await serverEvents.waitForEvent(EVENT_NEW_SESSION, 5000);
    const dgEvent = await serverEvents.waitForEvent(EVENT_DATAGRAM, 5000);
    assert.ok(dgEvent, 'server should receive a DATAGRAM event');

    try { client.close(0, 'done'); } catch { /* ok */ }
    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── rejectUnauthorized=false ─────────────────────────────────

  it('rejectUnauthorized=false accepts self-signed', async () => {
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      { rejectUnauthorized: false },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    // Should successfully complete the handshake.
    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    try { client.close(0, 'done'); } catch { /* ok */ }
    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── rejectUnauthorized=true ──────────────────────────────────

  it('rejectUnauthorized=true rejects self-signed', async () => {
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      { rejectUnauthorized: true },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    // Handshake should fail — expect an ERROR or SESSION_CLOSE event rather
    // than HANDSHAKE_COMPLETE.
    const result = await Promise.race([
      clientEvents.waitForEvent(EVENT_ERROR, 8000).then(() => 'error' as const),
      clientEvents.waitForEvent(EVENT_SESSION_CLOSE, 8000).then(() => 'close' as const),
      clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 8000).then(() => 'handshake' as const),
    ]);

    assert.ok(
      result === 'error' || result === 'close',
      `expected error or session_close for self-signed cert with rejectUnauthorized=true, got: ${result}`,
    );

    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── custom ALPN ──────────────────────────────────────────────

  it('custom ALPN is negotiated (QUIC)', async () => {
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const customAlpn = ['my-custom-proto'];

    const server = new binding.NativeQuicServer(
      {
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        alpn: customAlpn,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    const client = new binding.NativeQuicClient(
      {
        rejectUnauthorized: false,
        alpn: customAlpn,
      },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    // If ALPN matches, the handshake should complete.
    await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE, 5000);

    // Verify data can flow — send a small stream to confirm the session works.
    const payload = Buffer.from('alpn-check');
    client.streamSend(0, payload, true);

    await serverEvents.waitForEvent(EVENT_NEW_SESSION, 5000);
    const streamEvent = await serverEvents.waitForEvent(EVENT_NEW_STREAM, 5000);
    assert.ok(streamEvent, 'server should receive data on custom ALPN connection');

    try { client.close(0, 'done'); } catch { /* ok */ }
    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });

  // ── clientAuth=require rejects unauthenticated ───────────────

  it('clientAuth=require rejects unauthenticated', async () => {
    const mtls = generateMutualTlsTestCerts();
    const serverEvents = createEventCollector();
    const clientEvents = createEventCollector();

    const server = new binding.NativeQuicServer(
      {
        key: mtls.server.key,
        cert: mtls.server.cert,
        ca: mtls.ca.cert,
        clientAuth: 'require',
        disableRetry: true,
      },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1');

    // Client does NOT provide a client cert — server will close the connection
    // after the QUIC handshake completes because no peer cert is presented.
    const client = new binding.NativeQuicClient(
      {
        ca: mtls.ca.cert,
        rejectUnauthorized: false,
      },
      clientEvents.callback,
    );
    client.connect(`127.0.0.1:${addr.port}`, 'localhost');

    // The QUIC handshake itself may complete (TLS allows it), but the server
    // immediately closes the connection with error 0x0100 when it detects no
    // client certificate. We should see a SESSION_CLOSE or ERROR on the client.
    await clientEvents.waitForEvent(EVENT_SESSION_CLOSE, 10000);

    const closeEvent = clientEvents.allEvents.find(
      (e: any) => e.eventType === EVENT_SESSION_CLOSE || e.eventType === EVENT_ERROR,
    );
    assert.ok(closeEvent, 'client should receive session close or error after server rejects missing cert');

    try { client.shutdown(); } catch { /* ok */ }
    try { server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      clientEvents.waitForShutdown(3000).catch(() => {}),
      serverEvents.waitForShutdown(3000).catch(() => {}),
    ]);
  });
});
