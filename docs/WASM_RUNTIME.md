# WASM Client Runtime

`@currentspace/http3` can run its HTTP/3 and raw QUIC **client** on top of a
WebAssembly build of the same protocol core the native N-API binding uses,
selected with `runtimeMode: 'wasm'`. This is a usage guide. For the full
design rationale, the rejected alternatives (napi-rs wasm, wasm-bindgen,
quinn-proto), and the ABI/architecture internals, see
[WASM_CLIENT_PLAN.md](./WASM_CLIENT_PLAN.md) — this document does not repeat
that material.

## What this is (and isn't)

- It is a `wasm32-wasip1` build of **the same** `H3ClientHandler` /
  `QuicClientHandler` protocol state machines (quiche + BoringSSL) that the
  native binding drives — not a reimplementation, and not a different
  transport with similar behavior. Event types, error categories, and
  backpressure semantics are the same contract described in
  [API_CONTRACT.md](./API_CONTRACT.md).
- It runs **today**, in Node.js, driven over a `node:dgram` socket adapter.
- It is **designed for, but not yet deployable in, Cloudflare Workers**
  (`workerd`): the protocol core and its WASI shim are host-agnostic by
  construction, but Workers has no outbound UDP client socket API yet. See
  [Workers / workerd status](#workers--workerd-status-not-yet-deployed)
  below.
- It is **client-only**. Servers (`createSecureServer`, `createQuicServer`)
  always run on the native binding; passing `runtimeMode: 'wasm'` to either
  throws `ERR_HTTP3_RUNTIME_UNSUPPORTED` immediately (N1 in the plan).
- It is **opt-in only**. `runtimeMode: 'auto'` (the default) never selects
  `'wasm'` — auto-selection only ever chooses between the native `'fast'`
  and `'portable'` drivers described in
  [RUNTIME_MODES.md](./RUNTIME_MODES.md). You must ask for `'wasm'`
  explicitly, and `fallbackPolicy` has no effect on it: if construction
  fails (e.g. the compiled artifact is missing), the connect call rejects
  with that error — it never silently falls back to the native binding.

## Prerequisites: building the artifact

Building the wasm artifact requires a local **wasi-sdk 33** install. It is
never downloaded or assumed by any script — you point `WASI_SDK_PATH` at
your own install:

```bash
export WASI_SDK_PATH=/path/to/wasi-sdk-33   # must contain share/cmake/wasi-sdk-p1.cmake
pnpm run build:wasm                         # -> dist/wasm/http3_client.wasm
```

If `WASI_SDK_PATH` is unset, `build:wasm` fails immediately with (verified
by running it without the variable set):

```
build-wasm: error: WASI_SDK_PATH is not set. Point it at a wasi-sdk 33 install
(share/cmake/wasi-sdk-p1.cmake must exist under it). Never hardcoded — see
scripts/build-bssl-wasi.sh.
```

`pnpm run build:wasm` (`scripts/build-wasm.mjs`) runs the whole pipeline in
one step — you do not need to run anything else first:

1. Stages BoringSSL for `wasm32-wasip1` (`pnpm run build:bssl-wasi` /
   `scripts/build-bssl-wasi.sh`), cached under `target/bssl-wasi/<boring-sys
   version>/` — a no-op on repeat runs unless the pinned `boring-sys`
   version changes. `build:bssl-wasi` is exposed as its own script only so
   CI (or you) can warm/diagnose that step in isolation; it is not a
   required separate manual step.
2. Vendors and applies the 2-line quiche wasm-FFI patch
   (`scripts/prepare-quiche-wasm-patch.sh`) into a git-ignored
   `target/quiche-wasm-patched/<version>/` directory — scoped to this one
   build invocation via a `--config patch.crates-io.quiche.path=...`
   override, never written into the committed `Cargo.toml` (a plain `cargo
   build --release` is unaffected whether or not this has ever run).
3. Cross-compiles `crates/http3-wasm` for `wasm32-wasip1` in release mode.
4. Runs `wasm-opt -Oz --strip-debug` (skipped with a warning, copying the
   unoptimized build instead, if `wasm-opt`/binaryen isn't on `PATH`) and
   copies the result to `dist/wasm/http3_client.wasm`.

Real output from a from-scratch run on this machine (macOS arm64):

```
build-wasm: staging BoringSSL for wasm32-wasip1 (scripts/build-bssl-wasi.sh)...
build-wasm: bssl staged at .../target/bssl-wasi/4.22.0
build-wasm: preparing patched quiche source (scripts/prepare-quiche-wasm-patch.sh)...
build-wasm: patched quiche at .../target/quiche-wasm-patched/0.29.2
build-wasm: cross-compiling crates/http3-wasm for wasm32-wasip1 (this can take a while on a cold cache)...
build-wasm: built .../target/wasm32-wasip1/release/http3_wasm.wasm (10.00 MiB pre-opt)
build-wasm: running wasm-opt -Oz --strip-debug...
build-wasm: wasm-opt: 10.00 MiB -> 1.47 MiB
build-wasm: ready: .../dist/wasm/http3_client.wasm (1503 KiB)
```

Measured artifact size on this build: 1.47 MiB raw, ~618 KiB gzip-9 —
comfortably under the 10 MB paid-Workers bundle limit referenced in the plan
(§9 C8), if/when this ships in a Worker.

`dist/wasm/http3_client.wasm` is produced by this separate cargo/wasm-opt
pipeline, not by `tsc`. See
[Troubleshooting](#the-wasm-artifact-goes-missing-after-a-plain-build) for
what that implies about build ordering.

Rebuild the artifact any time you change `crates/http3-wasm` or any
always-compiled part of `src/` that the wasm build shares with native
(`h3_event.rs`, `config.rs`, the `H3ClientHandler`/`QuicClientHandler`
methods, etc.) — the same rule CLAUDE.md states for rebuilding the native
`.node` addon after other Rust changes.

## Using it from application code

The wasm runtime is not a separate import — it's the same `connect` /
`connectAsync` / `connectQuic` / `connectQuicAsync` functions you already
use, with `runtimeMode: 'wasm'` in the options. Internally,
`lib/client.ts`/`lib/quic-client.ts` lazily `import()` the `lib/wasm/**`
runtime only when `'wasm'` is actually requested, so native-only consumers
never load any wasm code.

Both examples below were **actually run** against this repo's own native
server (`createSecureServer`/`createQuicServer`) on this machine, self-signed
cert included; real output is included beneath each one (see the note after
both examples for the one cosmetic substitution made to run them from
outside the package directory).

### HTTP/3 client

```ts
import { readFileSync } from 'node:fs';
import { createSecureServer, connectAsync } from '@currentspace/http3';

// Any HTTP/3 server works here — this one is this repo's own, native, for
// a fully self-contained example. The wasm runtime is client-only, so the
// server side is always native regardless of the client's runtimeMode.
const server = createSecureServer({
  key: readFileSync('server.key'),
  cert: readFileSync('server.crt'),
  disableRetry: true,
});

server.on('stream', (stream, headers) => {
  stream.respond({ ':status': '200', 'content-type': 'text/plain' });
  stream.end(`hello from HTTP/3, you asked for ${String(headers[':path'])}\n`);
});

await new Promise<void>((resolve) => {
  server.once('listening', () => resolve());
  server.listen(4433, '127.0.0.1');
});

// The only line that differs from the native quickstart: runtimeMode: 'wasm'.
const session = await connectAsync('127.0.0.1:4433', {
  servername: 'localhost',
  rejectUnauthorized: false, // self-signed cert for this example
  runtimeMode: 'wasm',
  fallbackPolicy: 'error', // fail loudly if the artifact/toolchain is missing
});

console.log(session.runtimeInfo); // { selectedMode: 'wasm', driver: 'wasm', ... }

const stream = session.request(
  { ':method': 'GET', ':path': '/hello', ':authority': 'localhost', ':scheme': 'https' },
  { endStream: true },
);

const chunks: Buffer[] = [];
stream.on('data', (chunk: Buffer) => chunks.push(chunk));
await new Promise<void>((resolve, reject) => {
  stream.on('end', resolve);
  stream.on('error', reject);
});

console.log(Buffer.concat(chunks).toString('utf8'));
await session.close();
await server.close();
```

Real, verified output of exactly the code above (`node` can run `.ts` files
directly via type stripping; equivalent output under `tsx`/compiled JS):

```
{
  requestedMode: 'wasm',
  fallbackPolicy: 'error',
  selectedMode: 'wasm',
  driver: 'wasm',
  fallbackOccurred: false,
  reasonCode: 'requested-wasm',
  message: undefined,
  errno: undefined,
  syscall: undefined,
  warningCode: undefined,
  fastAttempt: null
}
hello from HTTP/3, you asked for /hello
```

### Raw QUIC client

```ts
import { readFileSync } from 'node:fs';
import { createQuicServer, connectQuicAsync } from '@currentspace/http3';

const server = createQuicServer({
  key: readFileSync('server.key'),
  cert: readFileSync('server.crt'),
  disableRetry: true,
});

server.on('session', (session) => {
  session.on('stream', (stream) => {
    stream.on('data', (chunk: Buffer) => stream.write(chunk)); // echo
    stream.on('end', () => stream.end());
  });
});

const addr = await server.listen(4434, '127.0.0.1');

const session = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
  servername: 'localhost',
  rejectUnauthorized: false,
  runtimeMode: 'wasm',
  fallbackPolicy: 'error',
});

const stream = session.openStream();
const received: Buffer[] = [];
stream.on('data', (chunk: Buffer) => received.push(chunk));
const done = new Promise<void>((resolve, reject) => {
  stream.on('end', resolve);
  stream.on('error', reject);
});
stream.end('hello over raw QUIC via wasm');
await done;

console.log(Buffer.concat(received).toString('utf8'));
await session.close();
await server.close();
```

Real, verified output of exactly the code above — the echoed message coming
back from the server through the wasm client's stream:

```
hello over raw QUIC via wasm
```

Both examples above are exactly what was run to verify this document (only
the `@currentspace/http3` import specifier was substituted for a direct
path to this repo's own build output, since the verification scripts live
outside the package directory tree — the executed code and its behavior are
otherwise identical). Both were re-run several times back to back with
consistent results (~0.2 s wall clock each, clean process exit, no hangs).

### Options that behave differently under `'wasm'`

Everything in [CONFIGURATION_OPTIONS.md](./CONFIGURATION_OPTIONS.md) applies
except:

| Option | Behavior under `runtimeMode: 'wasm'` |
| --- | --- |
| `qlogDir` / `qlogLevel` | Silently ignored — no qlog in the wasm build (see [Limitations](#current-limitations)). `session.exportQlog()` always returns `null` (verified). |
| `keylog` | Delivered via the `'keylog'` session event with real NSS-format key material, whether `keylog` is `true` or a string path — verified: `CLIENT_HANDSHAKE_TRAFFIC_SECRET`/`SERVER_HANDSHAKE_TRAFFIC_SECRET`/etc. arrive as real event payloads. The wasm core never writes key material to disk, though: even with a string path, that file is only ever created empty. If your tooling tails an actual `SSLKEYLOGFILE`-style file on disk rather than consuming the `'keylog'` event, it will see nothing on the wasm path — consume the event instead. |
| `cert` / `key` (H3 `connect`/`connectAsync`) | N/A on native either — the H3 client has no mTLS option in this library today, wasm included. See [Limitations](#current-limitations). |
| `cert` / `key` (`connectQuic`/`connectQuicAsync`) | Fully supported — raw QUIC client mTLS works the same as native. |
| `fallbackPolicy` | Has no effect — see [What this is](#what-this-is-and-isnt) above. |

## Current limitations

- **Client-only.** No server support, and none is planned for this
  architecture (N1). `createSecureServer`/`createQuicServer` reject
  `runtimeMode: 'wasm'` immediately.
- **No qlog.** Excluded from the wasm build entirely (N5) — the `qlog-files`
  Cargo feature (`quiche/qlog`) is not enabled when building
  `crates/http3-wasm` (`--no-default-features --features wasm-abi`).
  Keylog is available instead, delivered as events rather than a tailed
  file (see the options table above).
- **No H3 client mTLS.** This mirrors an existing native asymmetry, not a
  wasm-specific gap: the H3 `ConnectOptions` type has no `cert`/`key`
  fields on **either** runtime. Raw QUIC client mTLS (`cert`/`key` on
  `connectQuic`/`connectQuicAsync`) works on both runtimes.
- **No WebTransport, connection migration, or multipath** — a single
  connected UDP flow per session (N4); this also matches the eventual
  Workers execution model, which gives you no control over local address
  or path selection anyway.
- **Performance: wasm crypto is measurably slower than native — do not
  expect parity.** BoringSSL is built `OPENSSL_NO_ASM` for `wasm32-wasip1`
  (pure C crypto, no AES-NI), so this is intended for client/control-plane
  workloads, not bulk transfer (N3). As an informal, non-rigorous sanity
  check (macOS arm64, loopback, this machine, N=10 connects each, **not a
  benchmark**): the wasm client's self-reported `handshakeTimeMs` averaged
  roughly 4-6x the native `'portable'` driver's (~5 ms vs ~1 ms); overall
  wall-clock `connectAsync()` time — which also includes fixed per-call
  overhead common to both paths — was about 1.7-2x. Directionally
  consistent with the plan's expectation; treat the exact multiplier as
  illustrative, not a guarantee, since it will vary by machine, network
  path, and TLS cipher suite negotiated.
- **No 0-RTT/session-ticket persistence helpers for Workers KV/Durable
  Objects.** The `sessionTicket` option and `EVENT_SESSION_TICKET`/
  `'sessionTicket'` event flow through as-is (N6) — you can persist and
  supply tickets yourself; there's just no built-in storage adapter.
- **`EVENT_WRITE_READY` (18) is never emitted** by the wasm core — only
  `EVENT_DRAIN` (8) signals a previously-blocked stream draining. The
  public `Duplex`/`QuicStream` `'drain'` semantics are unaffected (they're
  driven by `EVENT_DRAIN` already); this only matters if you consume the
  raw event stream directly.

### Workers / `workerd` status: not yet deployed

The protocol core and its WASI shim are host-agnostic by design (the same
`.wasm` module and shim code are meant to run under a future workerd
adapter with zero changes), but **there is no Workers deployment today** —
only the Node.js path above is real and tested. The blocker is entirely on
the platform side: Workers has no outbound UDP client socket API, and
`node:dgram` under `nodejs_compat` is an import-compatible stub that
silently drops sends rather than a working transport.

Track the platform request at
[cloudflare/workerd discussion #4463](https://github.com/cloudflare/workerd/discussions/4463)
(this repo's own request for a minimal outbound-UDP-client surface is
recorded there). Do not build on a workerd deployment of this runtime until
that lands — [WASM_CLIENT_PLAN.md §9](./WASM_CLIENT_PLAN.md#9-workerd-readiness-workstream-e--design-now-deploy-later)
has the full constraints table and the design decisions already made to
accommodate it (frozen clock, no local address control, isolate memory
budget, etc.).

## Troubleshooting

### `build:wasm` fails with "WASI_SDK_PATH is not set"

Expected, verified behavior — you must point it at your own wasi-sdk 33
install (`share/cmake/wasi-sdk-p1.cmake` must exist under the path). No
script downloads or infers this path, by design, since it's
machine-specific.

### "Cannot find dist/wasm/http3_client.wasm"

Thrown by `connect`/`connectAsync`/`connectQuic`/`connectQuicAsync` when
`runtimeMode: 'wasm'` is requested but the artifact hasn't been built yet
(verified by actually removing the artifact and connecting):

```
Cannot find dist/wasm/http3_client.wasm. Searched:
  - <pkg>/dist/dist/wasm/http3_client.wasm
  - <pkg>/dist/wasm/http3_client.wasm
  - ...
Run `pnpm run build:wasm` first (requires WASI_SDK_PATH).
```

Fix: `pnpm run build:wasm` (with `WASI_SDK_PATH` set). This is a hard
failure, not a fallback — per the note above, `fallbackPolicy` does not
apply to `runtimeMode: 'wasm'`, so this always rejects the connect call
rather than silently continuing on native.

### The wasm artifact goes missing after a plain build

`dist/wasm/http3_client.wasm` is produced by `pnpm run build:wasm` (a
separate cargo + `wasm-opt` pipeline), not by `tsc`. `pnpm run build:dist`
(and therefore plain `pnpm run build`) only knows how to (re)write the
`.js`/`.d.ts` files under `dist/` — it has no way to regenerate the wasm
binary. If your build tooling clears `dist/` before a TypeScript rebuild,
build order matters: run `pnpm run build:wasm` **after** `pnpm run
build`/`build:dist`, not before, or re-run it again afterward if the
artifact is gone. A quick check: `ls dist/wasm/http3_client.wasm` after any
full rebuild, before relying on `runtimeMode: 'wasm'`.

### `HTTP3_WASM=1`

Gates the wasm test lane (`pnpm run test:wasm`, `test/wasm/**`) exactly
like `HTTP3_LONGHAUL`/`HTTP3_BROWSER_E2E` gate theirs. Unset, or set with no
built artifact, the suites self-skip with a clear reason (they never fail
for either condition):

```bash
HTTP3_WASM=1 pnpm run test:wasm
```

### Buffer-lifetime rule, if you're working with the wasm core directly

You are unlikely to need this at the application level — `lib/wasm/**`
already implements it — but if you ever call into `Http3WasmCore`/the raw
`h3c_*`/`qc_*` exports yourself: every buffer the core hands back a pointer
to (RX/TX staging regions, `drain_events`/`session_metrics`/
`take_keylog` output, event `dataOff`/`dataLen` payload ranges) is valid
**only until the next call on the same handle**. Copy or decode everything
you need synchronously before making another ABI call — only *dispatch* of
an already-decoded event batch may be deferred. See the crate doc comment
in `crates/http3-wasm/src/lib.rs` for the normative statement, and
[WASM_CLIENT_PLAN.md §5.3/§6.4](./WASM_CLIENT_PLAN.md#5-workstream-a--rust)
for the full ABI contract.

### Runtime selection quick reference

| You want | Set |
| --- | --- |
| Native, best driver for the platform, no fallback | `runtimeMode: 'fast'` |
| Native, portable driver (works in restricted containers) | `runtimeMode: 'portable'` |
| Native, best-effort with fallback (default) | `runtimeMode: 'auto'` (or omit) |
| The wasm client runtime | `runtimeMode: 'wasm'` (must be explicit — `'auto'` never selects it) |

See [RUNTIME_MODES.md](./RUNTIME_MODES.md) for the full native `fast`/
`portable`/`auto` semantics.
