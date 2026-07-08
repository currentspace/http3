// Minimal hand-rolled WASI preview1 shim — no node:wasi.
//
// Extended (Phase 2 of docs/WASM_CLIENT_PLAN.md) with the stubs the real
// http3-wasm module needs beyond the original 6-import tiny test crate:
// `fd_prestat_get`, `fd_prestat_dir_name`, `fd_read`. Re-enumerating the
// real module's imports (see spikes/quiche-wasm-wasip1/README.md and
// test/wasm/artifact-shape.test.ts) showed it needs 12 of the
// wasi_snapshot_preview1 surface, not the original 15-name candidate list
// this file's header comment used to reference — `sched_yield` (already
// present below) turned out to be needed (crossbeam-channel's spin-wait
// backoff calls `thread::yield_now()` even single-threaded), while
// `fd_fdstat_set_flags`/`path_filestat_get`/`path_open` turned out NOT to
// be needed (the wasm build excludes `os-runtime`'s TempFileGuard and
// qlog-files entirely, so no filesystem-touching code path exists to
// reference them).
//
// Exports `makePreview1()` so other scripts (e.g. validate-handshake.mjs)
// can reuse this exact shim against a different module; still runnable
// standalone against the original tiny test crate for backwards compat.
import { readFileSync } from 'node:fs';
import { randomFillSync } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const WASI_ESUCCESS = 0;
const WASI_EBADF = 8;
const CLOCK_REALTIME = 0;
const CLOCK_MONOTONIC = 1;

/**
 * Build a wasi_snapshot_preview1 import object.
 *
 * @param {object} [opts]
 * @param {() => WebAssembly.Memory} [opts.getMemory] Returns the
 *   instance's exported memory. Required before any import is actually
 *   invoked (i.e. call `setMemory`/pass this in before instantiation
 *   completes triggers a call) — matches every real preview1 shim's
 *   chicken-and-egg constraint (the memory only exists once the module
 *   the imports belong to has been instantiated).
 * @param {(text: string) => void} [opts.onStderr]
 */
export function makePreview1(opts = {}) {
  let memoryRef = null;
  const getMemory = opts.getMemory ?? (() => memoryRef);
  const setMemory = (m) => {
    memoryRef = m;
  };
  const onStderr = opts.onStderr ?? ((text) => process.stderr.write(text));
  const onStdout = opts.onStdout ?? ((text) => process.stdout.write(text));

  const mem = () => new DataView(getMemory().buffer);
  const u8 = () => new Uint8Array(getMemory().buffer);

  const baseWall = BigInt(Date.now()) * 1_000_000n;
  const baseHr = process.hrtime.bigint();

  const wasi_snapshot_preview1 = {
    random_get(ptr, len) {
      randomFillSync(u8().subarray(ptr, ptr + len));
      return WASI_ESUCCESS;
    },
    clock_time_get(id, _precision, outPtr) {
      const ns = id === CLOCK_MONOTONIC ? process.hrtime.bigint() : baseWall + (process.hrtime.bigint() - baseHr);
      mem().setBigUint64(outPtr, ns, true);
      return WASI_ESUCCESS;
    },
    environ_sizes_get(countPtr, bufSizePtr) {
      mem().setUint32(countPtr, 0, true);
      mem().setUint32(bufSizePtr, 0, true);
      return WASI_ESUCCESS;
    },
    environ_get(_environPtr, _bufPtr) {
      return WASI_ESUCCESS;
    },
    fd_write(fd, iovsPtr, iovsLen, nwrittenPtr) {
      let written = 0;
      const chunks = [];
      for (let i = 0; i < iovsLen; i++) {
        const base = mem().getUint32(iovsPtr + i * 8, true);
        const len = mem().getUint32(iovsPtr + i * 8 + 4, true);
        chunks.push(Buffer.from(getMemory().buffer, base, len));
        written += len;
      }
      const text = Buffer.concat(chunks).toString('utf8');
      (fd === 2 ? onStderr : onStdout)(text);
      mem().setUint32(nwrittenPtr, written, true);
      return WASI_ESUCCESS;
    },
    proc_exit(code) {
      throw new Error(`wasm proc_exit(${code})`);
    },
    fd_fdstat_get() {
      return WASI_ESUCCESS;
    },
    fd_fdstat_set_flags() {
      return WASI_ESUCCESS;
    },
    fd_seek() {
      return WASI_ESUCCESS;
    },
    fd_close() {
      return WASI_ESUCCESS;
    },
    sched_yield() {
      return WASI_ESUCCESS;
    },
    // --- Added for the real http3-wasm module (no preopens: every fd is
    // "not a preopen", which is exactly what an fd-3-upward preopen-
    // enumeration loop at libc startup expects to see immediately). ---
    fd_prestat_get(_fd, _prestatPtr) {
      return WASI_EBADF;
    },
    fd_prestat_dir_name(_fd, _pathPtr, _pathLen) {
      return WASI_EBADF;
    },
    fd_read(_fd, _iovsPtr, _iovsLen, nreadPtr) {
      // No real file descriptors are ever opened by this module (no
      // os-runtime, no qlog) — report EOF (0 bytes) rather than fail, in
      // case this is ever reached from a defensive/startup code path.
      mem().setUint32(nreadPtr, 0, true);
      return WASI_ESUCCESS;
    },
    path_filestat_get() {
      return WASI_EBADF;
    },
    path_open() {
      return WASI_EBADF;
    },
  };

  return { wasi_snapshot_preview1, setMemory };
}

// ---- Standalone CLI mode (backwards compatible with the original spike) ----
const isMain = process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1];
if (isMain) {
  const bytes = readFileSync(process.argv[2]);
  const { wasi_snapshot_preview1, setMemory } = makePreview1();

  const mod = new WebAssembly.Module(bytes);
  const needed = WebAssembly.Module.imports(mod).map((i) => i.name);
  const inst = new WebAssembly.Instance(mod, { wasi_snapshot_preview1 });
  setMemory(inst.exports.memory);

  const { core_init, core_alloc, core_recv, core_now_ns, core_rand, core_panic_path } = inst.exports;
  core_init();
  const p = core_alloc(1350);
  new Uint8Array(inst.exports.memory.buffer).fill(7, p, p + 1350);
  console.log('needed imports:', needed.join(', '));
  console.log('core_recv sum:', core_recv(p, 1350), '(expect', 7 * 1350 + ')');
  console.log('core_now_ns:', core_now_ns());
  console.log('core_rand:', core_rand().toString(16));
  console.log('core_panic_path(1):', core_panic_path(1));
  try {
    core_panic_path(0xdeadbeef);
  } catch (e) {
    console.log('panic surfaced as JS exception:', String(e).slice(0, 80));
  }
}
