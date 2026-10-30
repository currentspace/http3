import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makePreview1Imports } from '../../lib/wasm/wasi-shim.js';

test('WASI cached views follow memory growth and retain exact clock values', () => {
  const memory = new WebAssembly.Memory({ initial: 1, maximum: 3 });
  let now = 123_456_789n;
  const shim = makePreview1Imports({ nowNs: () => now, random: bytes => bytes.fill(0x5a) }, () => memory);
  for (const pages of [0, 1, 0, 1]) {
    memory.grow(pages);
    now += 17n;
    const offset = memory.buffer.byteLength - 8;
    assert.equal(shim.clock_time_get!(1, 0n, offset), 0);
    assert.equal(new DataView(memory.buffer).getBigUint64(offset, true), now);
    assert.equal(shim.random_get!(0, 8), 0);
    assert.deepEqual([...new Uint8Array(memory.buffer, 0, 8)], Array<number>(8).fill(0x5a));
  }
  now = -1n;
  shim.clock_time_get!(1, 0n, 0);
  assert.equal(new DataView(memory.buffer).getBigUint64(0, true), 0n);
});

test('WASI realtime advances by the same injected clock delta', () => {
  const memory = new WebAssembly.Memory({ initial: 1 });
  let now = 0n;
  const shim = makePreview1Imports({ nowNs: () => now }, () => memory);
  shim.clock_time_get!(0, 0n, 0);
  const first = new DataView(memory.buffer).getBigUint64(0, true);
  now += 123_456_789n;
  memory.grow(1);
  shim.clock_time_get!(0, 0n, 0);
  assert.equal(new DataView(memory.buffer).getBigUint64(0, true) - first, now);
});
