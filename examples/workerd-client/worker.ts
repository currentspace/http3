/**
 * C7 workerd smoke harness (docs/WASM_CLIENT_PLAN.md §7/§9) + illustrative
 * sample worker — see README.md for exactly what this does and does not
 * prove, and `docs/WASM_RUNTIME.md`'s "Workers / workerd status" section
 * for the full picture.
 *
 * There is no outbound UDP in workerd today (cloudflare/workerd#4463), so
 * this cannot drive a real network handshake. What it *does* prove, for
 * real, under an actual `workerd`/`wrangler dev` V8 isolate:
 *
 *   1. The compiled `http3_client.wasm` module — the exact artifact
 *      `pnpm run build:wasm` produces, imported the same way Wrangler
 *      bundles any `.wasm` module dependency (a precompiled
 *      `WebAssembly.Module`, never compiled from bytes at runtime) —
 *      instantiates successfully with only this package's own WASI shim
 *      (`lib/wasm/wasi-shim.ts`) — no `node:wasi`, no runtime WASI at all.
 *   2. The full `h3c_*`/`qc_*` export surface resolves as expected.
 *   3. `WasmH3ClientEventLoop` (the same class `lib/client.ts` uses on
 *      Node) constructs and runs its `connect()` pump against an
 *      in-memory mock `DatagramTransport` — no real sockets — proving the
 *      synchronous QUIC/TLS Initial-packet generation (RNG via
 *      `crypto.getRandomValues`, clock via `performance.now()`, the
 *      `queueMicrotask`-deferred event dispatch, the single-`setTimeout`
 *      pump/timer discipline) all run correctly inside workerd's own JS
 *      engine, not just Node's.
 */

// Real package-specifier imports (not a relative path into the repo) —
// `package.json`'s `"./wasm"` subpath export resolves this to
// `dist/wasm/index.workerd.js` under the `workerd`/`worker` conditions
// Wrangler sets, and to `dist/wasm/index.d.ts` (Node's shape — the one
// with `connectNodeUdp`/`loadHttp3WasmCoreFromFile`, deliberately absent
// here) otherwise. `examples/workerd-client/package.json` depends on
// `@currentspace/http3` via `workspace:*`, so this resolves through a
// real pnpm workspace symlink, exactly like an npm-installed consumer.
import { loadHttp3WasmCore, WasmH3ClientEventLoop } from '@currentspace/http3/wasm';
import type { DatagramTransport } from '@currentspace/http3/wasm';
// The wasm binary itself has no package `exports` entry (it's a build
// artifact, not part of the API) — reached via a plain relative path into
// the dependency's own installed files, which (being a relative path, not
// a bare specifier) bypasses `exports` resolution entirely, the same way
// any real npm consumer would reach
// `node_modules/@currentspace/http3/dist/wasm/http3_client.wasm`.
// eslint-disable-next-line import/no-unresolved -- resolved by wrangler's bundler, not tsc/eslint
import wasmModule from './node_modules/@currentspace/http3/dist/wasm/http3_client.wasm';

interface SmokeResult {
  step: string;
  ok: boolean;
  detail: string;
}

/** A `DatagramTransport` with no real I/O at all — records sends, never delivers a response. */
function createRecordingMockTransport(sent: Uint8Array[]): DatagramTransport {
  return {
    send(datagram: Uint8Array): void {
      sent.push(datagram.slice());
    },
    onMessage(): void {
      /* no peer exists to ever call this */
    },
    localAddress(): { address: string; family: string; port: number } {
      return { address: '203.0.113.10', family: 'IPv4', port: 12345 };
    },
    close(): Promise<void> {
      return Promise.resolve();
    },
  };
}

async function runSmokeTest(): Promise<SmokeResult[]> {
  const results: SmokeResult[] = [];

  // 1. Module instantiation (no node:wasi, no runtime WASI — just this
  // package's own shim, driven by `loadHttp3WasmCore`).
  const core = loadHttp3WasmCore({ module: wasmModule });
  results.push({
    step: 'instantiate',
    ok: Boolean(core.exports.memory),
    detail: 'WebAssembly.Instance created; exported linear memory present',
  });

  // 2. Export shape.
  const expectedExports = ['h3c_new', 'h3c_recv', 'h3c_next_send', 'h3c_drain_events', 'h3c_close', 'qc_new', 'qc_open_stream', 'wasm_alloc', 'wasm_free'];
  const missing = expectedExports.filter((name) => typeof (core.exports as unknown as Record<string, unknown>)[name] !== 'function');
  results.push({
    step: 'exports',
    ok: missing.length === 0,
    detail: missing.length === 0 ? `all ${String(expectedExports.length)} sampled h3c_*/qc_* exports resolved` : `missing: ${missing.join(', ')}`,
  });

  // 3. Construct the same event loop class lib/client.ts uses, against an
  // in-memory mock transport, and drive connect() far enough to generate
  // a real QUIC Initial packet — no real network I/O anywhere.
  const sent: Uint8Array[] = [];
  const dispatched: unknown[] = [];
  let connectError: string | null = null;
  const eventLoop = new WasmH3ClientEventLoop(
    {
      core: loadHttp3WasmCore({ module: wasmModule }), // fresh instance — this one's session-only
      transportFactory: (): Promise<DatagramTransport> => Promise.resolve(createRecordingMockTransport(sent)),
      rejectUnauthorized: false,
      maxIdleTimeoutMs: 1000,
    },
    (events) => {
      dispatched.push(...events);
    },
  );

  try {
    // RFC 5737 TEST-NET-3 address — documentation-only, never routable;
    // nothing is ever actually sent over a socket (the mock transport
    // above has no I/O), so this cannot leak/attempt any real connection.
    await eventLoop.connect('203.0.113.10:443', 'example.invalid');
  } catch (err: unknown) {
    connectError = err instanceof Error ? err.message : String(err);
  }

  const firstPacket = sent[0];
  const looksLikeQuicInitial = Boolean(
    firstPacket &&
    firstPacket.length >= 1200 && // QUIC Initial minimum datagram size
    (firstPacket[0] as number) >= 0xc0, // long-header form + fixed bit set
  );
  results.push({
    step: 'connect-generates-initial-packet',
    ok: connectError === null && looksLikeQuicInitial,
    detail: connectError !== null
      ? `connect() threw: ${connectError}`
      : `captured ${String(sent.length)} outbound datagram(s); first packet ${String(firstPacket?.length ?? 0)} bytes, leading byte 0x${(firstPacket?.[0] ?? 0).toString(16)}`,
  });

  // No real peer will ever respond, so there is no handshake to wait for
  // here — closing promptly rather than waiting out the idle timeout.
  await eventLoop.close();

  return results;
}

export default {
  async fetch(): Promise<Response> {
    try {
      const results = await runSmokeTest();
      const allOk = results.every((r) => r.ok);
      return new Response(JSON.stringify({ ok: allOk, results }, null, 2), {
        status: allOk ? 200 : 500,
        headers: { 'content-type': 'application/json' },
      });
    } catch (err: unknown) {
      const message = err instanceof Error ? `${err.message}\n${err.stack ?? ''}` : String(err);
      return new Response(JSON.stringify({ ok: false, error: message }, null, 2), {
        status: 500,
        headers: { 'content-type': 'application/json' },
      });
    }
  },
};
