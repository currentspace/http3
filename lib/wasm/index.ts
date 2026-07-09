/**
 * Node-facing entry point for the wasm client runtime's low-level building
 * blocks, reachable via the package's `"./wasm"` subpath export's
 * `default` condition (`package.json`).
 *
 * Most consumers should not need this file at all — the public,
 * documented way to use the wasm runtime from Node is `connect()`/
 * `connectAsync()`/`connectQuic()`/`connectQuicAsync()` with
 * `runtimeMode: 'wasm'` (see docs/WASM_RUNTIME.md), which
 * `lib/client.ts`/`lib/quic-client.ts` already wire up internally. This
 * module exists for advanced use cases that want the wasm core/event loop
 * directly (e.g. driving it outside this package's session/stream
 * wrapper), and as the Node-facing sibling of
 * {@link "./index.workerd.js" | lib/wasm/index.workerd.ts} (the
 * `workerd`/`worker` export conditions) — re-exporting the same
 * host-agnostic surface, plus the two Node-only conveniences
 * (`loadHttp3WasmCoreFromFile`, `connectNodeUdp`) that a workerd host
 * can't use.
 *
 * Deliberately excluded from `tsconfig.workerd.json` (unlike the rest of
 * `lib/wasm/**`): this file imports the Node-only
 * `node-core-loader.ts`/`node-udp-adapter.ts`, so it cannot compile
 * without `@types/node` — exactly why it's a *separate* file from
 * `index.workerd.ts` rather than one file with both surfaces.
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

export { WasmH3ServerEventLoop } from './h3-server-event-loop.js';
export type { WasmH3ServerEventLoopOptions } from './h3-server-event-loop.js';

export { WasmQuicServerEventLoop } from './quic-server-event-loop.js';
export type { WasmQuicServerEventLoopOptions } from './quic-server-event-loop.js';

export type { DatagramServerTransport, DatagramServerTransportAddress } from './datagram-server-transport.js';

export { loadHttp3WasmCoreFromFile } from './node-core-loader.js';
export { connectNodeUdp } from './node-udp-adapter.js';
export type { ConnectNodeUdpOptions } from './node-udp-adapter.js';
export { bindNodeUdpServer } from './node-udp-server-adapter.js';
export type { BindNodeUdpServerOptions } from './node-udp-server-adapter.js';
