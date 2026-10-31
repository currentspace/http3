import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { loadHttp3WasmCoreFromFile } from '../../lib/wasm/node-core-loader.js';
import { WasmMemoryError, ERR_HTTP3_WASM_MEMORY_LIMIT } from '../../lib/index.js';
import { WasmMemoryError as WasmEntryMemoryError } from '../../lib/wasm/index.js';
import { wasmArtifactPath, wasmSkipReason } from '../support/wasm-test-helpers.js';

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

});
