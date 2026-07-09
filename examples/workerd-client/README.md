# workerd client example / C7 smoke harness

Status: **instantiation + export-shape + in-memory-mock-transport smoke test,
real and passing under `wrangler dev` and `wrangler deploy --dry-run` on this
machine.** Not a real network handshake — workerd has no outbound UDP client
socket API yet. See [docs/WASM_RUNTIME.md](../../docs/WASM_RUNTIME.md)'s
"Workers / workerd status" section for the full picture, and
[docs/WASM_CLIENT_PLAN.md §9](../../docs/WASM_CLIENT_PLAN.md#9-workerd-readiness-workstream-e--design-now-deploy-later)
for the design constraints this fixture exercises.

## What this is

`worker.ts` is a real Cloudflare Worker that imports this package's `"./wasm"`
export (`@currentspace/http3/wasm`, resolved through a real `workspace:*`
dependency — see `package.json` — exactly like an npm-installed consumer would
resolve it) and:

1. Loads the precompiled `dist/wasm/http3_client.wasm` module (built by
   `pnpm run build:wasm` at the repo root) the same way Wrangler bundles any
   `.wasm` dependency — as a precompiled `WebAssembly.Module`, never
   compiled from raw bytes at runtime.
2. Instantiates it via `loadHttp3WasmCore` and checks the expected `h3c_*`/
   `qc_*` exports resolve.
3. Constructs `WasmH3ClientEventLoop` (the exact same class
   `lib/client.ts` uses on Node) against a purely in-memory mock
   `DatagramTransport` (no sockets at all) and calls `connect()`.
4. Asserts the mock transport captured a real, well-formed QUIC Initial
   packet (1200 bytes, long-header form).

Every request runs this whole sequence fresh and returns a JSON summary.

## What was actually verified (this machine, this session)

```
$ wrangler dev --port 18789 --local-protocol http
 ⛅️ wrangler 4.107.1
⎔ Starting local server...
[wrangler:info] Ready on http://localhost:18789

$ curl -s http://localhost:18789/
{
  "ok": true,
  "results": [
    { "step": "instantiate", "ok": true, "detail": "WebAssembly.Instance created; exported linear memory present" },
    { "step": "exports", "ok": true, "detail": "all 9 sampled h3c_*/qc_* exports resolved" },
    { "step": "connect-generates-initial-packet", "ok": true, "detail": "captured 1 outbound datagram(s); first packet 1200 bytes, leading byte 0xc4" }
  ]
}
```

Repeated several times back to back (leading byte varies run to run — it
encodes the packet-number length quiche picked, which depends on fresh
per-connection randomness; every observed value was a valid QUIC v1 long-header
Initial-packet first byte). Each request took ~2 s wall-clock: `connect()`
never receives a real response (no peer exists), and the mock transport
factory here does not resolve to a real network any faster, so `close()`
correctly falls through to its bounded ~2 s drain-deadline fallback rather
than hanging — itself a real, useful data point (the bounded-teardown code
path runs correctly under workerd's timers too).

```
$ wrangler deploy --dry-run
 ⛅️ wrangler 4.107.1
Total Upload: 1542.56 KiB / gzip: 630.20 KiB
No bindings found.
--dry-run: exiting now.
```

Comfortably under the Workers free-tier 3 MB (gzip) bundle limit, let alone
the 10 MB paid limit (docs/WASM_CLIENT_PLAN.md §9 C8).

`wrangler.jsonc` deliberately sets `"compatibility_flags": []` — no
`nodejs_compat` — and it still works: this package's `"./wasm"` entry point
and everything it depends on (`lib/wasm/**`, minus the two Node-only files
excluded from it) is host-agnostic by construction (uses only
`WebAssembly`, `crypto.getRandomValues`, `performance.now()`,
`TextEncoder`/`TextDecoder`, `setTimeout`/`queueMicrotask` — all real
Workers runtime globals, none of them `nodejs_compat` polyfills). That is
real, if narrow, positive signal on C15 (nodejs_compat fidelity is
otherwise unverified) — for *this* code path specifically, it doesn't even
need nodejs_compat to begin with.

The package resolution itself was also verified independently of Wrangler,
using Node's own conditional-exports algorithm and TypeScript's
`customConditions`:

```
$ node --conditions=workerd -e "console.log(require.resolve('@currentspace/http3/wasm'))"
/…/dist/wasm/index.workerd.js
```

— and with a `customConditions: ["workerd"]` tsconfig, `dist/wasm/index.d.ts`'s
Node-only exports (`connectNodeUdp`, `loadHttp3WasmCoreFromFile`) correctly
fail to resolve against `dist/wasm/index.workerd.d.ts`, proving the nested
per-condition `types` in `package.json`'s `"./wasm"` export avoids the
`workers-sdk#2805`-class shadowing bug the plan warns about (a flat,
single top-level `"types"` key would have "won" regardless of which
runtime condition matched — checked and fixed during this phase, not just
theorized about).

## What this does NOT prove

- **No real network handshake.** There is no outbound UDP client socket API
  in workerd today (tracked at
  [cloudflare/workerd#4463](https://github.com/cloudflare/workerd/discussions/4463)).
  The mock transport here never sends a byte anywhere; `connect()` never
  receives a real Server Hello, so no full handshake ever completes. A
  from-scratch offline "replay" of a captured handshake isn't practical
  either: quiche generates a fresh random SCID and X25519 keypair per
  connection (by design — that's real security-relevant randomness this
  library correctly never lets you pin down), so a pre-recorded server
  response encrypted for a different client's ephemeral keys won't decrypt
  against this run's.
- **No `nodejs_compat` surface tested.** `dist/wasm/index.d.ts` (the
  Node-facing entry, with `connectNodeUdp`/`loadHttp3WasmCoreFromFile`) is
  intentionally not reachable here — see the shadowing-bug check above for
  why that's the *point*, but it does mean this fixture has nothing to say
  about `node:stream`/`node:events`/`Buffer` fidelity under
  `nodejs_compat` (C15 remains open for anything that *does* need it).
- **Not a deployed, Cloudflare-hosted worker.** Only exercised locally
  (`wrangler dev`, `wrangler deploy --dry-run`) — a real `wrangler deploy`
  to a Cloudflare account was intentionally never run (out of scope for
  this repo/session, and would not exercise anything `--dry-run` doesn't
  already cover, since the missing piece is outbound UDP either way).

## Running it yourself

```bash
# From the repo root: build the wasm artifact first (needs WASI_SDK_PATH).
export WASI_SDK_PATH=/path/to/wasi-sdk-33
pnpm run build:wasm
pnpm run build:dist   # produces dist/wasm/index.workerd.js / index.d.ts

# Then, from this directory:
cd examples/workerd-client
pnpm install           # symlinks @currentspace/http3 -> the repo root (workspace:*)
npx wrangler dev       # or: npx wrangler deploy --dry-run
```
