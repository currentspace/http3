import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import type { Duplex } from 'node:stream';
import { loadHttp3WasmCoreFromFile } from '../../lib/wasm/node-core-loader.js';
import { WasmMemoryError, ERR_HTTP3_WASM_MEMORY_LIMIT } from '../../lib/index.js';
import { WasmMemoryError as WasmEntryMemoryError } from '../../lib/wasm/index.js';
import { createWasmServerH3Pair, createWasmServerQuicPair, wasmArtifactPath, wasmSkipReason } from '../support/wasm-test-helpers.js';

function closed(stream: Duplex): Promise<void> {
  return new Promise(resolve => stream.once('close', resolve));
}

function assertMemoryError(error: unknown): void {
  assert.ok(error instanceof WasmMemoryError);
  assert.ok(error instanceof WasmEntryMemoryError);
  assert.equal(error.code, ERR_HTTP3_WASM_MEMORY_LIMIT);
  assert.ok(error.requestedBytes > 0);
  assert.equal(error instanceof WebAssembly.RuntimeError, false);
}

describe('WASM payload memory pressure', { skip: wasmSkipReason() }, () => {
  it('allocation failure returns an error without trapping or poisoning later allocations', () => {
    const core = loadHttp3WasmCoreFromFile(wasmArtifactPath());
    assert.throws(() => core.alloc(64 * 1024 * 1024), error => { assertMemoryError(error); return true; });
    const { ptr, len } = core.writeBytes(Uint8Array.of(1, 2, 3));
    assert.deepEqual(core.copyOut(ptr, len), Uint8Array.of(1, 2, 3));
    core.free(ptr, len);
  });

  it('each payload ABI reports failed ownership allocation before touching a session', () => {
    const core = loadHttp3WasmCoreFromFile(wasmArtifactPath());
    // Staging fits, but duplicating it cannot fit in the 64 MiB heap.
    const len = 32 * 1024 * 1024;
    const ptr = core.alloc(len);
    try {
      assert.equal(core.exports.hs_stream_send(0, 0, 0n, ptr, len, 1), -5n);
      assert.equal(core.exports.h3c_stream_send(0, 0n, ptr, len, 1), -5n);
      assert.equal(core.exports.qs_stream_send(0, 0, 0n, ptr, len, 1), -5n);
      assert.equal(core.exports.qc_stream_send(0, 0n, ptr, len, 1), -5n);
    } finally { core.free(ptr, len); }
    assert.equal(core.exports.hs_stream_send(0, 0, 0n, 0, 0, 1), -3n);
  });

  for (const mib of [32, 64]) {
    for (const method of ['respondWithBody', 'write', 'end'] as const) {
      it(`H3 server ${method}: ${mib} MiB rejection reaches the application and preserves the connection`, async t => {
        const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
        t.after(async () => { await pair.cleanup(); });
        let failure!: Promise<unknown[]>;
        let callbackFailure!: Promise<Error | null | undefined>;
        let callbackCalls = 0;
        pair.server.on('stream', (stream, headers) => {
          stream.on('error', () => {});
          if (headers[':path'] === '/probe') { stream.respondWithBody({}, 'ok'); return; }
          failure = once(stream, 'error');
          const body = Buffer.alloc(mib * 1024 * 1024);
          if (method === 'respondWithBody') stream.respondWithBody({}, body);
          else {
            stream.respond({});
            callbackFailure = new Promise(resolve => {
              stream[method](body, (error?: Error | null) => { callbackCalls++; resolve(error); });
            });
          }
        });
        const request = pair.client.request({ ':path': '/' }, { endStream: true });
        request.on('error', () => {});
        const peerClosed = closed(request);
        request.resume();
        await peerClosed;
        assertMemoryError((await failure)[0]);
        if (method !== 'respondWithBody') { assertMemoryError(await callbackFailure); assert.equal(callbackCalls, 1); }
        const probe = pair.client.request({ ':path': '/probe' }, { endStream: true });
        let response = '';
        for await (const data of probe) response += String(data);
        assert.equal(response, 'ok');
      });
    }
  }

  for (const mib of [32, 64]) {
    it(`H3 client end: ${mib} MiB rejection reaches its callback and preserves the connection`, async t => {
      const pair = await createWasmServerH3Pair({ clientRuntimeMode: 'wasm' });
      t.after(async () => { await pair.cleanup(); });
      pair.server.on('stream', (stream, headers) => {
        stream.on('error', () => {});
        if (headers[':path'] === '/probe') stream.respondWithBody({}, 'ok');
        else stream.resume();
      });
      const request = pair.client.request({ ':method': 'POST', ':path': '/' });
      const failure = once(request, 'error');
      const done = closed(request);
      const callbackError = await new Promise(resolve => request.end(Buffer.alloc(mib * 1024 * 1024), (error?: Error | null) => resolve(error)));
      assertMemoryError(callbackError);
      assertMemoryError((await failure)[0]);
      await done;
      const probe = pair.client.request({ ':path': '/probe' }, { endStream: true });
      let response = '';
      for await (const data of probe) response += String(data);
      assert.equal(response, 'ok');
    });
  }

  for (const side of ['client', 'server'] as const) {
    for (const mib of [32, 64]) {
      it(`QUIC ${side} end: ${mib} MiB rejection leaves another stream usable`, async t => {
        const pair = await createWasmServerQuicPair({ clientRuntimeMode: 'wasm' });
        t.after(async () => { await pair.cleanup(); });
        const incoming = once(pair.serverSession, 'stream');
        const clientStream = pair.client.openStream();
        clientStream.on('error', () => {});
        clientStream.write(Buffer.from('hello'));
        const [serverStream] = await incoming;
        serverStream.on('error', () => {});
        serverStream.resume(); clientStream.resume();
        const sender: Duplex = side === 'client' ? clientStream : serverStream;
        const failure = once(sender, 'error');
        const done = closed(sender);
        const callbackError = await new Promise(resolve => sender.end(Buffer.alloc(mib * 1024 * 1024), (error?: Error | null) => resolve(error)));
        assertMemoryError(callbackError);
        assertMemoryError((await failure)[0]);
        await done;
        pair.serverSession.once('stream', stream => { stream.on('error', () => {}); stream.resume(); stream.end(Buffer.from('ok')); });
        const probe = pair.client.openStream();
        probe.end(Buffer.from('probe'));
        let response = '';
        for await (const data of probe) response += String(data);
        assert.equal(response, 'ok');
      });
    }
  }
});
