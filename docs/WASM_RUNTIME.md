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

### Workers / `workerd` status

**Current reality: still not deployable for real traffic.** The protocol
core and its WASI shim are host-agnostic by design (the same `.wasm`
module and shim code are meant to run under a future workerd adapter with
zero changes), but Workers has no outbound UDP client socket API today,
and `node:dgram` under `nodejs_compat` is an import-compatible stub that
silently drops sends rather than a working transport. Track the platform
request at
[cloudflare/workerd discussion #4463](https://github.com/cloudflare/workerd/discussions/4463)
(this repo's own request for a minimal outbound-UDP-client surface is
recorded there — see the draft template at the end of this section if you
want to actually file it). [WASM_CLIENT_PLAN.md §9](./WASM_CLIENT_PLAN.md#9-workerd-readiness-workstream-e--design-now-deploy-later)
has the full constraints table and the design decisions already made to
accommodate it (frozen clock, no local address control, isolate memory
budget, etc.).

#### What Phase 5 built

- **`lib/wasm/index.workerd.ts`** — a thin, host-agnostic entry point
  re-exporting the pieces a workerd host needs: `loadHttp3WasmCore`,
  `WasmH3ClientEventLoop`/`WasmQuicClientEventLoop`, the `DatagramTransport`
  interface, and `WasmEvent`/option types. It also exports
  `createUnavailableDatagramTransport()` — a `DatagramTransport` that
  throws a descriptive error from every method the instant it's used,
  for wiring things up *before* a real transport exists without silently
  no-op-ing (mirroring the plan's warning that `node:dgram`'s mere
  presence must never be trusted as a sign UDP works). It is **not** a
  working transport and is not meant to become one.
- **`WasmH3ClientEventLoop`/`WasmQuicClientEventLoop` now take an
  already-instantiated core and a required `transportFactory`**, rather
  than a Node file path and an implicit `node:dgram` default (how Phase 3
  originally shipped them). This was a real, necessary fix, not a
  cosmetic one: the Phase 3 shape hard-imported `node:fs`/`node:dgram`
  from inside those classes, which made them impossible to compile —
  let alone use — under a tsconfig without `@types/node`. `lib/client.ts`/
  `lib/quic-client.ts` now build the Node-specific pieces
  (`lib/wasm/node-core-loader.ts`, `lib/wasm/node-udp-adapter.ts`)
  themselves and hand them in; a workerd host does the equivalent with
  its own precompiled module and transport.
- **`tsconfig.workerd.json`** — compiles all of `lib/wasm/**` (minus the
  two files that are Node-only by design: `node-core-loader.ts`,
  `node-udp-adapter.ts`, plus the Node-facing `index.ts` aggregate) using
  `@cloudflare/workers-types` instead of `@types/node`. This surfaced real
  type friction beyond what the plan anticipated — not just `Buffer`, but
  `NodeJS.Timeout` (fixed via `ReturnType<typeof setTimeout>`, which
  correctly resolves to whichever type each tsconfig's ambient `setTimeout`
  declares) and `WebAssembly.Module`'s constructor (`workers-types`
  correctly declares it `abstract` with no public constructor, since
  workerd genuinely can't compile-from-bytes — worked around with a
  narrow, documented local type for the one branch Node's real
  implementation supports). The event/data payload contract
  (`WasmEvent.data`, keylog lines) is `Uint8Array`, not `Buffer`, throughout
  `lib/wasm/**` now; `lib/client.ts`/`lib/quic-client.ts` wrap every
  payload in a real `Buffer.from(...)` at the Node integration boundary
  (`lib/wasm-event-bridge.ts`), so every existing, documented, public
  Buffer-typed API (`'sessionTicket'`/`'datagram'` events, stream `'data'`
  chunks, `'keylog'`) is completely unaffected — verified by the full
  existing wasm test suite (21/21) and native suite (362/362) staying
  green through this refactor.
- **The `"./wasm"` package export** (deferred from Phase 4 — see the
  decision log in WASM_CLIENT_PLAN.md §12): now added, with **per-condition
  nested `types`**, not a single flat `types` key:
  ```json
  "./wasm": {
    "workerd": { "types": "./dist/wasm/index.workerd.d.ts", "default": "./dist/wasm/index.workerd.js" },
    "worker":  { "types": "./dist/wasm/index.workerd.d.ts", "default": "./dist/wasm/index.workerd.js" },
    "types": "./dist/wasm/index.d.ts",
    "default": "./dist/wasm/index.js"
  }
  ```
  A flat top-level `types` (the plan's originally-sketched shape) would
  have been exactly the workers-sdk#2805-class shadowing bug the plan
  warns about: TypeScript always matches a `types` condition when doing
  type resolution, so a flat key would "win" regardless of which runtime
  condition (`workerd`/`worker`/`default`) actually matched, silently
  showing Node-only exports (`connectNodeUdp`, `loadHttp3WasmCoreFromFile`)
  to a workerd consumer even though the *runtime* JS resolved correctly.
  Caught and fixed during this phase, not just theorized about — see the
  verification note below. `dist/wasm/index.js` (a new, small,
  Node-facing aggregate, `lib/wasm/index.ts`) is what the plain `default`
  condition points at; most Node consumers still don't need it directly
  (`connect()`/`connectAsync()`/`connectQuic()`/`connectQuicAsync()` with
  `runtimeMode: 'wasm'` remains the supported way in).
- **`examples/workerd-client/`** — a real Worker + `wrangler.jsonc`,
  importing `@currentspace/http3/wasm` through a genuine
  `workspace:*` dependency (not a relative path into the repo). Doubles as
  the C7 smoke harness. See its own README for exactly what was run and
  what it proved.

#### What the C7 smoke test actually proved (and didn't)

Run for real, repeatedly, on this machine, under both `wrangler dev` and
`wrangler deploy --dry-run` (full transcript in
`examples/workerd-client/README.md`):

- The compiled `dist/wasm/http3_client.wasm` artifact instantiates
  correctly under a real `workerd` V8 isolate via this package's own WASI
  shim — no `node:wasi`, no runtime WASI dependency at all.
- The full expected `h3c_*`/`qc_*` export surface resolves.
- `WasmH3ClientEventLoop` — the identical class the Node runtime path
  uses — constructs and drives `connect()` against a purely in-memory mock
  `DatagramTransport` (no sockets), producing a real, well-formed
  1200-byte QUIC v1 Initial packet. That exercises the WASI shim's
  `crypto.getRandomValues`/`performance.now()` bindings, `TextEncoder`/
  `TextDecoder`, the `queueMicrotask`-deferred dispatch, and the
  single-`setTimeout` pump/close discipline — all running correctly
  inside workerd's own engine, not just Node's.
- This worked with **`compatibility_flags: []`** — no `nodejs_compat` at
  all — which is real (if narrow) positive signal that this specific code
  path doesn't depend on nodejs_compat fidelity to begin with (C15
  remains open for anything that *does* need Node polyfills; this simply
  isn't one of those things).
- The bundle size: 1542.56 KiB / 630.20 KiB gzip (`wrangler deploy
  --dry-run`), comfortably under the free-tier 3 MB gzip bundle limit —
  consistent with the Phase 2 measurement of the artifact alone.
- The `"./wasm"` export's condition-based resolution was verified two
  ways: through Wrangler/esbuild for real (the example above), and
  independently via `node --conditions=workerd -e
  "require.resolve('@currentspace/http3/wasm')"` (resolves to
  `dist/wasm/index.workerd.js`) and a `customConditions: ["workerd"]`
  tsconfig (correctly refuses to resolve `connectNodeUdp` — a
  Node-only export — against `index.workerd.d.ts`).

**What it did not prove:** any real network I/O (there is none to test —
see C1 above), and nothing about `nodejs_compat` fidelity for
`node:stream`/`node:events`/`Buffer` (C15) — this code path is
constructed specifically to avoid needing any of that, so it has nothing
to say about consumers who *do* need it. No real `wrangler deploy` to a
Cloudflare account was performed (out of scope; `--dry-run` already
covers everything short of the missing UDP piece).

#### What remains before real deployment is possible

1. **Workerd (or `cloudflare:sockets`) ships an outbound UDP client API.**
   This is the entire blocker — everything else in this section is
   already in place and waiting for it.
2. **A real `DatagramTransport` implementation** against whatever shape
   that API takes, following `lib/wasm/node-udp-adapter.ts`'s pattern (a
   second file implementing the same host-agnostic interface, not a
   rewrite of the protocol core) — **with a capability probe that
   round-trips a real packet before trusting the transport**, exactly as
   docs/WASM_CLIENT_PLAN.md §6.5 specifies: never trust a module's mere
   presence.
3. **C15 (nodejs_compat fidelity) verification** for any consumer that
   layers `node:stream`/`node:events`/`Buffer`-dependent code on top
   (this package's own `"./wasm"` entry point doesn't need any of that,
   but a real application built on it plausibly will).
4. Re-running this phase's C7 smoke test — or a fuller version of it —
   once a real transport exists, to drive an actual handshake end to end.

#### Draft tracking issue (not filed — copy-paste this yourself)

Per this project's own convention (see the Phase 2 quiche-patch-upstreaming
note in WASM_CLIENT_PLAN.md §12), no agent creates real external GitHub
artifacts on your behalf. The `cloudflare-udp-client-discussion-comment.md`
file at the repo root already has this repo's drafted comment for
cloudflare/workerd discussion #4463. If you also want an **internal
tracking issue** in this repo (to watch #4463 and revisit the adapter once
UDP ships), here is a ready-to-file draft:

> **Title:** Track cloudflare/workerd#4463 (outbound UDP) for the wasm
> client's workerd `DatagramTransport`
>
> **Body:**
>
> This repo's wasm-backed HTTP/3 + raw QUIC client
> (docs/WASM_CLIENT_PLAN.md) is designed to run under Cloudflare Workers
> with zero changes to the protocol core once workerd ships an outbound
> UDP client socket API. Everything on our side is ready and smoke-tested
> (`lib/wasm/index.workerd.ts`, the `"./wasm"` package export,
> `examples/workerd-client/`) except the actual transport, which cannot
> exist until the platform ships one.
>
> - Upstream tracking: <https://github.com/cloudflare/workerd/discussions/4463>
> - This repo's request comment: `cloudflare-udp-client-discussion-comment.md`
> - When #4463 lands (or a `cloudflare:sockets`-style datagram API ships):
>   1. Implement a new `lib/wasm/workerd-udp-adapter.ts` (or similar) —
>      a second `DatagramTransport` implementation, mirroring
>      `lib/wasm/node-udp-adapter.ts`'s shape — **with a capability probe
>      that round-trips a real packet before trusting the transport**
>      (see docs/WASM_CLIENT_PLAN.md §6.5: never trust a `node:dgram`- or
>      similarly-shaped module's mere presence as a sign UDP actually
>      works).
>   2. Extend `examples/workerd-client/` (or add a new example) to drive a
>      real handshake against a real server, replacing the current mock
>      transport.
>   3. Revisit C15 (nodejs_compat fidelity for `node:stream`/`node:events`/
>      `Buffer`) once a real application-shaped consumer exists to test
>      against.
>   4. Update docs/WASM_RUNTIME.md's status from "not yet deployable" to
>      real usage instructions.

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
