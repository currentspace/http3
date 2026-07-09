/**
 * Server-side wasm support: H3 loopback tests exercising a wasm-backed
 * `Http3SecureServer` (`runtimeMode: 'wasm'`) through the full public API
 * (`server.on('stream', ...)`, `stream.respond()`/`respondWithBody()`/
 * `sendTrailers()`/`close()`), paired against:
 *
 *  - a **native** client (`connectAsync`, default `runtimeMode`) — cell 5
 *    of the 8-cell client x server x {H3,QUIC} runtime matrix.
 *  - a **wasm** client (`connectAsync(..., { runtimeMode: 'wasm' })`) —
 *    cell 7, the most novel combination: two independent wasm module
 *    instances (one client, one server) talking over a real loopback UDP
 *    socket pair, both driven entirely by this phase's new TS code.
 *
 * Mirrors test/wasm/h3-loopback.test.ts's scenario style (handshake, GET
 * w/ body, backpressure, trailers, datagrams, ping, close timing) for the
 * cell-7 (wasm x wasm) cases specifically, since those most exercise the
 * new server-side code.
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips cleanly
 * (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import type { ClientHttp3Stream } from '../../lib/stream.js';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../lib/stream.js';
import { createWasmServerH3Pair, wasmSkipReason } from '../support/wasm-test-helpers.js';
import type { WasmServerH3Pair } from '../support/wasm-test-helpers.js';

interface ResponseResult {
  status: string;
  headers: Record<string, string>;
  body: Buffer;
}

function waitForResponse(stream: ClientHttp3Stream, timeoutMs = 5000): Promise<ResponseResult> {
  return new Promise((resolve, reject) => {
    let status = '';
    let headers: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('waitForResponse timed out')), timeoutMs);
    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      headers = h;
    });
    stream.on('data', (chunk: Buffer) => chunks.push(chunk));
    stream.on('end', () => {
      clearTimeout(timer);
      resolve({ status, headers, body: Buffer.concat(chunks) });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

function waitForServerStream(pair: WasmServerH3Pair, timeoutMs = 5000): Promise<{ stream: ServerHttp3Stream; headers: IncomingHeaders; flags: StreamFlags }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('waitForServerStream timed out')), timeoutMs);
    pair.server.once('stream', (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
      clearTimeout(timer);
      resolve({ stream, headers, flags });
    });
  });
}

describe('wasm H3 SERVER loopback', { skip: wasmSkipReason() }, () => {
  describe('native client x wasm server (matrix cell 5)', () => {
    it('handshake completes, GET request/response, clean close on both sides', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'portable' });
      try {
        assert.equal(pair.client.handshakeComplete, true);
        // The client is native here — its own runtime is 'portable', not 'wasm'.
        assert.equal(pair.client.runtimeInfo?.selectedMode, 'portable');

        const serverStreamPromise = waitForServerStream(pair);
        const clientStream = pair.client.request({
          ':method': 'GET',
          ':path': '/hello',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });

        const { stream: serverStream } = await serverStreamPromise;
        serverStream.respondWithBody({ ':status': '200', 'x-server': 'wasm-h3-server' }, 'hello from wasm server');

        const res = await waitForResponse(clientStream);
        assert.equal(res.status, '200');
        assert.equal(res.headers['x-server'], 'wasm-h3-server');
        assert.equal(res.body.toString('utf8'), 'hello from wasm server');

        const closedPromise = new Promise<void>((resolve) => pair.client.once('close', () => resolve()));
        const start = Date.now();
        await pair.client.close();
        await closedPromise;
        assert.ok(Date.now() - start < 2500, 'client close() should resolve promptly via the shutdown sentinel');
      } finally {
        await pair.cleanup();
      }
    });
  });

  describe('wasm client x wasm server (matrix cell 7)', () => {
    it('handshake completes on both sides', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      try {
        assert.equal(pair.client.handshakeComplete, true);
        assert.equal(pair.client.runtimeInfo?.selectedMode, 'wasm');
        assert.equal(pair.client.runtimeInfo?.driver, 'wasm');
      } finally {
        await pair.cleanup();
      }
    });

    it('GET request receives a response with a body', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      try {
        const serverStreamPromise = waitForServerStream(pair);
        const clientStream = pair.client.request({
          ':method': 'GET',
          ':path': '/hello',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });

        const { stream: serverStream, flags } = await serverStreamPromise;
        assert.equal(flags.endStream, true);
        serverStream.respond({ ':status': '200', 'x-server': 'wasm-x-wasm' });
        serverStream.end('hello from an all-wasm loopback');

        const res = await waitForResponse(clientStream);
        assert.equal(res.status, '200');
        assert.equal(res.headers['x-server'], 'wasm-x-wasm');
        assert.equal(res.body.toString('utf8'), 'hello from an all-wasm loopback');
      } finally {
        await pair.cleanup();
      }
    });

    it('request body upload exercises backpressure (STREAM_BLOCKED -> DRAIN)', { timeout: 15000 }, async () => {
      const pair = await createWasmServerH3Pair({
        clientRuntimeMode: 'wasm',
        initialMaxData: 32 * 1024,
        initialMaxStreamDataBidiLocal: 16 * 1024,
      });
      try {
        const serverStreamPromise = waitForServerStream(pair);
        const clientStream = pair.client.request({
          ':method': 'POST',
          ':path': '/upload',
          ':authority': 'localhost',
          ':scheme': 'https',
        });

        const { stream: serverStream } = await serverStreamPromise;
        const bodyPromise = new Promise<Buffer>((resolve, reject) => {
          const chunks: Buffer[] = [];
          const timer = setTimeout(() => reject(new Error('server body collection timed out')), 12000);
          // Draining the readable side lets quiche's flow control grant the
          // client fresh MAX_STREAM_DATA credit — mirrors
          // test/core/flow-control-window.test.ts's identical technique.
          serverStream.on('data', (chunk: Buffer) => chunks.push(chunk));
          serverStream.on('end', () => {
            clearTimeout(timer);
            resolve(Buffer.concat(chunks));
          });
          serverStream.on('error', reject);
        });

        const payload = Buffer.alloc(256 * 1024, 'U');
        let sawBackpressure = false;
        const chunkSize = 8192;
        for (let offset = 0; offset < payload.length; offset += chunkSize) {
          const chunk = payload.subarray(offset, Math.min(offset + chunkSize, payload.length));
          const ok = clientStream.write(chunk);
          if (!ok) {
            sawBackpressure = true;
            await new Promise<void>((resolve) => clientStream.once('drain', resolve));
          }
        }
        clientStream.end();

        const received = await bodyPromise;
        assert.equal(received.length, payload.length);
        assert.equal(Buffer.compare(received, payload), 0);
        assert.equal(sawBackpressure, true, 'expected at least one backpressured write (STREAM_BLOCKED -> DRAIN) for a 256KB body over a 32KB connection window');

        serverStream.respondWithBody({ ':status': '200' }, 'ok');
        const res = await waitForResponse(clientStream);
        assert.equal(res.status, '200');
      } finally {
        await pair.cleanup();
      }
    });

    it('trailers arrive as a "trailers" event after the response body', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      try {
        const serverStreamPromise = waitForServerStream(pair);
        const clientStream = pair.client.request({
          ':method': 'GET',
          ':path': '/trailers',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });

        const trailersPromise = new Promise<Record<string, string>>((resolve) => {
          clientStream.once('trailers', (t: Record<string, string>) => resolve(t));
        });

        const { stream: serverStream } = await serverStreamPromise;
        serverStream.respond({ ':status': '200' });
        serverStream.write('body');
        serverStream.sendTrailers({ 'x-trailer': 'trailer-value' });
        serverStream.end();

        const res = await waitForResponse(clientStream);
        assert.equal(res.body.toString('utf8'), 'body');
        const trailers = await trailersPromise;
        assert.equal(trailers['x-trailer'], 'trailer-value');
      } finally {
        await pair.cleanup();
      }
    });

    it('datagram round-trip (client -> server -> client)', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm', enableDatagrams: true });
      try {
        const clientPayload = Buffer.from('hello from wasm-x-wasm client datagram');
        const serverDatagramPromise = new Promise<Buffer>((resolve) => {
          pair.serverSession.once('datagram', (data: Buffer) => resolve(data));
        });
        assert.equal(pair.client.sendDatagram(clientPayload), true);

        const serverData = await serverDatagramPromise;
        assert.equal(Buffer.compare(serverData, clientPayload), 0);

        const echoPayload = Buffer.from('echo from wasm server');
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

    it('close() resolves promptly on both sides', async () => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      try {
        const start = Date.now();
        await pair.client.close();
        const clientElapsed = Date.now() - start;
        assert.ok(clientElapsed < 2500, `client close() took ${clientElapsed}ms`);
        assert.equal(pair.client.closed, true);

        const serverCloseStart = Date.now();
        await pair.server.close();
        const serverElapsed = Date.now() - serverCloseStart;
        assert.ok(serverElapsed < 2500, `server close() took ${serverElapsed}ms`);
      } finally {
        // pair.cleanup() closes both again — idempotent no-ops.
        await pair.cleanup();
      }
    });
  });
});
