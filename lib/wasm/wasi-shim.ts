/**
 * Host-agnostic WASI `preview1` import shim for the `http3-wasm` client
 * core (`dist/wasm/http3_client.wasm`).
 *
 * This module implements exactly the `wasi_snapshot_preview1` functions the
 * compiled artifact imports (see `test/wasm/artifact-shape.test.ts`'s frozen
 * allowlist, re-derived from the real module via `WebAssembly.Module.imports`
 * — do not extend this file speculatively from upstream WASI docs), plus a
 * handful of harmless defensive stubs the plan calls out as a superset.
 *
 * No host APIs are imported here beyond what every JS host (Node today,
 * workerd/browsers later) already provides globally: `globalThis.crypto`
 * and `globalThis.performance`. There is deliberately no `node:*` import in
 * this file — see docs/WASM_CLIENT_PLAN.md §6.2/§6.6 and the `lib/wasm/**`
 * ESLint zone in `eslint.config.mjs`.
 *
 * The `nowNs`/`random` injection points exist so tests can install a
 * deterministic mock clock (C5) and fast-forward idle/PTO timers without a
 * real wall-clock wait — never hardcode `performance.now()`/`crypto` calls
 * anywhere they can't be overridden.
 */

/** WASI `errno` values this shim actually returns (a tiny subset). */
const WASI_ESUCCESS = 0;
const WASI_EBADF = 8;

/** WASI `clockid_t` value for `CLOCK_MONOTONIC` (the only one this shim special-cases — everything else, including `CLOCK_REALTIME` (0), is treated as wall-clock-like below). */
const CLOCK_MONOTONIC = 1;

export interface ShimOptions {
  /**
   * Injectable clock, nanoseconds since an arbitrary (but for a single
   * shim instance, fixed) origin. Defaults to a `performance.now()`-based
   * reading. Both `CLOCK_MONOTONIC` and `CLOCK_REALTIME` preview1 requests
   * derive from this single function (anchored differently — see
   * {@link makePreview1Imports}) so a mocked clock advances both
   * coherently. quiche's internal `Instant::now()` resolves to the
   * `CLOCK_MONOTONIC` import, so no timestamps ever cross the wasm ABI
   * directly; this is the only place a test needs to hook in a fake clock.
   */
  nowNs?: () => bigint;
  /** Injectable CSPRNG; fills `buf` with random bytes in place. Defaults to `globalThis.crypto.getRandomValues`. */
  random?: (buf: Uint8Array) => void;
  /** Receives decoded UTF-8 text the module writes to fd 1 or fd 2 (panic messages, debug logs). Defaults to a no-op. */
  onStderr?: (text: string) => void;
}

function defaultNowNs(): bigint {
  // `performance.now()` is available in Node (>= this repo's engines floor),
  // browsers, and workerd — unlike `process.hrtime`, which is Node-only.
  // Sub-millisecond precision is lost (ms float -> ns bigint), which is fine:
  // quiche's timers operate in millisecond granularity anyway (§4.7).
  return BigInt(Math.round(performance.now() * 1_000_000));
}

function defaultRandom(buf: Uint8Array): void {
  // `globalThis.crypto.getRandomValues` is available in Node, browsers, and
  // workerd — unlike `node:crypto`'s `randomFillSync`. The cast below is
  // sound (never actually SharedArrayBuffer-backed here — this module's
  // wasm memory is never `shared: true`) and works around `Uint8Array`'s
  // generic buffer-type parameter (`ArrayBufferLike` by default) not
  // structurally matching the DOM lib's `Uint8Array<ArrayBuffer>` parameter
  // type in this TypeScript/lib version.
  globalThis.crypto.getRandomValues(buf as Uint8Array<ArrayBuffer>);
}

/**
 * Build a `wasi_snapshot_preview1` import object for instantiating the
 * `http3-wasm` module.
 *
 * `getMemory` is a thunk rather than a direct `WebAssembly.Memory` because
 * of the WASI shim's chicken-and-egg constraint: the imports object must
 * exist *before* `new WebAssembly.Instance(module, imports)` runs, but the
 * instance's exported memory doesn't exist until *after* instantiation
 * completes. Callers (see `lib/wasm/core-loader.ts`) pass a closure that
 * reads the real memory once instantiation has finished; none of these
 * imports are actually invoked until the module's exported functions are
 * called afterwards, so the thunk is never dereferenced too early.
 */
export function makePreview1Imports(
  opts: ShimOptions,
  getMemory: () => WebAssembly.Memory,
  // Two WASI preview1 imports (`clock_time_get`'s `precision`, `fd_seek`'s
  // `offset`) take an i64 `timestamp`/`filedelta`, which the wasm<->JS
  // boundary represents as `bigint`, not `number` — `any` rest args (with
  // `ignoreRestArgs` in eslint.config.mjs) is the least-worst way to let
  // each function below declare its own real, mixed number/bigint
  // parameter list instead of lying about them all being `number`.
): Record<string, (...args: any[]) => number> {
  const nowNs = opts.nowNs ?? defaultNowNs;
  const random = opts.random ?? defaultRandom;
  const onStderr = opts.onStderr ?? ((): void => {});

  const dataView = (): DataView => new DataView(getMemory().buffer);
  const bytes = (): Uint8Array => new Uint8Array(getMemory().buffer);

  // Anchor wall-clock (REALTIME) at the shim's creation time, then advance
  // it by the same delta the (possibly mocked) monotonic-like `nowNs`
  // reports — mirrors spikes/quiche-wasm-wasip1/mini-shim.mjs's approach,
  // generalized to an injectable clock instead of `process.hrtime`.
  const baseWallNs = BigInt(Date.now()) * 1_000_000n;
  const baseMonoNs = nowNs();

  return {
    random_get(ptr: number, len: number): number {
      random(bytes().subarray(ptr, ptr + len));
      return WASI_ESUCCESS;
    },

    clock_time_get(id: number, _precision: bigint, outPtr: number): number {
      const current = nowNs();
      const ns = id === CLOCK_MONOTONIC ? current : baseWallNs + (current - baseMonoNs);
      dataView().setBigUint64(outPtr, ns < 0n ? 0n : ns, true);
      return WASI_ESUCCESS;
    },

    environ_sizes_get(countPtr: number, bufSizePtr: number): number {
      dataView().setUint32(countPtr, 0, true);
      dataView().setUint32(bufSizePtr, 0, true);
      return WASI_ESUCCESS;
    },

    environ_get(_environPtr: number, _bufPtr: number): number {
      return WASI_ESUCCESS;
    },

    fd_write(fd: number, iovsPtr: number, iovsLen: number, nwrittenPtr: number): number {
      let written = 0;
      const view = dataView();
      const chunks: Uint8Array[] = [];
      for (let i = 0; i < iovsLen; i++) {
        const base = view.getUint32(iovsPtr + i * 8, true);
        const len = view.getUint32(iovsPtr + i * 8 + 4, true);
        chunks.push(bytes().slice(base, base + len));
        written += len;
      }
      if ((fd === 1 || fd === 2) && chunks.length > 0) {
        const total = new Uint8Array(written);
        let offset = 0;
        for (const chunk of chunks) {
          total.set(chunk, offset);
          offset += chunk.length;
        }
        onStderr(new TextDecoder('utf-8').decode(total));
      }
      dataView().setUint32(nwrittenPtr, written, true);
      return WASI_ESUCCESS;
    },

    proc_exit(code: number): never {
      throw new Error(`http3-wasm module called proc_exit(${String(code)})`);
    },

    // ---- Defensive stubs (superset of the module's actual ~12-import
    // surface — see the module doc comment). Every no-preopen /
    // no-filesystem response below is exactly what a libc startup path
    // that defensively probes for open fds/preopens expects to see when
    // there truly are none (this build excludes `os-runtime`/`qlog-files`
    // entirely — no filesystem-touching code path exists to actually reach
    // most of these). ----
    fd_close(_fd: number): number {
      return WASI_ESUCCESS;
    },
    fd_fdstat_get(_fd: number, _statPtr: number): number {
      return WASI_ESUCCESS;
    },
    fd_fdstat_set_flags(_fd: number, _flags: number): number {
      return WASI_ESUCCESS;
    },
    fd_prestat_get(_fd: number, _prestatPtr: number): number {
      return WASI_EBADF;
    },
    fd_prestat_dir_name(_fd: number, _pathPtr: number, _pathLen: number): number {
      return WASI_EBADF;
    },
    fd_read(_fd: number, _iovsPtr: number, _iovsLen: number, nreadPtr: number): number {
      // No real file descriptors are ever opened by this module — report
      // EOF (0 bytes) rather than fail, in case a defensive/startup code
      // path ever reaches this.
      dataView().setUint32(nreadPtr, 0, true);
      return WASI_ESUCCESS;
    },
    fd_seek(_fd: number, _offset: bigint, _whence: number, _newOffsetPtr: number): number {
      return WASI_ESUCCESS;
    },
    path_filestat_get(_fd: number, _flags: number, _pathPtr: number, _pathLen: number, _bufPtr: number): number {
      return WASI_EBADF;
    },
    path_open(
      _fd: number,
      _dirflags: number,
      _pathPtr: number,
      _pathLen: number,
      _oflags: number,
      _fsRightsBase: number,
      _fsRightsInheriting: number,
      _fdflags: number,
      _openedFdPtr: number,
    ): number {
      return WASI_EBADF;
    },
    sched_yield(): number {
      return WASI_ESUCCESS;
    },
  };
}
