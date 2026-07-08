/**
 * Loads the compiled `http3-wasm` module and instantiates it with the WASI
 * shim from `lib/wasm/wasi-shim.ts`, then wraps the raw exports in a
 * minimal typed facade (`Http3WasmCore`) plus memory-view helpers that
 * respect wasm's "memory.grow detaches views" hazard.
 *
 * `loadHttp3WasmCore` itself never touches `node:fs` — it accepts either an
 * already-compiled `WebAssembly.Module` or raw `bytes`, so the exact same
 * function works under a future workerd host (which bundles `.wasm` as a
 * precompiled `Module` import and cannot compile-from-bytes). The one
 * Node-specific convenience wrapper, {@link loadHttp3WasmCoreFromFile}, is
 * the sanctioned exception noted in docs/WASM_CLIENT_PLAN.md §6.5 ("no
 * node:fs imports outside core-loader.ts's file read").
 */

import { readFileSync } from 'node:fs';
import { makePreview1Imports } from './wasi-shim.js';
import type { ShimOptions } from './wasi-shim.js';

export interface WasmCoreSource {
  /** Precompiled module — e.g. a workerd `.wasm` module import, or a caller-managed cache. */
  module?: WebAssembly.Module;
  /** Raw wasm bytes to compile with `new WebAssembly.Module(bytes)`. */
  bytes?: Uint8Array;
}

/**
 * The raw `http3-wasm` export surface, typed from `crates/http3-wasm`'s
 * `extern "C"` signatures (`src/h3.rs` / `src/quic.rs` / `src/abi.rs`) —
 * not guessed. Every `u64` Rust parameter (`stream_id`) and every `i64`
 * Rust return value crosses the wasm<->JS boundary as `bigint`; every
 * `u32`/`i32` crosses as `number`.
 */
export interface Http3WasmExports {
  readonly memory: WebAssembly.Memory;

  // Every method below is annotated `this: void`: these are raw wasm
  // exports (effectively free functions bound to nothing), and several
  // are passed around as bare function references (e.g. to
  // `core.readLastError`/`drainKeylog`) — without `this: void`,
  // `@typescript-eslint/unbound-method` flags every such reference as a
  // potential accidental-`this`-scoping hazard.
  wasm_alloc(this: void, size: number): number;
  wasm_free(this: void, ptr: number, size: number): void;

  // ---- h3c_* (HTTP/3 client) ----
  h3c_new(this: void, optsPtr: number, optsLen: number): number;
  h3c_last_error(this: void, handle: number, bufPtr: number, cap: number): number;
  h3c_rx_buffer(this: void, handle: number): number;
  h3c_recv(this: void, handle: number, len: number): bigint;
  h3c_tx_buffer(this: void, handle: number): number;
  h3c_next_send(this: void, handle: number): bigint;
  h3c_timeout_ms(this: void, handle: number): bigint;
  h3c_on_timeout(this: void, handle: number): bigint;
  h3c_drain_events(this: void, handle: number, outPtrPtr: number): bigint;
  h3c_send_request(this: void, handle: number, headersJsonPtr: number, headersJsonLen: number, fin: number): bigint;
  h3c_stream_send(this: void, handle: number, streamId: bigint, ptr: number, len: number, fin: number): bigint;
  h3c_stream_close(this: void, handle: number, streamId: bigint, errorCode: number): bigint;
  h3c_send_datagram(this: void, handle: number, ptr: number, len: number): bigint;
  h3c_ping(this: void, handle: number): bigint;
  h3c_session_metrics(this: void, handle: number, outPtrPtr: number): bigint;
  h3c_remote_settings(this: void, handle: number, outPtrPtr: number): bigint;
  h3c_close(this: void, handle: number, code: number, reasonPtr: number, reasonLen: number): bigint;
  h3c_take_keylog(this: void, handle: number, outPtrPtr: number): bigint;
  h3c_is_done(this: void, handle: number): number;
  h3c_free(this: void, handle: number): void;

  // ---- qc_* (raw QUIC client) — mirrors h3c_* minus send_request/remote_settings, plus open_stream ----
  qc_new(this: void, optsPtr: number, optsLen: number): number;
  qc_last_error(this: void, handle: number, bufPtr: number, cap: number): number;
  qc_rx_buffer(this: void, handle: number): number;
  qc_recv(this: void, handle: number, len: number): bigint;
  qc_tx_buffer(this: void, handle: number): number;
  qc_next_send(this: void, handle: number): bigint;
  qc_timeout_ms(this: void, handle: number): bigint;
  qc_on_timeout(this: void, handle: number): bigint;
  qc_drain_events(this: void, handle: number, outPtrPtr: number): bigint;
  qc_open_stream(this: void, handle: number): bigint;
  qc_stream_send(this: void, handle: number, streamId: bigint, ptr: number, len: number, fin: number): bigint;
  qc_stream_close(this: void, handle: number, streamId: bigint, errorCode: number): bigint;
  qc_send_datagram(this: void, handle: number, ptr: number, len: number): bigint;
  qc_ping(this: void, handle: number): bigint;
  qc_session_metrics(this: void, handle: number, outPtrPtr: number): bigint;
  qc_close(this: void, handle: number, code: number, reasonPtr: number, reasonLen: number): bigint;
  qc_take_keylog(this: void, handle: number, outPtrPtr: number): bigint;
  qc_is_done(this: void, handle: number): number;
  qc_free(this: void, handle: number): void;
}

/**
 * Cached `Uint8Array`/`DataView` pair over a `WebAssembly.Memory`,
 * transparently re-acquired whenever `memory.grow()` has detached the
 * previous `ArrayBuffer` (identity check on `.buffer`) — docs/WASM_CLIENT_PLAN.md
 * §4.6.
 */
class MemoryView {
  private cachedBuffer: ArrayBuffer | null = null;
  private cachedBytes: Uint8Array | null = null;
  private cachedDataView: DataView | null = null;

  constructor(private readonly memory: WebAssembly.Memory) {}

  private refresh(): void {
    const buffer = this.memory.buffer;
    if (this.cachedBuffer !== buffer) {
      this.cachedBuffer = buffer;
      this.cachedBytes = new Uint8Array(buffer);
      this.cachedDataView = new DataView(buffer);
    }
  }

  bytes(): Uint8Array {
    this.refresh();
    // Non-null: `refresh()` always assigns both caches together.
    return this.cachedBytes as Uint8Array;
  }

  dataView(): DataView {
    this.refresh();
    return this.cachedDataView as DataView;
  }
}

/** Typed facade over the raw wasm exports, plus buffer-lifetime-safe memory helpers. */
export interface Http3WasmCore {
  readonly exports: Http3WasmExports;
  /** `wasm_alloc(size)` — caller must `free()` with the same size. */
  alloc(size: number): number;
  /** `wasm_free(ptr, size)` — `size` must match the original `alloc()` call. */
  free(ptr: number, size: number): void;
  /** Allocate + copy `data` into linear memory. Caller must `free()` when done. */
  writeBytes(data: Uint8Array): { ptr: number; len: number };
  /** Allocate + copy the UTF-8 encoding of `text` into linear memory. Caller must `free()` when done. */
  writeUtf8(text: string): { ptr: number; len: number };
  /**
   * Copy `len` bytes starting at `ptr` out of linear memory into a fresh
   * `Buffer` the caller owns indefinitely. Use this — never a raw view —
   * for anything that must outlive the next ABI call on the same handle
   * (crates/http3-wasm/src/lib.rs's buffer-lifetime rule).
   */
  copyOut(ptr: number, len: number): Buffer;
  /** Decode `len` bytes starting at `ptr` as UTF-8 text (decoding copies immediately; safe past the next ABI call). */
  readUtf8(ptr: number, len: number): string;
  /** Read a little-endian `u32` at `ptr`. */
  readU32(ptr: number): number;
  /** Allocate a reusable 4-byte scratch cell for the `(handle, outPtrPtr) -> len` ABI convention. Caller must `free(cell, 4)` when the session ends. */
  allocOutPtrCell(): number;
  /**
   * Given the `len` an `(handle, outPtrPtr) -> len` export returned and the
   * same `outPtrPtr` cell passed to it, read the absolute data pointer the
   * export wrote back and return a **copy** of the bytes.
   */
  readOutPtrResult(outPtrPtr: number, len: number): Buffer;
  /** Same as {@link readOutPtrResult}, decoded as UTF-8 text instead of copied bytes. */
  readOutPtrResultUtf8(outPtrPtr: number, len: number): string;
  /**
   * Fetch and decode an error message via a `*_last_error(handle, bufPtr, cap)`
   * export (`h3c_last_error` or `qc_last_error` — pass the bound export
   * function directly; both share this exact shape).
   */
  readLastError(lastErrorFn: (handle: number, bufPtr: number, cap: number) => number, handle: number): string;
  /**
   * Copy `data` into linear memory at an already-allocated `ptr` (e.g. the
   * fixed RX staging region `h3c_rx_buffer`/`qc_rx_buffer` return) without
   * allocating. `data.length` must not exceed the destination region's size.
   */
  writeAt(ptr: number, data: Uint8Array): void;
}

const LAST_ERROR_SCRATCH_CAP = 1024;

class Http3WasmCoreImpl implements Http3WasmCore {
  private readonly view: MemoryView;

  constructor(readonly exports: Http3WasmExports) {
    this.view = new MemoryView(exports.memory);
  }

  alloc(size: number): number {
    return this.exports.wasm_alloc(size);
  }

  free(ptr: number, size: number): void {
    this.exports.wasm_free(ptr, size);
  }

  writeBytes(data: Uint8Array): { ptr: number; len: number } {
    const ptr = this.alloc(data.length);
    this.view.bytes().set(data, ptr);
    return { ptr, len: data.length };
  }

  writeAt(ptr: number, data: Uint8Array): void {
    this.view.bytes().set(data, ptr);
  }

  writeUtf8(text: string): { ptr: number; len: number } {
    return this.writeBytes(new TextEncoder().encode(text));
  }

  copyOut(ptr: number, len: number): Buffer {
    if (len <= 0) return Buffer.alloc(0);
    // `Buffer.from(typedArrayOrBuffer)` copies; `Buffer.from(arrayBuffer, offset, length)`
    // would only create a *view* onto wasm memory — the mistake the
    // buffer-lifetime rule exists to prevent. Slicing the (fresh, per-call)
    // Uint8Array first, then wrapping in Buffer.from, forces a real copy.
    return Buffer.from(this.view.bytes().slice(ptr, ptr + len));
  }

  readUtf8(ptr: number, len: number): string {
    if (len <= 0) return '';
    return new TextDecoder('utf-8').decode(this.view.bytes().subarray(ptr, ptr + len));
  }

  readU32(ptr: number): number {
    return this.view.dataView().getUint32(ptr, true);
  }

  allocOutPtrCell(): number {
    return this.alloc(4);
  }

  readOutPtrResult(outPtrPtr: number, len: number): Buffer {
    if (len <= 0) return Buffer.alloc(0);
    return this.copyOut(this.readU32(outPtrPtr), len);
  }

  readOutPtrResultUtf8(outPtrPtr: number, len: number): string {
    if (len <= 0) return '';
    return this.readUtf8(this.readU32(outPtrPtr), len);
  }

  readLastError(lastErrorFn: (handle: number, bufPtr: number, cap: number) => number, handle: number): string {
    const ptr = this.alloc(LAST_ERROR_SCRATCH_CAP);
    try {
      const n = lastErrorFn(handle, ptr, LAST_ERROR_SCRATCH_CAP);
      return n > 0 ? this.readUtf8(ptr, n) : '(no error message)';
    } finally {
      this.free(ptr, LAST_ERROR_SCRATCH_CAP);
    }
  }
}

function resolveModule(src: WasmCoreSource): WebAssembly.Module {
  if (src.module) return src.module;
  if (src.bytes) {
    // Cast is sound: never actually SharedArrayBuffer-backed in practice
    // (works around Uint8Array's generic buffer-type parameter not
    // structurally matching this TypeScript/lib version's `BufferSource`).
    return new WebAssembly.Module(src.bytes as Uint8Array<ArrayBuffer>);
  }
  throw new Error('loadHttp3WasmCore: WasmCoreSource must supply either `module` or `bytes`');
}

/**
 * Instantiate the `http3-wasm` module with the WASI shim and wrap it in a
 * typed {@link Http3WasmCore} facade. Host-agnostic: works identically in
 * Node (pass `{ bytes }`) and a future workerd host (pass `{ module }` —
 * wrangler resolves `.wasm` imports as precompiled `Module`s and cannot
 * compile-from-bytes there).
 */
export function loadHttp3WasmCore(src: WasmCoreSource, opts: ShimOptions = {}): Http3WasmCore {
  const module = resolveModule(src);

  // A plain mutable-property container (rather than a reassigned `let`)
  // so `instance` itself stays a `const` binding — the WASI shim's
  // `getMemory` thunk needs to close over *something* mutable before
  // instantiation exists to assign into it.
  const state: { instance: WebAssembly.Instance | undefined } = { instance: undefined };
  const getMemory = (): WebAssembly.Memory => {
    if (!state.instance) {
      throw new Error('http3-wasm: a WASI import was invoked before instantiation completed');
    }
    return (state.instance.exports as unknown as Http3WasmExports).memory;
  };

  const wasi_snapshot_preview1 = makePreview1Imports(opts, getMemory);
  const instance = new WebAssembly.Instance(module, { wasi_snapshot_preview1 });
  state.instance = instance;

  return new Http3WasmCoreImpl(instance.exports as unknown as Http3WasmExports);
}

const moduleCacheByPath = new Map<string, WebAssembly.Module>();

/**
 * Node convenience path: read + compile a `.wasm` file once, cached at
 * module scope keyed by path so repeated calls (e.g. one per connection)
 * never recompile (`new WebAssembly.Module` for this artifact's size is
 * on the order of a fraction of a millisecond, but there is no reason to
 * repeat it). This is the sanctioned Node-specific exception in this
 * otherwise host-agnostic file — see the module doc comment.
 */
export function loadHttp3WasmCoreFromFile(path: string, opts: ShimOptions = {}): Http3WasmCore {
  let module = moduleCacheByPath.get(path);
  if (!module) {
    module = new WebAssembly.Module(readFileSync(path));
    moduleCacheByPath.set(path, module);
  }
  return loadHttp3WasmCore({ module }, opts);
}
