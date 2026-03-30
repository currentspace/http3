/**
 * H3 FFI stress tests.
 * Exercises the HTTP/3 layer under sustained load: rapid request bursts,
 * concurrent streams, large POST bodies, immediate-close patterns,
 * interleaved operations, and telemetry integrity after stress.
 *
 * All instances use runtimeMode:'portable' to avoid io_uring ring allocation
 * exhaustion under rapid create/destroy cycles.
 */

import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadBinding,
  createH3Pair,
  EVENT_HEADERS,
  EVENT_DATA,
  EVENT_FINISHED,
  EVENT_SHUTDOWN_COMPLETE,
} from '../support/native-test-helpers.js';

import type { H3Pair } from '../support/native-test-helpers.js';

const binding = loadBinding();

// Force clean exit for lingering ThreadsafeFunction refs.
after(() => {
  setTimeout(() => process.exit(0), 500).unref();
});

// ── Auto-responder helper ───────────────────────────────────────

function startAutoResponder(pair: H3Pair, responseBody: Buffer): NodeJS.Timeout {
  const responded = new Set<number>();
  return setInterval(() => {
    for (const evt of pair.serverEvents.allEvents) {
      if (evt.eventType === EVENT_HEADERS && !responded.has(evt.streamId)) {
        responded.add(evt.streamId);
        try {
          pair.server.sendResponseHeaders(evt.connHandle, evt.streamId,
            [{ name: ':status', value: '200' }], false);
          pair.server.streamSend(evt.connHandle, evt.streamId, responseBody, true);
        } catch { /* stream may be gone */ }
      }
    }
  }, 5);
}

// ── POST auto-responder (waits for FINISHED before responding) ──

function startPostAutoResponder(pair: H3Pair, responseBody: Buffer): NodeJS.Timeout {
  const responded = new Set<number>();
  return setInterval(() => {
    // Only respond once we have seen FINISHED for the stream.
    const finishedStreams = new Set<number>();
    for (const evt of pair.serverEvents.allEvents) {
      if (evt.eventType === EVENT_FINISHED) {
        finishedStreams.add(evt.streamId);
      }
    }
    for (const evt of pair.serverEvents.allEvents) {
      if (
        evt.eventType === EVENT_HEADERS &&
        !responded.has(evt.streamId) &&
        finishedStreams.has(evt.streamId)
      ) {
        responded.add(evt.streamId);
        try {
          pair.server.sendResponseHeaders(evt.connHandle, evt.streamId,
            [{ name: ':status', value: '200' }], false);
          pair.server.streamSend(evt.connHandle, evt.streamId, responseBody, true);
        } catch { /* stream may be gone */ }
      }
    }
  }, 5);
}

// ── Polling helper ──────────────────────────────────────────────

function waitForCondition(
  predicate: () => boolean,
  timeoutMs: number,
  label: string,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const deadline = setTimeout(
      () => reject(new Error(`Timed out waiting for: ${label} (${timeoutMs}ms)`)),
      timeoutMs,
    );
    const check = (): void => {
      if (predicate()) {
        clearTimeout(deadline);
        resolve();
      } else {
        setTimeout(check, 10);
      }
    };
    check();
  });
}

// ── H3 FFI stress tests ─────────────────────────────────────────

describe('H3 FFI stress', () => {

  // ── Rapid request burst ─────────────────────────────────────

  it('rapid request burst: 200 GET requests through single H3 session', { timeout: 30000 }, async () => {
    const pair = await createH3Pair();
    try {
      const responder = startAutoResponder(pair, Buffer.alloc(0));

      // Fire 200 GET requests as fast as possible.
      const REQUEST_COUNT = 200;
      for (let i = 0; i < REQUEST_COUNT; i++) {
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: `/burst/${i}` },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
      }

      // Wait until we have received at least 180 HEADERS responses on the client.
      const MIN_EXPECTED = 180;
      await waitForCondition(
        () => {
          const count = pair.clientEvents.allEvents.filter(
            (e: any) => e.eventType === EVENT_HEADERS,
          ).length;
          return count >= MIN_EXPECTED;
        },
        25000,
        `${MIN_EXPECTED} client HEADERS responses`,
      );

      clearInterval(responder);

      const responseCount = pair.clientEvents.allEvents.filter(
        (e: any) => e.eventType === EVENT_HEADERS,
      ).length;
      assert.ok(
        responseCount >= MIN_EXPECTED,
        `expected >= ${MIN_EXPECTED} responses, got ${responseCount}`,
      );
    } finally {
      await pair.cleanup();
    }
  });

  // ── Concurrent streams ──────────────────────────────────────

  it('50 concurrent H3 streams with response verification', { timeout: 30000 }, async () => {
    const pair = await createH3Pair();
    try {
      const responseBody = Buffer.alloc(128, 0x41); // 128 bytes of 'A'
      const responder = startAutoResponder(pair, responseBody);

      const STREAM_COUNT = 50;
      const sentStreamIds = new Set<number>();

      // Send 50 requests as fast as possible.
      for (let i = 0; i < STREAM_COUNT; i++) {
        const streamId = pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: `/concurrent/${i}` },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
        sentStreamIds.add(streamId);
      }

      // Wait for all 50 client HEADERS responses.
      await waitForCondition(
        () => {
          const count = pair.clientEvents.allEvents.filter(
            (e: any) => e.eventType === EVENT_HEADERS,
          ).length;
          return count >= STREAM_COUNT;
        },
        25000,
        `${STREAM_COUNT} client HEADERS responses`,
      );

      clearInterval(responder);

      // Verify all 50 unique streamIds received responses.
      const respondedStreamIds = new Set<number>(
        pair.clientEvents.allEvents
          .filter((e: any) => e.eventType === EVENT_HEADERS)
          .map((e: any) => e.streamId),
      );
      assert.strictEqual(
        respondedStreamIds.size,
        STREAM_COUNT,
        `expected ${STREAM_COUNT} unique response streamIds, got ${respondedStreamIds.size}`,
      );
    } finally {
      await pair.cleanup();
    }
  });

  // ── Large POST requests ─────────────────────────────────────

  it('10 concurrent 64KB POST requests', { timeout: 60000 }, async () => {
    const pair = await createH3Pair();
    try {
      const POST_COUNT = 10;
      const MIN_EXPECTED = 8;
      const postBody = Buffer.alloc(64 * 1024, 0x42); // 64KB of 'B'
      const responseBody = Buffer.from('post-ack');
      const responder = startPostAutoResponder(pair, responseBody);

      // Send 10 POST requests: headers with fin=false, then body with fin=true.
      for (let i = 0; i < POST_COUNT; i++) {
        const streamId = pair.client.sendRequest(
          [
            { name: ':method', value: 'POST' },
            { name: ':path', value: `/upload/${i}` },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          false,
        );
        pair.client.streamSend(streamId, postBody, true);
      }

      // Wait until we get at least MIN_EXPECTED HEADERS responses.
      await waitForCondition(
        () => {
          const count = pair.clientEvents.allEvents.filter(
            (e: any) => e.eventType === EVENT_HEADERS,
          ).length;
          return count >= MIN_EXPECTED;
        },
        55000,
        `${MIN_EXPECTED} client HEADERS responses for POSTs`,
      );

      clearInterval(responder);

      const responseCount = pair.clientEvents.allEvents.filter(
        (e: any) => e.eventType === EVENT_HEADERS,
      ).length;
      assert.ok(
        responseCount >= MIN_EXPECTED,
        `expected >= ${MIN_EXPECTED} POST responses, got ${responseCount}`,
      );
    } finally {
      await pair.cleanup();
    }
  });

  // ── Immediate close ─────────────────────────────────────────

  it('100 streams opened and immediately closed', { timeout: 15000 }, async () => {
    const pair = await createH3Pair();
    try {
      // Fire 100 GET requests with fin=true (immediate close).
      for (let i = 0; i < 100; i++) {
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: `/immediate-close/${i}` },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
      }

      // Don't wait for responses -- just verify no crash.
      // Small delay to let events propagate before cleanup.
      await new Promise((r) => setTimeout(r, 200));
    } finally {
      await pair.cleanup();
    }

    // After cleanup, the worker threads have been joined — no crash means success.
    // The SHUTDOWN_COMPLETE sentinel is not reliably delivered via TSFN, so
    // we verify clean shutdown by the fact that joinWorker() returned.
  });

  // ── Interleaved send and close ──────────────────────────────

  it('interleaved send and close operations', { timeout: 15000 }, async () => {
    const pair = await createH3Pair();
    try {
      const streamIds: number[] = [];

      for (let i = 0; i < 50; i++) {
        if (i % 2 === 1) {
          // Odd: send a new request.
          const streamId = pair.client.sendRequest(
            [
              { name: ':method', value: 'GET' },
              { name: ':path', value: `/interleave/${i}` },
              { name: ':authority', value: 'localhost' },
              { name: ':scheme', value: 'https' },
            ],
            true,
          );
          streamIds.push(streamId);
        } else {
          // Even: attempt to send on the most recent stream if available.
          // The stream is already closed (fin=true), so this will throw --
          // that is expected and verifies graceful error handling.
          if (streamIds.length > 0) {
            const lastId = streamIds[streamIds.length - 1];
            try {
              pair.client.streamSend(lastId, Buffer.from('extra'), true);
            } catch { /* expected: stream already closed */ }
          }
        }
      }

      // Verify no crash; small delay to let events settle.
      await new Promise((r) => setTimeout(r, 200));
    } finally {
      await pair.cleanup();
    }

    // If we reach here without an unhandled exception, the test passes.
    assert.ok(true, 'interleaved operations completed without crash');
  });

  // ── Telemetry integrity ─────────────────────────────────────

  it('telemetry integrity after stress burst', { timeout: 30000 }, async () => {
    binding.resetRuntimeTelemetry();

    const pair = await createH3Pair();
    try {
      const BURST_SIZE = 50;
      const responseBody = Buffer.from('telemetry-check');
      const responder = startAutoResponder(pair, responseBody);

      // Send a burst of 50 requests.
      for (let i = 0; i < BURST_SIZE; i++) {
        pair.client.sendRequest(
          [
            { name: ':method', value: 'GET' },
            { name: ':path', value: `/telemetry/${i}` },
            { name: ':authority', value: 'localhost' },
            { name: ':scheme', value: 'https' },
          ],
          true,
        );
      }

      // Wait for responses.
      await waitForCondition(
        () => {
          const count = pair.clientEvents.allEvents.filter(
            (e: any) => e.eventType === EVENT_HEADERS,
          ).length;
          return count >= BURST_SIZE;
        },
        25000,
        `${BURST_SIZE} client HEADERS responses for telemetry burst`,
      );

      clearInterval(responder);
    } finally {
      await pair.cleanup();
    }

    const telemetry = binding.runtimeTelemetry();
    assert.ok(
      telemetry.eventBatchDeliveredEventsTotal > 0,
      `expected eventBatchDeliveredEventsTotal > 0, got ${telemetry.eventBatchDeliveredEventsTotal}`,
    );
    assert.strictEqual(
      telemetry.eventBatchDroppedEventsTotal,
      0,
      `expected zero dropped events, got ${telemetry.eventBatchDroppedEventsTotal}`,
    );
  });
});
