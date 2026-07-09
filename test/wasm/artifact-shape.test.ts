/**
 * C2 (docs/WASM_CLIENT_PLAN.md §7): import/export allowlist test for the
 * `dist/wasm/http3_client.wasm` artifact `pnpm run build:wasm` produces.
 *
 * Fails on any dependency bump that grows the wasi_snapshot_preview1
 * syscall surface, and asserts the full `h3c_*`/`qc_*` ABI table (§5.3)
 * is present in the compiled module's exports.
 *
 * Gated by HTTP3_WASM=1 (same pattern as HTTP3_LONGHAUL / HTTP3_BROWSER_E2E
 * — see test/longhaul/*.test.ts, test/browser/browser-e2e.test.ts): when
 * unset, or when the artifact file doesn't exist yet (build:wasm hasn't
 * run), this suite self-skips with a clear reason — it never fails for
 * either of those two conditions.
 *
 * Artifact provenance: loads dist/wasm/http3_client.wasm exclusively (the
 * copy build:wasm produces) — never target/wasm32-wasip1/release/*.wasm
 * directly (per §7 C8's "Artifact provenance" note).
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ENABLED = Boolean(process.env.HTTP3_WASM);

/** Walk up from this file's compiled location (dist-test/test/wasm/) until
 * the repo root (identified by pnpm-workspace.yaml, unique to it) is
 * found — robust to dist-test's directory depth changing. */
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

const artifactPath = ENABLED ? join(findRepoRoot(), 'dist/wasm/http3_client.wasm') : '';
const artifactExists = ENABLED && existsSync(artifactPath);

const skipReason = !ENABLED
  ? 'set HTTP3_WASM=1 to enable wasm artifact tests'
  : !artifactExists
    ? `wasm artifact not found at ${artifactPath} — run \`pnpm run build:wasm\` first`
    : false;

/**
 * The normative allowlist — re-derived empirically from the actual built
 * module (via `WebAssembly.Module.imports`) rather than assumed from the
 * spike's speculative 15-name list in docs/WASM_CLIENT_PLAN.md §2 O5 /
 * §7 C2. Measured differences, both expected:
 *
 *  - `sched_yield` IS needed (not in the spike's 15-name list, which
 *    explicitly called it out as *not* expected): this crate's
 *    dependency graph includes `crossbeam-channel` (the chunk-pool
 *    return channel, used unconditionally — see A1 task 1's "keep
 *    crossbeam-channel" note), whose backoff/spin-wait path calls
 *    `std::thread::yield_now()` even in code that will only ever run
 *    single-threaded.
 *  - `fd_fdstat_get`, `fd_fdstat_set_flags`, `path_filestat_get`,
 *    `path_open` are NOT needed: this crate compiles the `http3` crate
 *    with `--no-default-features --features wasm-abi`, which excludes
 *    `os-runtime` (no `TempFileGuard`/temp-file TLS loading — this
 *    crate uses the always-compiled in-memory config builders instead)
 *    and excludes `qlog-files` (N5) — i.e. every filesystem-touching
 *    code path the spike's raw quiche test could have pulled in is
 *    absent here by construction.
 */
const ALLOWED_WASI_IMPORTS = new Set([
  'random_get',
  'environ_get',
  'environ_sizes_get',
  'clock_time_get',
  'fd_close',
  'fd_prestat_get',
  'fd_prestat_dir_name',
  'fd_read',
  'fd_seek',
  'fd_write',
  'proc_exit',
  'sched_yield',
]);

/** The full `h3c_*` HTTP/3 client + `qc_*` raw QUIC client ABI table
 * (docs/WASM_CLIENT_PLAN.md §5.3), plus `wasm_alloc`/`wasm_free`. */
const EXPECTED_EXPORTS = [
  'wasm_alloc',
  'wasm_free',
  'h3c_new',
  'h3c_last_error',
  'h3c_rx_buffer',
  'h3c_recv',
  'h3c_tx_buffer',
  'h3c_next_send',
  'h3c_timeout_ms',
  'h3c_on_timeout',
  'h3c_drain_events',
  'h3c_send_request',
  'h3c_stream_send',
  'h3c_stream_close',
  'h3c_send_datagram',
  'h3c_ping',
  'h3c_session_metrics',
  'h3c_remote_settings',
  'h3c_close',
  'h3c_take_keylog',
  'h3c_is_done',
  'h3c_free',
  'qc_new',
  'qc_last_error',
  'qc_rx_buffer',
  'qc_recv',
  'qc_tx_buffer',
  'qc_next_send',
  'qc_timeout_ms',
  'qc_on_timeout',
  'qc_drain_events',
  'qc_open_stream',
  'qc_stream_send',
  'qc_stream_close',
  'qc_send_datagram',
  'qc_ping',
  'qc_session_metrics',
  'qc_close',
  'qc_take_keylog',
  'qc_is_done',
  'qc_free',
];

/**
 * The full `hs_*` HTTP/3 server + `qs_*` raw QUIC server ABI table
 * (server-side wasm support, added alongside the client table above —
 * `crates/http3-wasm/src/h3_server.rs` / `src/quic_server.rs`). One handle
 * is the whole bound server (not a single connection); every
 * per-connection export additionally takes a `connHandle` parameter —
 * see those files' crate-level "client vs. server shape" doc comment.
 * `hs_send_trailers` is a deliberate, minimal, additive Rust ABI addition
 * made alongside the TS server runtime (the inherited surface omitted it —
 * see that export's own doc comment for why it's a genuinely distinct
 * primitive from `hs_send_response_headers`, not a duplicate).
 */
const EXPECTED_SERVER_EXPORTS = [
  'hs_new',
  'hs_last_error',
  'hs_rx_buffer',
  'hs_tx_buffer',
  'hs_recv',
  'hs_next_send',
  'hs_next_send_dest',
  'hs_timeout_ms',
  'hs_on_timeout',
  'hs_drain_events',
  'hs_send_response_headers',
  'hs_send_trailers',
  'hs_stream_send',
  'hs_stream_close',
  'hs_send_datagram',
  'hs_ping',
  'hs_session_metrics',
  'hs_remote_settings',
  'hs_close_connection',
  'hs_connection_is_closed',
  'hs_is_done',
  'hs_shutdown',
  'hs_free',
  'qs_new',
  'qs_last_error',
  'qs_rx_buffer',
  'qs_tx_buffer',
  'qs_recv',
  'qs_next_send',
  'qs_next_send_dest',
  'qs_timeout_ms',
  'qs_on_timeout',
  'qs_drain_events',
  'qs_stream_send',
  'qs_stream_close',
  'qs_send_datagram',
  'qs_ping',
  'qs_session_metrics',
  'qs_close_connection',
  'qs_connection_is_closed',
  'qs_is_done',
  'qs_shutdown',
  'qs_free',
];

describe('wasm artifact shape (C2)', { skip: skipReason }, () => {
  const bytes = readFileSync(artifactPath);
  const mod = new WebAssembly.Module(bytes);

  it('compiles as a valid WebAssembly module', () => {
    assert.ok(mod instanceof WebAssembly.Module);
  });

  it('imports only wasi_snapshot_preview1 functions from the frozen allowlist', () => {
    const imports = WebAssembly.Module.imports(mod);
    assert.ok(imports.length > 0, 'expected at least one import');
    for (const imp of imports) {
      assert.equal(
        imp.module,
        'wasi_snapshot_preview1',
        `unexpected import module '${imp.module}' for '${imp.name}' — this artifact must only import WASI preview1, no sockets/threads/poll`,
      );
      assert.ok(
        ALLOWED_WASI_IMPORTS.has(imp.name),
        `import '${imp.name}' is not in the frozen allowlist — ` +
          'either a dependency bump grew the syscall surface (investigate), ' +
          'or this is a genuine, expected new import (update ALLOWED_WASI_IMPORTS ' +
          'to match reality, per docs/WASM_CLIENT_PLAN.md §7 C2)',
      );
    }
  });

  it('exports the full h3c_*/qc_* ABI table plus wasm_alloc/wasm_free', () => {
    const exportNames = new Set(WebAssembly.Module.exports(mod).map((e) => e.name));
    const missing = EXPECTED_EXPORTS.filter((name) => !exportNames.has(name));
    assert.deepEqual(missing, [], `missing expected exports: ${missing.join(', ')}`);
  });

  it('exports the full hs_*/qs_* server ABI table', () => {
    const exportNames = new Set(WebAssembly.Module.exports(mod).map((e) => e.name));
    const missing = EXPECTED_SERVER_EXPORTS.filter((name) => !exportNames.has(name));
    assert.deepEqual(missing, [], `missing expected server exports: ${missing.join(', ')}`);
  });

  it('exports a linear memory', () => {
    const memoryExport = WebAssembly.Module.exports(mod).find((e) => e.name === 'memory');
    assert.ok(memoryExport, 'expected a "memory" export');
    assert.equal(memoryExport?.kind, 'memory');
  });
});
