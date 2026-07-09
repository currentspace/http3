/**
 * Workers/workerd entry point (Phase 5, docs/WASM_CLIENT_PLAN.md §9).
 *
 * This is a **thin re-export module**, not a new implementation: every
 * piece it exposes already exists in `lib/wasm/**` and is exactly what
 * Node's `lib/client.ts`/`lib/quic-client.ts` build against today. The
 * only things this file adds are (1) picking the subset of `lib/wasm/**`
 * that is meaningful outside Node, and (2) one clearly-labeled,
 * always-throwing `DatagramTransport` stub — see below.
 *
 * Reached via the package's `"./wasm"` subpath export's `workerd`/`worker`
 * conditions (`package.json`), so a workerd/Wrangler build resolves
 * `@currentspace/http3/wasm` to this file (compiled to
 * `dist/wasm/index.workerd.js`) instead of the Node-facing sibling,
 * `lib/wasm/index.ts` (`dist/wasm/index.js`, the `default` condition) —
 * see that file's doc comment for the two extra, Node-only exports it
 * carries that this file deliberately does not
 * (`loadHttp3WasmCoreFromFile`, `connectNodeUdp`). `lib/client.ts`/
 * `lib/quic-client.ts` still reach `lib/wasm/**` via their own internal
 * dynamic imports rather than through either aggregate — these two files
 * are for external consumers (this package's own code doesn't need them).
 *
 * ## What a workerd caller does with this
 *
 * ```ts
 * import wasmModule from './http3_client.wasm'; // wrangler: precompiled WebAssembly.Module
 * import { WasmH3ClientEventLoop, loadHttp3WasmCore } from '@currentspace/http3/wasm';
 * import type { DatagramTransport } from '@currentspace/http3/wasm';
 *
 * const core = loadHttp3WasmCore({ module: wasmModule });
 * const myTransport: DatagramTransport = { ... }; // caller's own — see below
 * const eventLoop = new WasmH3ClientEventLoop(
 *   { core, transportFactory: async () => myTransport, rejectUnauthorized: false },
 *   (events) => { ... },
 * );
 * await eventLoop.connect('203.0.113.1:443', 'example.com');
 * ```
 *
 * ## Why there is no real transport here (docs/WASM_CLIENT_PLAN.md §9 C1)
 *
 * Workers has **no outbound UDP client socket API today**. `node:dgram`
 * under `nodejs_compat` is an import-compatible stub that silently drops
 * sends rather than a working transport — the plan is explicit that
 * "module presence must never be trusted" as a sign that UDP works. So
 * this file does **not** ship a `connectWorkerdUdp`-style adapter the way
 * `lib/wasm/node-udp-adapter.ts` does for Node: writing one would either
 * (a) silently do nothing useful, reproducing exactly the failure mode the
 * plan warns against, or (b) require inventing a fake API surface that
 * doesn't exist yet and would almost certainly not match whatever workerd
 * eventually ships (tracked at cloudflare/workerd#4463 — see
 * docs/WASM_RUNTIME.md's "Workers / workerd status" section for the full
 * writeup and a draft tracking-issue template). Instead:
 *
 * - The real path: implement {@link DatagramTransport} yourself against
 *   whatever workerd API exists by the time you read this, and pass it (or
 *   a factory for it) to the event loop constructor.
 * - {@link createUnavailableDatagramTransport} is a convenience stub for
 *   exactly one case — you're wiring this up *before* a real transport
 *   exists (e.g. exercising module loading / instantiation in CI) and want
 *   a loud, descriptive, impossible-to-miss failure the moment any I/O is
 *   actually attempted, rather than a silent no-op. It is **not** a
 *   working transport and never will be one.
 */

export { loadHttp3WasmCore } from './core-loader.js';
export type { Http3WasmCore, Http3WasmExports, WasmCoreSource } from './core-loader.js';

export { makePreview1Imports } from './wasi-shim.js';
export type { ShimOptions } from './wasi-shim.js';

export type { WasmEvent } from './events.js';

export type { CommonWasmClientOptions } from './wasm-options.js';

export { WasmH3ClientEventLoop } from './h3-client-event-loop.js';
export type { WasmH3ClientEventLoopOptions } from './h3-client-event-loop.js';

export { WasmQuicClientEventLoop } from './quic-client-event-loop.js';
export type { WasmQuicClientEventLoopOptions } from './quic-client-event-loop.js';

export type { DatagramTransport, DatagramTransportAddress } from './datagram-transport.js';
import type { DatagramTransport } from './datagram-transport.js';

function workerdUdpUnavailableError(): Error {
  return new Error(
    'workerd outbound UDP client sockets do not exist yet ' +
    '(see cloudflare/workerd#4463: https://github.com/cloudflare/workerd/discussions/4463). ' +
    'createUnavailableDatagramTransport() is a loud-failure placeholder, never a working ' +
    "transport — supply a real DatagramTransport once workerd ships one. Do not trust " +
    "node:dgram's mere presence under nodejs_compat as a sign that UDP works: it is an " +
    'import-compatible stub that silently drops sends.',
  );
}

/**
 * A {@link DatagramTransport} that throws a descriptive error from every
 * method. **Not a working transport** — see the module doc comment for
 * when this is (and is not) an appropriate thing to pass around. Every
 * method throws (or rejects) synchronously/immediately on first use, so a
 * caller who wires this in by mistake gets an unmissable failure at the
 * first `send`/`onMessage`/`localAddress`/`close` call rather than a
 * silently-inert connection.
 */
export function createUnavailableDatagramTransport(): DatagramTransport {
  return {
    send(): void {
      throw workerdUdpUnavailableError();
    },
    onMessage(): void {
      throw workerdUdpUnavailableError();
    },
    localAddress(): never {
      throw workerdUdpUnavailableError();
    },
    async close(): Promise<void> {
      // The `await` is a no-op (nothing here is genuinely asynchronous) —
      // present only so this satisfies both `promise-function-async` (must
      // be `async`) and `require-await` (an `async` function must contain
      // an `await`). Functionally identical to an immediately-rejected
      // promise.
      await Promise.resolve();
      throw workerdUdpUnavailableError();
    },
  };
}
