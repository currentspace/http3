/**
 * C3 (docs/WASM_CLIENT_PLAN.md §7): raw QUIC loopback tests, wasm client
 * <-> native server — mirrors test/interop/quic-loopback.test.ts's
 * scenarios, driven through the wasm runtime via the full public API
 * (`connectQuicAsync(..., { runtimeMode: 'wasm' })`).
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips
 * cleanly (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import type { QuicStream } from '../../lib/quic-stream.js';
import { createWasmQuicPair, wasmSkipReason } from '../support/wasm-test-helpers.js';

const EVENT_NEW_STREAM = 2;
const EVENT_FINISHED = 5;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_DATAGRAM = 14;

function collect(stream: QuicStream, timeoutMs = 5000): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collect timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => chunks.push(chunk));
    stream.on('end', () => {
      clearTimeout(timer);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

describe('wasm QUIC loopback (C3)', { skip: wasmSkipReason() }, () => {
  it('handshake completes via connectQuicAsync with runtimeMode wasm', async () => {
    const pair = await createWasmQuicPair();
    try {
      assert.equal(pair.client.handshakeComplete, true);
      assert.equal(pair.client.runtimeInfo?.selectedMode, 'wasm');
      assert.equal(pair.client.runtimeInfo?.driver, 'wasm');
    } finally {
      await pair.cleanup();
    }
  });

  it('openStream() bidi echo', async () => {
    const pair = await createWasmQuicPair();
    try {
      const stream = pair.client.openStream();
      const payload = Buffer.from('hello from wasm QUIC client');
      // Write (and FIN) *before* awaiting the server-side event — opening
      // a QuicStream only allocates a local id; nothing reaches the wire
      // (and the server has nothing to observe) until end()/write() runs.
      stream.end(payload);

      const streamEvent = await pair.serverEvents.waitForEvent(EVENT_NEW_STREAM);
      assert.equal(Buffer.compare(Buffer.from(streamEvent.data), payload), 0);
      pair.server.streamSend(streamEvent.connHandle, streamEvent.streamId, streamEvent.data, streamEvent.fin ?? false);

      const echoed = await collect(stream);
      assert.equal(Buffer.compare(echoed, payload), 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('server-initiated stream surfaces as a "stream" event', async () => {
    const pair = await createWasmQuicPair();
    try {
      const handshakeEvt = await pair.serverEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE);
      const streamPromise = new Promise<QuicStream>((resolve) => {
        pair.client.once('stream', (stream: QuicStream) => resolve(stream));
      });

      const payload = Buffer.from('server-initiated push');
      // First server-initiated bidi stream ID per the QUIC stream-id
      // scheme (server bidi: 1, 5, 9, ...) — mirrors
      // QuicServerSession.openStream()'s own ID bookkeeping (lib/quic-server.ts),
      // replicated here since this test drives the raw native binding
      // directly rather than the QuicServerSession wrapper.
      pair.server.streamSend(handshakeEvt.connHandle, 1, payload, true);

      const stream = await streamPromise;
      assert.equal(stream.id, 1);
      const received = await collect(stream);
      assert.equal(Buffer.compare(received, payload), 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('QuicStream write backpressure (STREAM_BLOCKED -> DRAIN) then FIN', { timeout: 15000 }, async () => {
    const pair = await createWasmQuicPair({ maxIdleTimeoutMs: 30_000 });
    try {
      const stream = pair.client.openStream();
      const payload = Buffer.alloc(512 * 1024, 'Q');

      let sawBackpressure = false;
      const chunkSize = 16 * 1024;
      for (let offset = 0; offset < payload.length; offset += chunkSize) {
        const chunk = payload.subarray(offset, Math.min(offset + chunkSize, payload.length));
        const ok = stream.write(chunk);
        if (!ok) {
          sawBackpressure = true;
          await new Promise<void>((resolve) => stream.once('drain', resolve));
        }
      }

      // Echo everything back so we can assert on data integrity and FIN
      // handling in one pass: accumulate server-side events for this
      // stream, and once we observe fin, echo the whole thing back.
      const streamId = stream.id;
      const received = await new Promise<Buffer>((resolve, reject) => {
        const chunks: Buffer[] = [];
        let processed = 0;
        const deadline = Date.now() + 10000;
        const check = (): void => {
          const events = pair.serverEvents.allEvents as Array<{ eventType: number; streamId: number; connHandle: number; data?: Buffer; fin?: boolean }>;
          for (; processed < events.length; processed++) {
            const evt = events[processed];
            if (!evt || evt.streamId !== streamId) continue;
            // The completion check must be unconditional (not nested
            // inside the `evt.data` branch): raw QUIC signals "stream
            // fully done" via a distinct EVENT_FINISHED (5) — which
            // carries no data and no `fin: true` flag of its own — rather
            // than always setting `fin: true` on the last DATA event
            // (mirrors lib/quic-client.ts's own _onFinished handling).
            if (evt.data) chunks.push(Buffer.from(evt.data));
            if (evt.eventType === EVENT_FINISHED || evt.fin) {
              resolve(Buffer.concat(chunks));
              return;
            }
          }
          if (Date.now() > deadline) {
            reject(new Error('timed out waiting for full server-side stream body'));
            return;
          }
          setTimeout(check, 10);
        };
        stream.end(); // send FIN (no additional data — final chunk already written above)
        check();
      });

      assert.equal(sawBackpressure, true, 'expected at least one backpressured write for a 512KB body');
      assert.equal(received.length, payload.length);
      assert.equal(Buffer.compare(received, payload), 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('datagram round-trip (client -> server -> client)', async () => {
    const pair = await createWasmQuicPair({ enableDatagrams: true });
    try {
      const clientPayload = Buffer.from('hello from wasm QUIC client datagram');
      const serverDatagramPromise = pair.serverEvents.waitForEvent(EVENT_DATAGRAM);
      assert.equal(pair.client.sendDatagram(clientPayload), true);

      const serverEvt = await serverDatagramPromise;
      assert.ok(serverEvt.data);
      assert.equal(Buffer.compare(Buffer.from(serverEvt.data), clientPayload), 0);

      const echoPayload = Buffer.from('echo from native QUIC server');
      const clientDatagramPromise = new Promise<Buffer>((resolve) => {
        pair.client.once('datagram', (data: Buffer) => resolve(data));
      });
      assert.equal(pair.server.sendDatagram(serverEvt.connHandle, echoPayload), true);

      const echoed = await clientDatagramPromise;
      assert.equal(Buffer.compare(echoed, echoPayload), 0);
    } finally {
      await pair.cleanup();
    }
  });

  it('close() resolves promptly and destroys open streams', async () => {
    const pair = await createWasmQuicPair();
    try {
      const stream = pair.client.openStream();
      const closedPromise = new Promise<void>((resolve) => stream.once('close', resolve));

      const start = Date.now();
      await pair.client.close();
      const elapsed = Date.now() - start;
      assert.ok(elapsed < 2500, `close() took ${elapsed}ms — expected well under the 5s SHUTDOWN_TIMEOUT_MS fallback`);
      assert.equal(pair.client.closed, true);
      await closedPromise;
    } finally {
      await pair.cleanup();
    }
  });
});
