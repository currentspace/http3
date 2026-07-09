/**
 * Node-only convenience wrapper around `lib/wasm/core-loader.ts`'s
 * host-agnostic `loadHttp3WasmCore`: reads a `.wasm` file from disk,
 * compiles it (cached at module scope, keyed by path), and instantiates
 * it with the WASI shim.
 *
 * Deliberately a **separate file** from `core-loader.ts`, not an `if`
 * branch inside it: TypeScript resolves and type-checks a module's
 * imports whether they're reached via a static import or a dynamic
 * `import()` (verified empirically while building this phase — a dynamic
 * `import()` of a file that itself imports `node:fs` still surfaces that
 * file's `node:fs`-related type errors under a tsconfig without
 * `@types/node`), so the only way to keep `lib/wasm/core-loader.ts` (and
 * everything that imports it, including the workerd-facing
 * `lib/wasm/index.workerd.ts`) compiling under `tsconfig.workerd.json` is
 * a real file boundary: this file is never imported, statically or
 * dynamically, from anywhere under `lib/wasm/**` — only from Node-only
 * call sites (`lib/client.ts`, `lib/quic-client.ts`, and tests that
 * construct `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` directly).
 *
 * This is also why `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` take
 * an already-instantiated `Http3WasmCore` in their options rather than a
 * `wasmPath` string (Phase 3 had the latter; Phase 5 changed it, see
 * docs/WASM_CLIENT_PLAN.md §9): the *caller* decides how to get a core —
 * `loadHttp3WasmCoreFromFile` here for Node, `loadHttp3WasmCore({ module
 * })` from `core-loader.ts` directly for workerd (or a test's mock) — and
 * the event loop classes never need to know which.
 */

import { readFileSync } from 'node:fs';
import { loadHttp3WasmCore } from './core-loader.js';
import type { Http3WasmCore } from './core-loader.js';
import type { ShimOptions } from './wasi-shim.js';

const moduleCacheByPath = new Map<string, WebAssembly.Module>();

/**
 * Read + compile a `.wasm` file once, cached at module scope keyed by
 * path so repeated calls (e.g. one per connection) never recompile
 * (`new WebAssembly.Module` for this artifact's size is on the order of a
 * fraction of a millisecond, but there is no reason to repeat it).
 */
export function loadHttp3WasmCoreFromFile(path: string, opts: ShimOptions = {}): Http3WasmCore {
  let module = moduleCacheByPath.get(path);
  if (!module) {
    module = new WebAssembly.Module(readFileSync(path));
    moduleCacheByPath.set(path, module);
  }
  return loadHttp3WasmCore({ module }, opts);
}
