/**
 * Branches client event loop construction between the native binding and
 * the wasm runtime (docs/WASM_CLIENT_PLAN.md §6.6).
 *
 * Design note: this factory takes the **native** and **wasm** constructors
 * as caller-supplied thunks rather than owning both construction paths
 * itself. The obvious alternative — importing the concrete native
 * `ClientEventLoop`/`QuicClientEventLoop` classes here and building them
 * directly — would work fine for H3 (`ClientEventLoop` is exported from
 * `lib/event-loop.ts`, which never imports this file), but raw QUIC's
 * native event loop class is private to `lib/quic-client.ts`; importing it
 * here would make `quic-client.ts -> this file -> quic-client.ts` a real
 * circular import. The thunk shape sidesteps that entirely (this file
 * never imports `lib/client.ts` or `lib/quic-client.ts`) while still
 * satisfying every functional requirement: branch on `mode`, dynamic-import
 * `lib/wasm/**` lazily and only on the wasm branch (so native-only
 * consumers never load any wasm code), and never invoke the native
 * constructor thunk for `mode === 'wasm'` (so `getBinding()` — and the
 * literal string `'wasm'` — never reaches the native binding).
 */

import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import type { SelectedRuntimeMode } from './runtime.js';

/**
 * Branch between a native and a wasm client event loop constructor.
 * `createWasm` is only ever invoked (and its `import()` only ever
 * resolved) when `mode === 'wasm'`.
 */
export async function createClientEventLoop<TLoop>(
  mode: SelectedRuntimeMode,
  createNative: () => TLoop,
  createWasm: () => Promise<TLoop>,
): Promise<TLoop> {
  if (mode === 'wasm') {
    return createWasm();
  }
  return createNative();
}

/**
 * Locate `dist/wasm/http3_client.wasm` relative to this package's own
 * install location — robust to both the production compiled layout
 * (`dist/client-event-loop-factory.js`, one level under the package root)
 * and the test compiled layout (`dist-test/lib/client-event-loop-factory.js`,
 * two levels under it), by walking up looking for `package.json` the same
 * way `lib/event-loop.ts`'s `findBinding()` locates the native binding.
 * Deliberately does NOT anchor on `pnpm-workspace.yaml` (as
 * `test/wasm/artifact-shape.test.ts`'s test-only `findRepoRoot()` does) —
 * this function must also resolve correctly from an installed npm package,
 * which has no workspace file at all.
 */
export function resolveWasmArtifactPath(): string {
  const searched: string[] = [];
  let dir = __dirname;
  for (let i = 0; i < 6; i++) {
    const candidate = join(dir, 'dist', 'wasm', 'http3_client.wasm');
    searched.push(candidate);
    if (existsSync(candidate) && existsSync(join(dir, 'package.json'))) {
      return candidate;
    }
    dir = resolve(dir, '..');
  }
  throw new Error(
    `Cannot find dist/wasm/http3_client.wasm. Searched:\n${searched.map((p) => `  - ${p}`).join('\n')}\n` +
    'Run `pnpm run build:wasm` first (requires WASI_SDK_PATH).',
  );
}
