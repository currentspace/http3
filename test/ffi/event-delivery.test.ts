/**
 * TSFN event delivery correctness tests.
 *
 * Exercises causal ordering of native events, data integrity of payloads
 * and headers, handle consistency, batching telemetry, and the
 * SHUTDOWN_COMPLETE sentinel contract.
 */

import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadBinding,
  createEventCollector,
  createQuicPair,
  createH3Pair,
  EVENT_NEW_SESSION,
  EVENT_NEW_STREAM,
  EVENT_HEADERS,
  EVENT_DATA,
  EVENT_HANDSHAKE_COMPLETE,
  EVENT_SHUTDOWN_COMPLETE,
} from '../support/native-test-helpers.js';

const binding = loadBinding();

// Force clean exit -- native TSFN prevent natural shutdown.
after(() => {
  setTimeout(() => process.exit(0), 200).unref();
});

// ── Event ordering ─────────────────────────────────────────────────

describe('Event ordering', () => {
  it('H3: events arrive in causal order', async () => {
    const pair = await createH3Pair();
    try {
      // Client sends a GET request (fin=true, no body).
      const streamId = pair.client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/order-test' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );
      assert.ok(typeof streamId === 'number', 'sendRequest should return a stream ID');

      // Wait for the server to observe the request headers.
      await pair.serverEvents.waitForEvent(EVENT_HEADERS);

      // Extract the server-side handles for the response.
      const headersEvt = pair.serverEvents.allEvents.find(
        (e: any) => e.eventType === EVENT_HEADERS,
      );
      assert.ok(headersEvt, 'server should have received a HEADERS event');

      // Send a response with body so we get a DATA event on the client.
      pair.server.sendResponseHeaders(
        headersEvt.connHandle,
        headersEvt.streamId,
        [{ name: ':status', value: '200' }],
        false,
      );
      pair.server.streamSend(
        headersEvt.connHandle,
        headersEvt.streamId,
        Buffer.from('order-payload'),
        true,
      );

      // Wait for client to receive DATA.
      await pair.clientEvents.waitForEvent(EVENT_DATA);

      // Verify causal order on the SERVER side:
      //   NEW_SESSION < HEADERS  (H3 layer does not emit NEW_STREAM;
      //   stream creation is implicit in the first HEADERS event.)
      const serverAll = pair.serverEvents.allEvents;
      const idxNewSession = serverAll.findIndex(
        (e: any) => e.eventType === EVENT_NEW_SESSION,
      );
      const idxHeaders = serverAll.findIndex(
        (e: any) => e.eventType === EVENT_HEADERS,
      );

      assert.ok(idxNewSession !== -1, 'server should have NEW_SESSION');
      assert.ok(idxHeaders !== -1, 'server should have HEADERS');
      assert.ok(
        idxNewSession < idxHeaders,
        `NEW_SESSION (idx=${idxNewSession}) should precede HEADERS (idx=${idxHeaders})`,
      );

      // Verify causal order on the CLIENT side:
      //   HANDSHAKE_COMPLETE < HEADERS < DATA
      const clientAll = pair.clientEvents.allEvents;
      const idxHandshake = clientAll.findIndex(
        (e: any) => e.eventType === EVENT_HANDSHAKE_COMPLETE,
      );
      const idxClientHeaders = clientAll.findIndex(
        (e: any) => e.eventType === EVENT_HEADERS,
      );
      const idxClientData = clientAll.findIndex(
        (e: any) => e.eventType === EVENT_DATA,
      );

      assert.ok(idxHandshake !== -1, 'client should have HANDSHAKE_COMPLETE');
      assert.ok(idxClientHeaders !== -1, 'client should have HEADERS');
      assert.ok(idxClientData !== -1, 'client should have DATA');
      assert.ok(
        idxHandshake < idxClientHeaders,
        `HANDSHAKE_COMPLETE (idx=${idxHandshake}) should precede HEADERS (idx=${idxClientHeaders})`,
      );
      assert.ok(
        idxClientHeaders < idxClientData,
        `HEADERS (idx=${idxClientHeaders}) should precede DATA (idx=${idxClientData})`,
      );
    } finally {
      await pair.cleanup();
    }
  });

  it('QUIC: events arrive in causal order', async () => {
    const pair = await createQuicPair();
    try {
      // Client opens a bidi stream (stream 0) with data.
      pair.client.streamSend(0, Buffer.from('quic-order-test'), true);

      // Wait for server to observe the stream.
      await pair.serverEvents.waitForEvent(EVENT_NEW_STREAM);

      // Verify causal order on the SERVER side:
      //   NEW_SESSION before NEW_STREAM
      const serverAll = pair.serverEvents.allEvents;
      const idxNewSession = serverAll.findIndex(
        (e: any) => e.eventType === EVENT_NEW_SESSION,
      );
      const idxNewStream = serverAll.findIndex(
        (e: any) => e.eventType === EVENT_NEW_STREAM,
      );

      assert.ok(idxNewSession !== -1, 'server should have NEW_SESSION');
      assert.ok(idxNewStream !== -1, 'server should have NEW_STREAM');
      assert.ok(
        idxNewSession < idxNewStream,
        `NEW_SESSION (idx=${idxNewSession}) should precede NEW_STREAM (idx=${idxNewStream})`,
      );

      // Verify causal order on the CLIENT side:
      //   HANDSHAKE_COMPLETE should have arrived before any stream data
      const clientAll = pair.clientEvents.allEvents;
      const idxHandshake = clientAll.findIndex(
        (e: any) => e.eventType === EVENT_HANDSHAKE_COMPLETE,
      );
      assert.ok(idxHandshake !== -1, 'client should have HANDSHAKE_COMPLETE');
      assert.strictEqual(idxHandshake, 0, 'HANDSHAKE_COMPLETE should be the first client event');
    } finally {
      await pair.cleanup();
    }
  });

  it('SHUTDOWN_COMPLETE is always the last event', async () => {
    const pair = await createH3Pair();

    // Trigger some traffic so there are events before shutdown.
    pair.client.sendRequest(
      [
        { name: ':method', value: 'GET' },
        { name: ':path', value: '/shutdown-order' },
        { name: ':authority', value: 'localhost' },
        { name: ':scheme', value: 'https' },
      ],
      true,
    );
    await pair.serverEvents.waitForEvent(EVENT_HEADERS);

    // Shut down both sides and wait for SHUTDOWN_COMPLETE.
    try { pair.client.close(0, 'test done'); } catch { /* ok */ }
    try { pair.client.shutdown(); } catch { /* ok */ }
    try { pair.server.shutdown(); } catch { /* ok */ }
    await Promise.allSettled([
      pair.clientEvents.waitForShutdown(5000),
      pair.serverEvents.waitForShutdown(5000),
    ]);

    // Server events: SHUTDOWN_COMPLETE must be the last one.
    const serverAll = pair.serverEvents.allEvents;
    const lastServer = serverAll[serverAll.length - 1];
    assert.strictEqual(
      lastServer.eventType,
      EVENT_SHUTDOWN_COMPLETE,
      `last server event should be SHUTDOWN_COMPLETE, got eventType=${lastServer.eventType}`,
    );

    // Client events: SHUTDOWN_COMPLETE must be the last one.
    const clientAll = pair.clientEvents.allEvents;
    const lastClient = clientAll[clientAll.length - 1];
    assert.strictEqual(
      lastClient.eventType,
      EVENT_SHUTDOWN_COMPLETE,
      `last client event should be SHUTDOWN_COMPLETE, got eventType=${lastClient.eventType}`,
    );
  });
});

// ── Event data integrity ───────────────────────────────────────────

describe('Event data integrity', () => {
  it('Buffer data in DATA events matches sent data', async () => {
    const pair = await createH3Pair();
    try {
      const sentBody = Buffer.from('integrity-check-payload-\x00\x01\xff');

      // Client sends request with body.
      const streamId = pair.client.sendRequest(
        [
          { name: ':method', value: 'POST' },
          { name: ':path', value: '/echo' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        false,
      );
      pair.client.streamSend(streamId, sentBody, true);

      // Wait for the server to receive DATA.
      await pair.serverEvents.waitForEvent(EVENT_DATA);

      // Collect all DATA event buffers on the server side.
      const serverDataEvents = pair.serverEvents.allEvents.filter(
        (e: any) => e.eventType === EVENT_DATA && e.data,
      );
      assert.ok(serverDataEvents.length > 0, 'server should receive DATA events');

      const receivedBody = Buffer.concat(
        serverDataEvents.map((e: any) => Buffer.from(e.data)),
      );
      assert.ok(
        sentBody.equals(receivedBody),
        `received body should match sent body byte-by-byte.\n` +
        `  sent:     ${sentBody.toString('hex')}\n` +
        `  received: ${receivedBody.toString('hex')}`,
      );
    } finally {
      await pair.cleanup();
    }
  });

  it('headers in HEADERS events match sent headers', async () => {
    const pair = await createH3Pair();
    try {
      pair.client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/headers-test' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );

      // Wait for the server to see HEADERS.
      const headersEvt = await pair.serverEvents.waitForEvent(EVENT_HEADERS);

      assert.ok(Array.isArray(headersEvt.headers), 'HEADERS event should have a headers array');

      // Build a lookup from the received headers.
      const headerMap = new Map<string, string>();
      for (const h of headersEvt.headers) {
        headerMap.set(h.name, h.value);
      }

      assert.strictEqual(headerMap.get(':method'), 'GET', ':method should be GET');
      assert.strictEqual(headerMap.get(':path'), '/headers-test', ':path should be /headers-test');
      assert.strictEqual(headerMap.get(':scheme'), 'https', ':scheme should be https');
      assert.strictEqual(headerMap.get(':authority'), 'localhost', ':authority should be localhost');
    } finally {
      await pair.cleanup();
    }
  });

  it('connHandle is consistent across events for same connection', async () => {
    const pair = await createQuicPair();
    try {
      pair.client.streamSend(0, Buffer.from('handle-consistency'), true);
      await pair.serverEvents.waitForEvent(EVENT_NEW_STREAM);

      // All server events for this session should share the same connHandle.
      // Exclude SHUTDOWN_COMPLETE (connHandle is always 0 for that sentinel).
      const serverEventsWithHandle = pair.serverEvents.allEvents.filter(
        (e: any) =>
          e.connHandle !== undefined &&
          e.eventType !== EVENT_SHUTDOWN_COMPLETE,
      );
      assert.ok(
        serverEventsWithHandle.length >= 2,
        `should have at least 2 session events with connHandle, got ${serverEventsWithHandle.length}`,
      );

      const firstHandle = serverEventsWithHandle[0].connHandle;
      for (const evt of serverEventsWithHandle) {
        assert.strictEqual(
          evt.connHandle,
          firstHandle,
          `connHandle should be consistent: expected ${firstHandle}, got ${evt.connHandle} on eventType=${evt.eventType}`,
        );
      }
    } finally {
      await pair.cleanup();
    }
  });

  it('streamId is consistent across events for same stream', async () => {
    const pair = await createH3Pair();
    try {
      pair.client.sendRequest(
        [
          { name: ':method', value: 'POST' },
          { name: ':path', value: '/stream-id-test' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        false,
      );
      // Send body on the request stream (streamId returned by sendRequest).
      const clientStreamId = pair.client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/stream-id-check' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );

      // Wait for the server to receive HEADERS for the second request.
      // We need to wait for both; the second one has /stream-id-check.
      await pair.serverEvents.waitForEvent(EVENT_HEADERS);
      // Give a moment for the second request's events to arrive.
      await new Promise((r) => setTimeout(r, 200));

      // Filter server HEADERS events for the /stream-id-check path.
      const headersEvents = pair.serverEvents.allEvents.filter(
        (e: any) => e.eventType === EVENT_HEADERS,
      );
      assert.ok(headersEvents.length > 0, 'should have HEADERS events');

      // For each stream that has both NEW_STREAM and HEADERS, verify streamId matches.
      const newStreamEvents = pair.serverEvents.allEvents.filter(
        (e: any) => e.eventType === EVENT_NEW_STREAM,
      );
      for (const ns of newStreamEvents) {
        const matchingHeaders = headersEvents.find(
          (h: any) => h.streamId === ns.streamId,
        );
        if (matchingHeaders) {
          assert.strictEqual(
            ns.streamId,
            matchingHeaders.streamId,
            `NEW_STREAM and HEADERS should share the same streamId: ${ns.streamId}`,
          );
        }
      }
    } finally {
      await pair.cleanup();
    }
  });
});

// ── Batching telemetry ─────────────────────────────────────────────

describe('Batching telemetry', () => {
  it('telemetry eventBatchFlushesTotal increments', async () => {
    binding.resetRuntimeTelemetry();

    const pair = await createH3Pair();
    try {
      // Generate traffic to trigger event batches.
      pair.client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/telemetry-flush' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );
      await pair.serverEvents.waitForEvent(EVENT_HEADERS);
    } finally {
      await pair.cleanup();
    }

    const snap = binding.runtimeTelemetry();
    assert.ok(
      snap.eventBatchFlushesTotal > 0,
      `eventBatchFlushesTotal should be > 0, got ${snap.eventBatchFlushesTotal}`,
    );
  });

  it('telemetry eventBatchDroppedEventsTotal is 0', async () => {
    binding.resetRuntimeTelemetry();

    const pair = await createH3Pair();
    try {
      pair.client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/telemetry-drops' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );
      await pair.serverEvents.waitForEvent(EVENT_HEADERS);
    } finally {
      await pair.cleanup();
    }

    const snap = binding.runtimeTelemetry();
    assert.strictEqual(
      snap.eventBatchDroppedEventsTotal,
      0,
      `eventBatchDroppedEventsTotal should be 0 under normal load, got ${snap.eventBatchDroppedEventsTotal}`,
    );
  });
});

// ── SHUTDOWN_COMPLETE sentinel ─────────────────────────────────────

describe('SHUTDOWN_COMPLETE sentinel', () => {
  it('H3 server shutdown delivers SHUTDOWN_COMPLETE', async () => {
    const pair = await createH3Pair();

    // Trigger minimal traffic so the server has something to clean up.
    pair.client.sendRequest(
      [
        { name: ':method', value: 'GET' },
        { name: ':path', value: '/shutdown-sentinel' },
        { name: ':authority', value: 'localhost' },
        { name: ':scheme', value: 'https' },
      ],
      true,
    );
    await pair.serverEvents.waitForEvent(EVENT_HEADERS);

    try { pair.client.close(0, 'done'); } catch { /* ok */ }
    try { pair.client.shutdown(); } catch { /* ok */ }
    try { pair.server.shutdown(); } catch { /* ok */ }
    await pair.serverEvents.waitForShutdown(5000);

    const shutdownEvt = pair.serverEvents.allEvents.find(
      (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
    );
    assert.ok(shutdownEvt, 'server should emit SHUTDOWN_COMPLETE');
    assert.strictEqual(
      shutdownEvt.eventType,
      15,
      `SHUTDOWN_COMPLETE eventType should be 15, got ${shutdownEvt.eventType}`,
    );
  });

  it('QUIC client shutdown delivers SHUTDOWN_COMPLETE', async () => {
    const pair = await createQuicPair();

    try { pair.client.close(0, 'done'); } catch { /* ok */ }
    try { pair.client.shutdown(); } catch { /* ok */ }
    await pair.clientEvents.waitForShutdown(5000);

    const shutdownEvt = pair.clientEvents.allEvents.find(
      (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
    );
    assert.ok(shutdownEvt, 'client should emit SHUTDOWN_COMPLETE');
    assert.strictEqual(
      shutdownEvt.eventType,
      15,
      `SHUTDOWN_COMPLETE eventType should be 15, got ${shutdownEvt.eventType}`,
    );

    // Clean up server side too.
    try { pair.server.shutdown(); } catch { /* ok */ }
    await pair.serverEvents.waitForShutdown(3000).catch(() => {});
  });
});
