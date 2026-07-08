/**
 * C3 loopback helpers (docs/WASM_CLIENT_PLAN.md §7): pair a native server
 * (raw NAPI bindings — same construction as
 * `test/support/native-test-helpers.ts`'s `createH3Pair`/`createQuicPair`)
 * with a wasm-backed **client** connected through the full public API
 * (`connectAsync()`/`connectQuicAsync()` with `runtimeMode: 'wasm'`) —
 * proving the Phase 3 runtime-mode wiring end-to-end, not just the raw
 * event loop in isolation.
 *
 * Kept as a separate file from `native-test-helpers.ts` (rather than
 * extending it) so the always-loaded native FFI test helper module never
 * gains a wasm-conditional code path: every native test in the repo
 * imports `native-test-helpers.ts`, but only `test/wasm/**` imports this
 * file.
 *
 * Gated by `HTTP3_WASM=1` (same pattern as `HTTP3_LONGHAUL`/
 * `HTTP3_BROWSER_E2E` — see test/wasm/artifact-shape.test.ts's C2 test,
 * test/longhaul/*.test.ts): when unset, or when
 * `dist/wasm/http3_client.wasm` doesn't exist yet, callers must use
 * {@link wasmSkipReason} with node:test's `describe(..., { skip })` so
 * these suites self-skip rather than fail.
 */

import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { loadBinding, generateTestCerts, createEventCollector } from './native-test-helpers.js';
import type { EventCollector } from './native-test-helpers.js';
import { connectAsync } from '../../lib/client.js';
import { connectQuicAsync } from '../../lib/quic-client.js';
import type { Http3ClientSession } from '../../lib/client.js';
import type { QuicClientSession } from '../../lib/quic-client.js';

/** @internal Walk up from this file's compiled location until the repo root (identified by pnpm-workspace.yaml) is found. Mirrors test/wasm/artifact-shape.test.ts's findRepoRoot. */
function findRepoRoot(): string {
  let dir = __dirname;
  for (let i = 0; i < 8; i++) {
    if (existsSync(join(dir, 'pnpm-workspace.yaml'))) {
      return dir;
    }
    dir = resolve(dir, '..');
  }
  throw new Error(`could not locate repo root (pnpm-workspace.yaml) walking up from ${__dirname}`);
}

const ENABLED = Boolean(process.env.HTTP3_WASM);
const artifactPath = ENABLED ? join(findRepoRoot(), 'dist/wasm/http3_client.wasm') : '';
const artifactExists = ENABLED && existsSync(artifactPath);

/**
 * Reason a wasm-runtime suite should self-skip, or `false` if it should
 * run. Pass directly as `describe(name, { skip: wasmSkipReason() }, fn)`.
 */
export function wasmSkipReason(): string | false {
  if (!ENABLED) return 'set HTTP3_WASM=1 to enable wasm runtime tests';
  if (!artifactExists) return `wasm artifact not found at ${artifactPath} — run \`pnpm run build:wasm\` first`;
  return false;
}

/**
 * The resolved `dist/wasm/http3_client.wasm` path — only meaningful when
 * {@link wasmSkipReason} returns `false`. Exposed for tests that construct
 * `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` directly (bypassing
 * `connect()`/`connectQuic()`) — e.g. the C5 deterministic-clock test,
 * which needs to inject a mock `shim` the public API has no option for.
 */
export function wasmArtifactPath(): string {
  return artifactPath;
}

// ---- H3 pair ----

export interface WasmH3Pair {
  /** Raw NativeWorkerServerBinding — drive server-side behavior directly (sendResponseHeaders, streamSend, sendTrailers, closeSession, streamClose, sendDatagram, pingSession). */
  server: any;
  serverEvents: EventCollector;
  serverAddr: { address: string; port: number };
  /** The wasm-backed client, obtained via the full public connectAsync() API with runtimeMode: 'wasm'. */
  client: Http3ClientSession;
  cleanup(): Promise<void>;
}

export interface WasmH3PairOptions {
  enableDatagrams?: boolean;
  maxIdleTimeoutMs?: number;
  initialMaxStreamDataBidiLocal?: number;
  /** Applied symmetrically to both server and client — see test/core/flow-control-window.test.ts for the same technique used to force STREAM_BLOCKED/DRAIN in a bounded amount of data. */
  initialMaxData?: number;
}

/**
 * Native H3 server (`runtimeMode: 'portable'`, self-signed cert,
 * `disableRetry`) + a wasm-backed `Http3ClientSession` connected via
 * `connectAsync(..., { runtimeMode: 'wasm' })`.
 */
export async function createWasmH3Pair(opts?: WasmH3PairOptions): Promise<WasmH3Pair> {
  const binding = loadBinding();
  const certs = generateTestCerts();
  const serverEvents = createEventCollector();

  const server = new binding.NativeWorkerServer(
    {
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      runtimeMode: 'portable',
      enableDatagrams: opts?.enableDatagrams ?? false,
      ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
      ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
      ...(opts?.initialMaxData != null && { initialMaxData: opts.initialMaxData }),
    },
    serverEvents.callback,
  );

  const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

  const client = await connectAsync(`127.0.0.1:${addr.port}`, {
    rejectUnauthorized: false,
    runtimeMode: 'wasm',
    fallbackPolicy: 'error',
    servername: 'localhost',
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
    ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
    ...(opts?.initialMaxData != null && { initialMaxData: opts.initialMaxData }),
  });

  return {
    server,
    serverEvents,
    serverAddr: addr,
    client,
    async cleanup() {
      try { await client.close(); } catch { /* already closed */ }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    },
  };
}

// ---- QUIC pair ----

export interface WasmQuicPair {
  /** Raw NativeQuicServerBinding — drive server-side behavior directly. */
  server: any;
  serverEvents: EventCollector;
  serverAddr: { address: string; port: number };
  /** The wasm-backed client, obtained via the full public connectQuicAsync() API with runtimeMode: 'wasm'. */
  client: QuicClientSession;
  cleanup(): Promise<void>;
}

export interface WasmQuicPairOptions {
  enableDatagrams?: boolean;
  maxIdleTimeoutMs?: number;
}

/**
 * Native raw-QUIC server (`runtimeMode: 'portable'`, self-signed cert,
 * `disableRetry`) + a wasm-backed `QuicClientSession` connected via
 * `connectQuicAsync(..., { runtimeMode: 'wasm' })`.
 */
export async function createWasmQuicPair(opts?: WasmQuicPairOptions): Promise<WasmQuicPair> {
  const binding = loadBinding();
  const certs = generateTestCerts();
  const serverEvents = createEventCollector();

  const server = new binding.NativeQuicServer(
    {
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      runtimeMode: 'portable',
      enableDatagrams: opts?.enableDatagrams ?? false,
      ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
    },
    serverEvents.callback,
  );

  const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

  const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
    rejectUnauthorized: false,
    runtimeMode: 'wasm',
    fallbackPolicy: 'error',
    servername: 'localhost',
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
  });

  return {
    server,
    serverEvents,
    serverAddr: addr,
    client,
    async cleanup() {
      try { await client.close(); } catch { /* already closed */ }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    },
  };
}
