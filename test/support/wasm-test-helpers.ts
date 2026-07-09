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
 * imports `native-test-helpers.ts`, but only `test/wasm/**` imports most of
 * this file. The sole exception is {@link clientRuntimeMode} (C4,
 * docs/WASM_CLIENT_PLAN.md §7): `test/interop/h3-loopback.test.ts` and
 * `test/interop/quic-loopback.test.ts` import that one function to thread
 * `runtimeMode: 'wasm'` into their **client**-side `connect()`/
 * `connectQuic()` options when enabled, so the exact same interop
 * scenarios run over both runtimes. Importing it is safe even when
 * `HTTP3_WASM` is unset — the module-level check below is a cheap env-var
 * + `existsSync` read with no side effects, and every other import this
 * file has (`native-test-helpers.js`, `lib/client.js`, `lib/quic-client.js`)
 * is already part of those suites' own dependency graph.
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
import { createSecureServer } from '../../lib/server.js';
import type { Http3SecureServer } from '../../lib/server.js';
import { createQuicServer } from '../../lib/quic-server.js';
import type { QuicServer, QuicServerSession } from '../../lib/quic-server.js';
import type { Http3ServerSession } from '../../lib/session.js';
import type { RuntimeMode } from '../../lib/runtime.js';

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

/**
 * C4 (docs/WASM_CLIENT_PLAN.md §7): the `runtimeMode` the **client** side
 * of an interop suite should request, or `undefined` to fall back to
 * whatever the caller's default is (typically `'portable'`).
 *
 * Returns `'wasm'` under exactly the same gate {@link wasmSkipReason} uses
 * (`HTTP3_WASM` set **and** `dist/wasm/http3_client.wasm` built) so the
 * decision "does the wasm test lane run at all" and "do the parameterized
 * interop suites exercise the wasm client" always flip together. Returns
 * `undefined` otherwise — never `'portable'` itself, so callers stay in
 * charge of their own default (`connect(addr, { runtimeMode:
 * clientRuntimeMode() ?? 'portable' })`).
 */
export function clientRuntimeMode(): RuntimeMode | undefined {
  return wasmSkipReason() === false ? 'wasm' : undefined;
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

// ---- Wasm SERVER pairs (server-side wasm support) ----
//
// Unlike createWasmH3Pair/createWasmQuicPair above (native server driven via
// the raw NAPI binding, wasm only on the client), these helpers put the
// wasm runtime on the SERVER side, through the full public API
// (`createSecureServer`/`createQuicServer` with `runtimeMode: 'wasm'`) —
// there is no raw-binding equivalent for a wasm server (the wasm server
// event loop is only ever reached through `Http3SecureServer.listen()`/
// `QuicServer.listen()`). `clientRuntimeMode` parameterizes the CLIENT side
// so the same helper covers both "native client x wasm server" and "wasm
// client x wasm server" — see test/wasm/h3-server-loopback.test.ts /
// test/wasm/quic-server-loopback.test.ts.

/** Wait for `server.listen(port, host)` (`Http3SecureServer`, synchronous-returning) to actually bind, since its 'listening' event fires asynchronously. */
function waitForH3ServerListening(server: Http3SecureServer): Promise<{ address: string; family: string; port: number }> {
  return new Promise((resolve, reject) => {
    server.once('error', (err: Error) => reject(err));
    server.once('listening', () => {
      const addr = server.address();
      if (!addr) {
        reject(new Error('Http3SecureServer.address() is null right after the listening event'));
        return;
      }
      resolve(addr);
    });
  });
}

export interface WasmServerH3Pair {
  /** A real `Http3SecureServer` bound with `runtimeMode: 'wasm'` — drive it via the public session/stream API (`server.on('stream', ...)`), not a raw binding. */
  server: Http3SecureServer;
  /** The server-side session for `client`'s connection — captured eagerly (registered before the client connects) since the 'session' event has already fired by the time this pair resolves (the client only resolves after its own handshake completes, which implies the server-side session already exists). */
  serverSession: Http3ServerSession;
  serverAddr: { address: string; family: string; port: number };
  client: Http3ClientSession;
  cleanup(): Promise<void>;
}

export interface WasmServerH3PairOptions {
  /** The CLIENT's runtime mode — `'portable'` (native client x wasm server, cell 5) or `'wasm'` (wasm client x wasm server, cell 7). Default: `'portable'`. */
  clientRuntimeMode?: 'portable' | 'wasm';
  enableDatagrams?: boolean;
  maxIdleTimeoutMs?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxData?: number;
}

/**
 * A wasm-backed `Http3SecureServer` (`runtimeMode: 'wasm'`, self-signed
 * cert, `disableRetry`) paired with a client whose own runtime mode is
 * controlled by `opts.clientRuntimeMode`.
 */
export async function createWasmServerH3Pair(opts?: WasmServerH3PairOptions): Promise<WasmServerH3Pair> {
  const certs = generateTestCerts();

  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    runtimeMode: 'wasm',
    fallbackPolicy: 'error',
    disableRetry: true,
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
    ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
    ...(opts?.initialMaxData != null && { initialMaxData: opts.initialMaxData }),
  });

  // Registered before the client ever starts connecting (not after this
  // function returns) — the 'session' event fires as soon as the QUIC
  // handshake begins on the server side, which happens well before
  // connectAsync's returned promise resolves (that only resolves once the
  // full handshake completes), so a listener attached only after this
  // function returns would already have missed it.
  let serverSession: Http3ServerSession | null = null;
  server.on('session', (session) => {
    serverSession = session;
  });

  const listeningPromise = waitForH3ServerListening(server);
  server.listen(0, '127.0.0.1');
  const serverAddr = await listeningPromise;

  const client = await connectAsync(`127.0.0.1:${serverAddr.port}`, {
    rejectUnauthorized: false,
    runtimeMode: opts?.clientRuntimeMode ?? 'portable',
    fallbackPolicy: 'error',
    servername: 'localhost',
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
    ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
    ...(opts?.initialMaxData != null && { initialMaxData: opts.initialMaxData }),
  });

  if (!serverSession) {
    throw new Error('wasm H3 server did not emit a "session" event before the client finished its handshake');
  }

  return {
    serverSession,
    server,
    serverAddr,
    client,
    async cleanup() {
      try { await client.close(); } catch { /* already closed */ }
      try { await server.close(); } catch { /* already closed */ }
    },
  };
}

export interface WasmServerQuicPair {
  /** A real `QuicServer` bound with `runtimeMode: 'wasm'` — drive it via the public session/stream API (`server.on('session', ...)`), not a raw binding. */
  server: QuicServer;
  /** The server-side session for `client`'s connection — captured eagerly (registered before the client connects). Server-side handshake completion (which is what fires QuicServer's 'session' event) always precedes the client's own — the client's HANDSHAKE_DONE arrives only after the server has already processed the client's Finished — so this is reliably populated by the time this pair resolves. */
  serverSession: QuicServerSession;
  serverAddr: { address: string; family: string; port: number };
  client: QuicClientSession;
  cleanup(): Promise<void>;
}

export interface WasmServerQuicPairOptions {
  /** The CLIENT's runtime mode — `'portable'` (native client x wasm server, cell 6) or `'wasm'` (wasm client x wasm server, cell 8). Default: `'portable'`. */
  clientRuntimeMode?: 'portable' | 'wasm';
  enableDatagrams?: boolean;
  maxIdleTimeoutMs?: number;
}

/**
 * A wasm-backed `QuicServer` (`runtimeMode: 'wasm'`, self-signed cert,
 * `disableRetry`) paired with a client whose own runtime mode is
 * controlled by `opts.clientRuntimeMode`.
 */
export async function createWasmServerQuicPair(opts?: WasmServerQuicPairOptions): Promise<WasmServerQuicPair> {
  const certs = generateTestCerts();

  const server = createQuicServer({
    key: certs.key,
    cert: certs.cert,
    runtimeMode: 'wasm',
    fallbackPolicy: 'error',
    disableRetry: true,
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
  });

  // See WasmServerQuicPair.serverSession's doc comment for why eager
  // registration (before the client connects) reliably captures this.
  let serverSession: QuicServerSession | null = null;
  server.on('session', (session) => {
    serverSession = session;
  });

  const serverAddr = await server.listen(0, '127.0.0.1');

  const client = await connectQuicAsync(`127.0.0.1:${serverAddr.port}`, {
    rejectUnauthorized: false,
    runtimeMode: opts?.clientRuntimeMode ?? 'portable',
    fallbackPolicy: 'error',
    servername: 'localhost',
    enableDatagrams: opts?.enableDatagrams ?? false,
    ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
  });

  if (!serverSession) {
    throw new Error('wasm QUIC server did not emit a "session" event before the client finished its handshake');
  }

  return {
    server,
    serverSession,
    serverAddr,
    client,
    async cleanup() {
      try { await client.close(); } catch { /* already closed */ }
      try { await server.close(); } catch { /* already closed */ }
    },
  };
}
