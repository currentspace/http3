/**
 * Server-side wasm support: raw QUIC loopback tests exercising a
 * wasm-backed `QuicServer` (`runtimeMode: 'wasm'`) through the full public
 * API (`server.on('session', ...)`, `session.on('stream', ...)`,
 * `session.openStream()`), paired against:
 *
 *  - a **native** client (`connectQuicAsync`, default `runtimeMode`) —
 *    cell 6 of the 8-cell client x server x {H3,QUIC} runtime matrix.
 *  - a **wasm** client (`connectQuicAsync(..., { runtimeMode: 'wasm' })`)
 *    — cell 8, the most novel combination: two independent wasm module
 *    instances (one client, one server) talking over a real loopback UDP
 *    socket pair.
 *
 * Mirrors test/wasm/quic-loopback.test.ts's scenario style (handshake,
 * bidi echo, server-initiated stream, backpressure, datagrams, close
 * timing) for the cell-8 (wasm x wasm) cases specifically.
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips cleanly
 * (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import type { QuicStream } from '../../lib/quic-stream.js';
import { createWasmServerQuicPair, wasmSkipReason } from '../support/wasm-test-helpers.js';

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

function waitForServerStream(session: { once(event: 'stream', listener: (stream: QuicStream) => void): unknown }, timeoutMs = 5000): Promise<QuicStream> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('waitForServerStream timed out')), timeoutMs);
    session.once('stream', (stream: QuicStream) => {
      clearTimeout(timer);
      resolve(stream);
    });
  });
}

describe('wasm QUIC SERVER loopback', { skip: wasmSkipReason() }, () => {
  describe('native client x wasm server (matrix cell 6)', () => {
    it('handshake completes, openStream() bidi echo, clean close on both sides', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'portable' });
      try {
        assert.equal(pair.client.handshakeComplete, true);
        assert.equal(pair.client.runtimeInfo?.selectedMode, 'portable');

        const serverStreamPromise = waitForServerStream(pair.serverSession);
        const clientStream = pair.client.openStream();
        const payload = Buffer.from('hello from native QUIC client x wasm server');
        clientStream.end(payload);

        const serverStream = await serverStreamPromise;
        const received = await collect(serverStream);
        assert.equal(Buffer.compare(received, payload), 0);
        serverStream.end(received); // echo back

        const echoed = await collect(clientStream);
        assert.equal(Buffer.compare(echoed, payload), 0);

        const start = Date.now();
        await pair.client.close();
        assert.ok(Date.now() - start < 2500, 'client close() should resolve promptly');

        const serverCloseStart = Date.now();
        await pair.server.close();
        assert.ok(Date.now() - serverCloseStart < 2500, 'server close() should resolve promptly');
      } finally {
        await pair.cleanup();
      }
    });
  });

  describe('wasm client x wasm server (matrix cell 8)', () => {
    it('handshake completes on both sides', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
      try {
        assert.equal(pair.client.handshakeComplete, true);
        assert.equal(pair.client.runtimeInfo?.selectedMode, 'wasm');
        assert.equal(pair.client.runtimeInfo?.driver, 'wasm');
      } finally {
        await pair.cleanup();
      }
    });

    it('openStream() bidi echo', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
      try {
        const serverStreamPromise = waitForServerStream(pair.serverSession);
        const clientStream = pair.client.openStream();
        const payload = Buffer.from('hello from an all-wasm QUIC loopback');
        clientStream.end(payload);

        const serverStream = await serverStreamPromise;
        const received = await collect(serverStream);
        assert.equal(Buffer.compare(received, payload), 0);
        serverStream.end(received);

        const echoed = await collect(clientStream);
        assert.equal(Buffer.compare(echoed, payload), 0);
      } finally {
        await pair.cleanup();
      }
    });

    it('server-initiated stream surfaces as a "stream" event on the client', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
      try {
        const clientStreamPromise = new Promise<QuicStream>((resolve) => {
          pair.client.once('stream', (stream: QuicStream) => resolve(stream));
        });

        const payload = Buffer.from('server-initiated push over an all-wasm loopback');
        const serverStream = pair.serverSession.openStream();
        serverStream.end(payload);

        const clientStream = await clientStreamPromise;
        const received = await collect(clientStream);
        assert.equal(Buffer.compare(received, payload), 0);
      } finally {
        await pair.cleanup();
      }
    });

    it('QuicStream write backpressure (STREAM_BLOCKED -> DRAIN) then FIN', { timeout: 15000 }, async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm', maxIdleTimeoutMs: 30_000 });
      try {
        const serverStreamPromise = waitForServerStream(pair.serverSession);
        const clientStream = pair.client.openStream();
        const payload = Buffer.alloc(512 * 1024, 'Q');

        let sawBackpressure = false;
        const chunkSize = 16 * 1024;
        for (let offset = 0; offset < payload.length; offset += chunkSize) {
          const chunk = payload.subarray(offset, Math.min(offset + chunkSize, payload.length));
          const ok = clientStream.write(chunk);
          if (!ok) {
            sawBackpressure = true;
            await new Promise<void>((resolve) => clientStream.once('drain', resolve));
          }
        }
        clientStream.end();

        const serverStream = await serverStreamPromise;
        const received = await collect(serverStream, 12000);

        assert.equal(sawBackpressure, true, 'expected at least one backpressured write for a 512KB body');
        assert.equal(received.length, payload.length);
        assert.equal(Buffer.compare(received, payload), 0);
      } finally {
        await pair.cleanup();
      }
    });

    it('datagram round-trip (client -> server -> client)', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm', enableDatagrams: true });
      try {
        const clientPayload = Buffer.from('hello from wasm-x-wasm QUIC client datagram');
        const serverDatagramPromise = new Promise<Buffer>((resolve) => {
          pair.serverSession.once('datagram', (data: Buffer) => resolve(data));
        });
        assert.equal(pair.client.sendDatagram(clientPayload), true);

        const serverData = await serverDatagramPromise;
        assert.equal(Buffer.compare(serverData, clientPayload), 0);

        const echoPayload = Buffer.from('echo from wasm QUIC server');
        const clientDatagramPromise = new Promise<Buffer>((resolve) => {
          pair.client.once('datagram', (data: Buffer) => resolve(data));
        });
        assert.equal(pair.serverSession.sendDatagram(echoPayload), true);

        const echoed = await clientDatagramPromise;
        assert.equal(Buffer.compare(echoed, echoPayload), 0);
      } finally {
        await pair.cleanup();
      }
    });

    it('close() resolves promptly and destroys open streams on both sides', async () => {
      const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
      try {
        const stream = pair.client.openStream();
        const closedPromise = new Promise<void>((resolve) => stream.once('close', resolve));

        const start = Date.now();
        await pair.client.close();
        const elapsed = Date.now() - start;
        assert.ok(elapsed < 2500, `client close() took ${elapsed}ms`);
        assert.equal(pair.client.closed, true);
        await closedPromise;

        const serverCloseStart = Date.now();
        await pair.server.close();
        assert.ok(Date.now() - serverCloseStart < 2500, 'server close() should resolve promptly');
      } finally {
        await pair.cleanup();
      }
    });
  });
});
