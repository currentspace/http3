// Minimal hand-rolled WASI preview1 shim — no node:wasi.
import { readFileSync } from 'node:fs';
import { randomFillSync } from 'node:crypto';

const bytes = readFileSync(process.argv[2]);

let memory; // set after instantiation
const mem = () => new DataView(memory.buffer);
const u8 = () => new Uint8Array(memory.buffer);

const WASI_ESUCCESS = 0;
const CLOCK_REALTIME = 0, CLOCK_MONOTONIC = 1;
const baseWall = BigInt(Date.now()) * 1_000_000n;
const baseHr = process.hrtime.bigint();

const wasi_snapshot_preview1 = {
  random_get(ptr, len) {
    randomFillSync(u8().subarray(ptr, ptr + len));
    return WASI_ESUCCESS;
  },
  clock_time_get(id, _precision, outPtr) {
    let ns;
    if (id === CLOCK_MONOTONIC) ns = process.hrtime.bigint();
    else ns = baseWall + (process.hrtime.bigint() - baseHr);
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
      chunks.push(Buffer.from(memory.buffer, base, len));
      written += len;
    }
    const text = Buffer.concat(chunks).toString('utf8');
    (fd === 2 ? process.stderr : process.stdout).write(text);
    mem().setUint32(nwrittenPtr, written, true);
    return WASI_ESUCCESS;
  },
  proc_exit(code) {
    throw new Error(`wasm proc_exit(${code})`);
  },
  // extras commonly needed by wasi-libc if stdio is initialized:
  fd_fdstat_get() { return WASI_ESUCCESS; },
  fd_seek() { return 8; /* EBADF-ish: ESPIPE=70? return errno inval */ },
  fd_close() { return WASI_ESUCCESS; },
  sched_yield() { return WASI_ESUCCESS; },
};

const mod = new WebAssembly.Module(bytes);
const needed = WebAssembly.Module.imports(mod).map(i => i.name);
const inst = new WebAssembly.Instance(mod, { wasi_snapshot_preview1 });
memory = inst.exports.memory;

const { core_init, core_alloc, core_recv, core_now_ns, core_rand, core_panic_path } = inst.exports;
core_init();
const p = core_alloc(1350);
u8().fill(7, p, p + 1350);
console.log('needed imports:', needed.join(', '));
console.log('core_recv sum:', core_recv(p, 1350), '(expect', 7 * 1350 + ')');
console.log('core_now_ns:', core_now_ns());
console.log('core_rand:', core_rand().toString(16));
console.log('core_panic_path(1):', core_panic_path(1));
try { core_panic_path(0xdeadbeef); } catch (e) {
  console.log('panic surfaced as JS exception:', String(e).slice(0, 80));
}
