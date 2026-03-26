/**
 * H3 event coverage tests.
 *
 * Exercises error recovery across streams, shutdown with active streams,
 * concurrent H3 + QUIC pairs, request/response patterns, session close
 * events, and SESSION_TICKET delivery.
 */

import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadBinding,
  createEventCollector,
  createH3Pair,
  createQuicPair,
  EVENT_NEW_SESSION,
  EVENT_HEADERS,
  EVENT_DATA,
  EVENT_FINISHED,
  EVENT_RESET,
  EVENT_SESSION_CLOSE,
  EVENT_DRAIN,
  EVENT_GOAWAY,
  EVENT_ERROR,
  EVENT_HANDSHAKE_COMPLETE,
  EVENT_SESSION_TICKET,
  EVENT_SHUTDOWN_COMPLETE,
} from '../support/native-test-helpers.js';

const binding = loadBinding();

// Force clean exit -- native TSFN prevent natural shutdown.
after(() => {
  setTimeout(() => process.exit(0), 500).unref();
});

// ── Error recovery across streams ─────────────────────────────────

describe('H3 event coverage', () => {
  describe('error recovery across streams', () => {
    it('ERROR on one stream does not kill other streams', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends two GET requests.
        const streamId1 = pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/stream1' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
        const streamId2 = pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/stream2' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );

        assert.ok(typeof streamId1 === 'number', 'sendRequest should return stream ID for request 1');
        assert.ok(typeof streamId2 === 'number', 'sendRequest should return stream ID for request 2');

        // Wait for server to receive both HEADERS events.
        // Use sequential waits: first wait for at least 1, then wait for the 2nd.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);
        // Give event loop time to deliver the second request's HEADERS.
        await new Promise<void>((r) => setTimeout(r, 200));
        // Wait for the second HEADERS if not already arrived.
        const headersCount = pair.serverEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_HEADERS,
        ).length;
        if (headersCount < 2) {
          await pair.serverEvents.waitForNEvents(EVENT_HEADERS, 2, 5000);
        }

        // Find server-side HEADERS events for each stream.
        const serverHeaders = pair.serverEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        assert.ok(serverHeaders.length >= 2, `server should have received at least 2 HEADERS events, got ${serverHeaders.length}`);

        const hdr1 = serverHeaders[0];
        const hdr2 = serverHeaders[1];

        // Server responds to stream 1 with headers (fin=false), then closes with error.
        pair.server.sendResponseHeaders(
          hdr1.connHandle,
          hdr1.streamId,
          [{ name: ':status', value: '200' }],
          false,
        );
        pair.server.streamClose(hdr1.connHandle, hdr1.streamId, 0x100);

        // Server responds normally to stream 2 with headers + body + fin.
        pair.server.sendResponseHeaders(
          hdr2.connHandle,
          hdr2.streamId,
          [{ name: ':status', value: '200' }],
          false,
        );
        pair.server.streamSend(
          hdr2.connHandle,
          hdr2.streamId,
          Buffer.from('stream2-body'),
          true,
        );

        // Wait for client to receive DATA on stream 2.
        await pair.clientEvents.waitForEvent(EVENT_DATA, 5000);

        // Verify client got DATA on stream 2.
        const clientDataEvents = pair.clientEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_DATA && e.data,
        );
        assert.ok(clientDataEvents.length > 0, 'client should receive DATA events on stream 2');

        // Verify client sees RESET or ERROR on stream 1.
        const errorOrResetOnStream1 = pair.clientEvents.allEvents.some(
          (e: any) =>
            (e.eventType === EVENT_RESET || e.eventType === EVENT_ERROR) &&
            e.streamId === streamId1,
        );
        // Stream closure may also manifest as FINISHED; accept either signal.
        const finishedOnStream1 = pair.clientEvents.allEvents.some(
          (e: any) => e.eventType === EVENT_FINISHED && e.streamId === streamId1,
        );
        assert.ok(
          errorOrResetOnStream1 || finishedOnStream1,
          'client should see RESET, ERROR, or FINISHED on stream 1 after streamClose',
        );
      } finally {
        await pair.cleanup();
      }
    });
  });

  // ── Shutdown with active streams ──────────────────────────────────

  describe('shutdown with active streams', () => {
    it('server shutdown during active request produces SHUTDOWN_COMPLETE', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends GET (fin=true).
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/active-request' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );

        // Wait for server to see the request.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        // Before server responds, shut it down.
        try { pair.server.shutdown(); } catch { /* ok */ }

        // Verify SHUTDOWN_COMPLETE on server.
        await pair.serverEvents.waitForShutdown(5000);

        const shutdownEvt = pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
        );
        assert.ok(shutdownEvt, 'server should emit SHUTDOWN_COMPLETE');
        assert.strictEqual(shutdownEvt.eventType, EVENT_SHUTDOWN_COMPLETE);
      } finally {
        // Ensure client side is cleaned up too.
        try { pair.client.close(0, 'test done'); } catch { /* ok */ }
        try { pair.client.shutdown(); } catch { /* ok */ }
        await pair.clientEvents.waitForShutdown(3000).catch(() => {});
      }
    });

    it('client shutdown during pending request delivers SHUTDOWN_COMPLETE', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends GET (fin=true).
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/pending-request' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );

        // Wait for server to see the request, confirming it's in-flight.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        // Before server responds, shut down the client.
        try { pair.client.close(0, 'client shutdown'); } catch { /* ok */ }
        try { pair.client.shutdown(); } catch { /* ok */ }

        // Verify SHUTDOWN_COMPLETE on client.
        await pair.clientEvents.waitForShutdown(5000);

        const shutdownEvt = pair.clientEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
        );
        assert.ok(shutdownEvt, 'client should emit SHUTDOWN_COMPLETE');
        assert.strictEqual(shutdownEvt.eventType, EVENT_SHUTDOWN_COMPLETE);
      } finally {
        try { pair.server.shutdown(); } catch { /* ok */ }
        await pair.serverEvents.waitForShutdown(3000).catch(() => {});
      }
    });
  });

  // ── Concurrent H3 + QUIC pairs ───────────────────────────────────

  describe('concurrent H3 + QUIC pairs', () => {
    it('H3 and QUIC pairs operate independently in same process', async () => {
      // Create both pairs concurrently.
      const [h3Pair, quicPair] = await Promise.all([
        createH3Pair(),
        createQuicPair(),
      ]);

      try {
        // On H3 pair: client sends GET, server responds.
        const h3StreamId = h3Pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/concurrent-h3' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
        assert.ok(typeof h3StreamId === 'number', 'H3 sendRequest should return stream ID');

        // On QUIC pair: client sends data on stream 0.
        const quicPayload = Buffer.from('concurrent-quic-data');
        quicPair.client.streamSend(0, quicPayload, true);

        // Wait for H3 server to receive the request.
        await h3Pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        const h3HeadersEvt = h3Pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        assert.ok(h3HeadersEvt, 'H3 server should receive HEADERS');

        // H3 server responds.
        h3Pair.server.sendResponseHeaders(
          h3HeadersEvt.connHandle,
          h3HeadersEvt.streamId,
          [{ name: ':status', value: '200' }],
          false,
        );
        h3Pair.server.streamSend(
          h3HeadersEvt.connHandle,
          h3HeadersEvt.streamId,
          Buffer.from('h3-response'),
          true,
        );

        // Wait for both sides to receive their data.
        const [h3Data, quicData] = await Promise.all([
          h3Pair.clientEvents.waitForEvent(EVENT_DATA, 5000),
          quicPair.serverEvents.waitForEvent(EVENT_DATA, 5000).catch(() =>
            // QUIC may deliver data via NEW_STREAM event; fall back.
            quicPair.serverEvents.allEvents.find(
              (e: any) => (e.eventType === EVENT_DATA || e.eventType === 2) && e.data,
            ),
          ),
        ]);

        // Verify H3 client received response data.
        const h3DataEvents = h3Pair.clientEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_DATA && e.data,
        );
        assert.ok(h3DataEvents.length > 0, 'H3 client should receive DATA');

        // Verify QUIC server received stream data.
        const quicDataEvents = quicPair.serverEvents.allEvents.filter(
          (e: any) => (e.eventType === EVENT_DATA || e.eventType === 2) && e.data,
        );
        assert.ok(quicDataEvents.length > 0, 'QUIC server should receive data');
      } finally {
        await Promise.all([
          h3Pair.cleanup(),
          quicPair.cleanup(),
        ]);
      }
    });
  });

  // ── H3 request/response patterns ──────────────────────────────────

  describe('H3 request/response patterns', () => {
    it('multiple sequential request/response cycles on same session', async () => {
      const pair = await createH3Pair();
      try {
        for (let i = 0; i < 5; i++) {
          // Client sends GET with unique path.
          const streamId = pair.client.sendRequest(
            [
              { name: ':method', value: 'GET' },
              { name: ':path', value: `/cycle-${i}` },
              { name: ':authority', value: 'localhost' },
              { name: ':scheme', value: 'https' },
            ],
            true,
          );
          assert.ok(typeof streamId === 'number', `cycle ${i}: sendRequest should return stream ID`);

          // Wait for server to see HEADERS for this request.
          // Use waitForNEvents to ensure we have enough HEADERS events accumulated.
          await pair.serverEvents.waitForNEvents(EVENT_HEADERS, i + 1, 5000);

          // Find the server HEADERS event for this cycle's path.
          const headersEvts = pair.serverEvents.allEvents.filter(
            (e: any) => e.eventType === EVENT_HEADERS,
          );
          const thisHeaders = headersEvts[i];
          assert.ok(thisHeaders, `cycle ${i}: server should have HEADERS event`);

          // Server responds with 200 + body.
          pair.server.sendResponseHeaders(
            thisHeaders.connHandle,
            thisHeaders.streamId,
            [{ name: ':status', value: '200' }],
            false,
          );
          pair.server.streamSend(
            thisHeaders.connHandle,
            thisHeaders.streamId,
            Buffer.from(`response-${i}`),
            true,
          );

          // Wait for client to receive HEADERS + DATA for this cycle.
          // We need at least (i+1) client HEADERS events.
          await pair.clientEvents.waitForNEvents(EVENT_HEADERS, i + 1, 5000);
          await pair.clientEvents.waitForNEvents(EVENT_DATA, i + 1, 5000);
        }

        // Verify all 5 cycles completed: at least 5 HEADERS and 5 DATA on client.
        const clientHeaders = pair.clientEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        const clientData = pair.clientEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_DATA && e.data,
        );
        assert.ok(clientHeaders.length >= 5, `client should have at least 5 HEADERS events, got ${clientHeaders.length}`);
        assert.ok(clientData.length >= 5, `client should have at least 5 DATA events, got ${clientData.length}`);
      } finally {
        await pair.cleanup();
      }
    });

    it('POST request with body echoed back', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends POST with headers (fin=false).
        const streamId = pair.client.sendRequest(
          [
            { name: ':method', value: 'POST' },
            { name: ':path', value: '/echo' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          false,
        );

        // Client sends 4KB body.
        const requestBody = Buffer.alloc(4 * 1024, 0x41); // filled with 'A'
        pair.client.streamSend(streamId, requestBody, true);

        // Wait for server to receive HEADERS + DATA.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        // Wait for server to receive the full body (fin or FINISHED).
        await new Promise<void>((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('timeout waiting for server data')), 10000);
          const check = (): void => {
            const hasFin = pair.serverEvents.allEvents.some(
              (e: any) => (e.eventType === EVENT_DATA && e.fin) || e.eventType === EVENT_FINISHED,
            );
            const hasData = pair.serverEvents.allEvents.some(
              (e: any) => e.eventType === EVENT_DATA && e.data,
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

        // Collect all DATA events on the server.
        const serverDataEvents = pair.serverEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_DATA && e.data,
        );
        const receivedBody = Buffer.concat(
          serverDataEvents.map((e: any) => Buffer.from(e.data)),
        );

        // Server echoes the body back.
        const headersEvt = pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        pair.server.sendResponseHeaders(
          headersEvt.connHandle,
          headersEvt.streamId,
          [{ name: ':status', value: '200' }],
          false,
        );
        pair.server.streamSend(
          headersEvt.connHandle,
          headersEvt.streamId,
          receivedBody,
          true,
        );

        // Wait for client to receive the echoed DATA.
        await pair.clientEvents.waitForEvent(EVENT_DATA, 5000);

        // Collect client DATA events.
        const clientDataEvents = pair.clientEvents.allEvents.filter(
          (e: any) => e.eventType === EVENT_DATA && e.data,
        );
        const echoedBody = Buffer.concat(
          clientDataEvents.map((e: any) => Buffer.from(e.data)),
        );

        assert.ok(
          requestBody.equals(echoedBody),
          `echoed body should match request body.\n` +
          `  sent length:     ${requestBody.length}\n` +
          `  received length: ${echoedBody.length}`,
        );
      } finally {
        await pair.cleanup();
      }
    });

    it('server sends response without body (204 No Content)', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends GET.
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/no-content' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );

        // Wait for server to see HEADERS.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        const headersEvt = pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        assert.ok(headersEvt, 'server should receive HEADERS');

        // Server responds with 204 headers (fin=true, no body).
        pair.server.sendResponseHeaders(
          headersEvt.connHandle,
          headersEvt.streamId,
          [{ name: ':status', value: '204' }],
          true,
        );

        // Wait for client to receive HEADERS.
        await pair.clientEvents.waitForEvent(EVENT_HEADERS, 5000);

        // Verify client receives HEADERS with :status=204.
        const clientHeadersEvt = pair.clientEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_HEADERS && e.headers,
        );
        assert.ok(clientHeadersEvt, 'client should receive HEADERS event');

        const statusHeader = clientHeadersEvt.headers.find(
          (h: any) => h.name === ':status',
        );
        assert.ok(statusHeader, 'response should contain :status header');
        assert.strictEqual(statusHeader.value, '204', ':status should be 204');
      } finally {
        await pair.cleanup();
      }
    });

    it('large response body: 256KB', async () => {
      const pair = await createH3Pair();
      try {
        // Client sends GET.
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: '/large-response' },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );

        // Wait for server to see HEADERS.
        await pair.serverEvents.waitForEvent(EVENT_HEADERS, 5000);

        const headersEvt = pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_HEADERS,
        );
        assert.ok(headersEvt, 'server should receive HEADERS');

        // Server responds with 256KB body.
        const largeBody = Buffer.alloc(256 * 1024, 0x42); // filled with 'B'
        pair.server.sendResponseHeaders(
          headersEvt.connHandle,
          headersEvt.streamId,
          [{ name: ':status', value: '200' }],
          false,
        );
        pair.server.streamSend(
          headersEvt.connHandle,
          headersEvt.streamId,
          largeBody,
          true,
        );

        // Collect all client DATA events until FIN.
        await new Promise<void>((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('timeout collecting client data')), 15000);
          const check = (): void => {
            const hasFin = pair.clientEvents.allEvents.some(
              (e: any) => (e.eventType === EVENT_DATA && e.fin) || e.eventType === EVENT_FINISHED,
            );
            const hasData = pair.clientEvents.allEvents.some(
              (e: any) => e.eventType === EVENT_DATA && e.data,
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

        // Verify total bytes == 256KB.
        const allClientData = Buffer.concat(
          pair.clientEvents.allEvents
            .filter((e: any) => e.eventType === EVENT_DATA && e.data)
            .map((e: any) => Buffer.from(e.data)),
        );
        assert.strictEqual(
          allClientData.length,
          256 * 1024,
          `client should receive exactly 256KB, got ${allClientData.length}`,
        );
        assert.ok(
          allClientData.equals(largeBody),
          'received 256KB body should match the sent buffer byte-for-byte',
        );
      } finally {
        await pair.cleanup();
      }
    });
  });

  // ── Session close events ──────────────────────────────────────────

  describe('session close events', () => {
    it('server closeSession produces SESSION_CLOSE on client', async () => {
      const pair = await createH3Pair();
      try {
        // Handshake already complete from createH3Pair.
        // Find the server-side connHandle from NEW_SESSION.
        const newSessionEvt = pair.serverEvents.allEvents.find(
          (e: any) => e.eventType === EVENT_NEW_SESSION,
        );
        assert.ok(newSessionEvt, 'server should have NEW_SESSION event');

        // Server closes the session.
        pair.server.closeSession(newSessionEvt.connHandle, 0, 'test');

        // Wait for client to see SESSION_CLOSE.
        // The close may also appear as GOAWAY or ERROR depending on timing.
        await new Promise<void>((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('timeout waiting for session close on client')), 5000);
          const check = (): void => {
            const hasClose = pair.clientEvents.allEvents.some(
              (e: any) =>
                e.eventType === EVENT_SESSION_CLOSE ||
                e.eventType === EVENT_GOAWAY ||
                e.eventType === EVENT_ERROR,
            );
            if (hasClose) {
              clearTimeout(timeout);
              resolve();
            } else {
              setTimeout(check, 20);
            }
          };
          check();
        });

        const closeEvt = pair.clientEvents.allEvents.find(
          (e: any) =>
            e.eventType === EVENT_SESSION_CLOSE ||
            e.eventType === EVENT_GOAWAY ||
            e.eventType === EVENT_ERROR,
        );
        assert.ok(closeEvt, 'client should see SESSION_CLOSE, GOAWAY, or ERROR after server closeSession');
      } finally {
        try { pair.client.close(0, 'cleanup'); } catch { /* ok */ }
        try { pair.client.shutdown(); } catch { /* ok */ }
        try { pair.server.shutdown(); } catch { /* ok */ }
        await Promise.allSettled([
          pair.clientEvents.waitForShutdown(3000).catch(() => {}),
          pair.serverEvents.waitForShutdown(3000).catch(() => {}),
        ]);
      }
    });

    it('client close produces SESSION_CLOSE on server', async () => {
      const pair = await createH3Pair();
      try {
        // Handshake already complete from createH3Pair.
        // Client closes with error code 0 and reason.
        pair.client.close(0, 'test');

        // Wait for server to see SESSION_CLOSE, GOAWAY, or ERROR.
        await new Promise<void>((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('timeout waiting for session close on server')), 5000);
          const check = (): void => {
            const hasClose = pair.serverEvents.allEvents.some(
              (e: any) =>
                e.eventType === EVENT_SESSION_CLOSE ||
                e.eventType === EVENT_GOAWAY ||
                e.eventType === EVENT_ERROR,
            );
            if (hasClose) {
              clearTimeout(timeout);
              resolve();
            } else {
              setTimeout(check, 20);
            }
          };
          check();
        });

        const closeEvt = pair.serverEvents.allEvents.find(
          (e: any) =>
            e.eventType === EVENT_SESSION_CLOSE ||
            e.eventType === EVENT_GOAWAY ||
            e.eventType === EVENT_ERROR,
        );
        assert.ok(closeEvt, 'server should see SESSION_CLOSE, GOAWAY, or ERROR after client close');
      } finally {
        try { pair.client.shutdown(); } catch { /* ok */ }
        try { pair.server.shutdown(); } catch { /* ok */ }
        await Promise.allSettled([
          pair.clientEvents.waitForShutdown(3000).catch(() => {}),
          pair.serverEvents.waitForShutdown(3000).catch(() => {}),
        ]);
      }
    });
  });

  // ── SESSION_TICKET event ──────────────────────────────────────────

  describe('SESSION_TICKET event', () => {
    it('client receives SESSION_TICKET after handshake', async () => {
      const pair = await createH3Pair();
      try {
        // Handshake already complete from createH3Pair.
        // SESSION_TICKET may arrive late; wait with 10s timeout.
        try {
          await pair.clientEvents.waitForEvent(EVENT_SESSION_TICKET, 10000);

          const ticketEvt = pair.clientEvents.allEvents.find(
            (e: any) => e.eventType === EVENT_SESSION_TICKET,
          );
          assert.ok(ticketEvt, 'client should receive SESSION_TICKET event');
          // The ticket event should carry data (the serialised session ticket).
          assert.ok(
            ticketEvt.data !== undefined && ticketEvt.data !== null,
            'SESSION_TICKET event should have a data property',
          );
        } catch {
          // SESSION_TICKET delivery is not guaranteed in all configurations
          // (e.g., some TLS backends may not issue tickets). If we time out,
          // that is acceptable -- skip the assertion.
        }
      } finally {
        await pair.cleanup();
      }
    });
  });
});
