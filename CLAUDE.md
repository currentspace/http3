# CLAUDE.md

## Package Manager

**pnpm only.** Do not use npm or yarn.

## Quick Reference

```bash
pnpm install                              # install deps
pnpm run build                            # napi release + tsc
pnpm run build:debug                      # napi debug + tsc
pnpm test                                 # full TS test suite
pnpm run test:rust:full                   # full Rust test suite
```

## Build

The project has two build outputs: a Rust NAPI native module (`.node` binary) and TypeScript compiled to JS.

```bash
npx napi build --platform --release       # Rust -> *.node (release)
npx napi build --platform                 # Rust -> *.node (debug)
pnpm run build:dist                       # lib/ -> dist/ (production TS)
pnpm run build:test                       # test/ -> dist-test/ (test TS)
```

Always rebuild the NAPI module after changing Rust code. Always rebuild test TS after changing test files.

### WASM client build (experimental, Phase 2 of docs/WASM_CLIENT_PLAN.md)

```bash
export WASI_SDK_PATH=/path/to/wasi-sdk-33   # required; never assumed/downloaded by any script
pnpm run build:bssl-wasi                    # stage BoringSSL for wasm32-wasip1 (cached)
pnpm run build:wasm                         # -> dist/wasm/http3_client.wasm
HTTP3_WASM=1 pnpm run test:wasm             # import/export allowlist check (C2)
```

`WASI_SDK_PATH` must point at a local wasi-sdk 33 install (`share/cmake/wasi-sdk-p1.cmake`
must exist under it) — it is machine-specific, so no script hardcodes or downloads it.
`build:wasm` also derives a few other wasm32-wasip1-only env vars
(`BORING_BSSL_*_wasm32_wasip1`, `BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1`,
`CARGO_TARGET_WASM32_WASIP1_RUSTFLAGS`) itself, inside `scripts/build-wasm.mjs`
— never by hand-prefixing a cargo command. Rebuild the wasm artifact after
changing `crates/http3-wasm` or any always-compiled part of `src/`
(`h3_event.rs`, `config.rs`, `worker.rs`'s `H3ClientHandler`, etc.) the same
way you'd rebuild the native `.node` addon after other Rust changes. Full
usage docs (Node + Workers/workerd status) live in `docs/WASM_RUNTIME.md`;
see `docs/WASM_CLIENT_PLAN.md` for the design.

Reachable from Node via `runtimeMode: 'wasm'` on `connect`/`connectAsync`/
`connectQuic`/`connectQuicAsync`, and via the package's `"./wasm"` subpath
export (`@currentspace/http3/wasm`) for lower-level/workerd use —
`lib/wasm/index.ts` (Node) and `lib/wasm/index.workerd.ts` (workerd,
tested against real `wrangler dev`/`wrangler deploy --dry-run` — see
`examples/workerd-client/`) are the two entry points, selected via
`package.json`'s per-condition `workerd`/`worker`/`default` exports.

## Lint & Typecheck

```bash
pnpm run lint                             # eslint lib/ test/
pnpm run typecheck                        # tsc --noEmit on both tsconfigs
pnpm run typecheck:workerd                # tsc -p tsconfig.workerd.json --noEmit (lib/wasm/** without @types/node)
```

ESLint enforces `no-floating-promises`, `explicit-function-return-type`, and `promise-function-async`. Test files get relaxed unsafe rules.

`tsconfig.workerd.json` type-checks `lib/wasm/**` (minus the two
Node-only files, `node-udp-adapter.ts`/`node-core-loader.ts`, plus the
Node-facing `index.ts` aggregate) using `@cloudflare/workers-types`
instead of `@types/node` — it's how a real `Buffer`/`node:fs`/`node:dgram`
dependency accidentally creeping into that directory gets caught at
typecheck time instead of at "someone tries to use it from workerd" time.
Registered in `eslint.config.mjs`'s `parserOptions.project` alongside the
other two tsconfigs.

## Testing

### Rust

```bash
pnpm run test:rust:unit                   # cargo test --lib --no-default-features
pnpm run test:rust:mock                   # quic_mock_pair + h3_mock_pair
pnpm run test:rust:mock:extended          # all 11 integration test files
pnpm run test:rust:full                   # unit + interop + extended
pnpm run test:rust:stress                 # 5-min stress (--ignored flag)
pnpm run test:rust:coverage:report        # llvm-cov text report
```

Integration tests require `--features bench-internals --no-default-features`. The `test:rust:mock:extended` script handles this.

### TypeScript

```bash
pnpm test                                 # core + runtime + interop + release + ffi
pnpm run test:core                        # core API tests
pnpm run test:ffi                         # FFI boundary tests
pnpm run test:ffi:h3                      # H3 event coverage + stress
pnpm run test:interop                     # all interop tests
pnpm run test:interop:h3                  # H3 loopback + edge cases
pnpm run test:runtime                     # runtime selection + driver pressure
pnpm run test:coverage                    # c8 coverage (core + ffi)
```

All TS tests use Node.js built-in `node:test` runner with
`--test-isolation=none --test-timeout=15000`. Single-process isolation
keeps the full suite under 15 s on a developer machine; the 15 s
per-test cap surfaces hangs quickly. Long-running stress and longhaul
suites have their own scripts with looser timeouts.

### Long-running Tests

```bash
HTTP3_LONGHAUL=1 pnpm run test:longhaul   # 5-min sustained H3 + QUIC (700s timeout)
pnpm run test:rust:stress:all             # all Rust 5-min stress tests
pnpm run test:browser:e2e                 # Playwright (needs HTTP3_BROWSER_E2E=1)
```

### Verification Sequence (before commit)

```bash
# Or just run the canonical script: `pnpm verify` (see scripts/verify.sh).
cargo test --lib --no-default-features && \
pnpm run test:rust:mock:extended && \
npx napi build --platform --release && \
pnpm run build:test && pnpm run build:dist && \
pnpm test
```

## Architecture

```
JS (lib/)  ──NAPI──>  Rust worker threads (src/)  ──quiche──>  UDP
   │                       │                            │
   ├─ server.ts            ├─ worker.rs (H3)            ├─ io_uring (Linux fast)
   ├─ client.ts            ├─ quic_worker.rs (QUIC)     ├─ poll (Linux portable)
   ├─ quic-server.ts       ├─ event_loop.rs             └─ kqueue (macOS)
   ├─ quic-client.ts       ├─ connection.rs (H3)
   ├─ stream.ts            ├─ quic_connection.rs
   └─ event-loop.ts        ├─ chunk_pool.rs (outbound)
                           ├─ buffer_pool.rs (inbound)
                           └─ h3_event.rs (TSFN events)
```

- **JS → Rust**: Commands via crossbeam channel (StreamSend, ResponseHeaders, CloseSession, etc.)
- **Rust → JS**: Events via NAPI ThreadsafeFunction in batches (Vec<JsH3Event>)
- **Buffers**: RecyclableBuffer (inbound, napi_create_external_buffer + finalize callback) and Chunk (outbound, pool return on drop) — both recycle to per-worker pools via crossbeam channels

## Key Source Files

| File | Purpose |
|------|---------|
| `src/event_loop.rs` | Generic event loop driving all worker types |
| `src/worker.rs` | H3 server/client protocol handlers |
| `src/quic_worker.rs` | Raw QUIC server/client protocol handlers |
| `src/h3_event.rs` | Event types + RecyclableBuffer FFI type |
| `src/chunk_pool.rs` | Outbound payload chunk pool (1KB/2KB/4KB classes) |
| `src/buffer_pool.rs` | Inbound recv buffer pool with cross-thread recycler |
| `src/connection.rs` | H3 connection (wraps quiche::h3::Connection) |
| `src/quic_connection.rs` | Raw QUIC connection (wraps quiche::Connection) |
| `src/transport/` | Platform I/O drivers (io_uring, poll, kqueue, mock) |
| `lib/event-loop.ts` | WorkerEventLoop — TSFN management and event dispatch |
| `lib/server.ts` | Http3SecureServer — public H3 server API |
| `lib/client.ts` | Http3ClientSession — public H3 client API |
| `test/support/native-test-helpers.ts` | FFI test utilities (createQuicPair, createH3Pair, EventCollector) |
| `crates/http3-wasm/src/{h3,quic}.rs` | `h3c_*`/`qc_*` extern-C ABI exports (wasm32-wasip1 client-only build) |
| `lib/wasm/index.workerd.ts` | Workers/workerd entry point (`"./wasm"` export's `workerd`/`worker` conditions); no `@types/node`, no Buffer, no `node:*` |
| `lib/wasm-event-bridge.ts` | Node-only: wraps wasm `Uint8Array` event payloads in real `Buffer`s at the `lib/client.ts`/`lib/quic-client.ts` boundary |
| `scripts/build-wasm.mjs` | Orchestrates `pnpm run build:wasm` (bssl + quiche patch + cargo + wasm-opt) |
| `scripts/build-bssl-wasi.sh` | Builds BoringSSL for wasm32-wasip1 from boring-sys's vendored source |
| `scripts/prepare-quiche-wasm-patch.sh` | Vendors + patches quiche's wasm FFI fix into `target/quiche-wasm-patched/` |

## Conventions

### Must Follow

- **pnpm only** — no npm/yarn
- **Never revert/checkout/reset** to older code — always fix forward
- **Never prefix commands with env vars** — use `.cargo/config.toml` `[env]` section
- **Fix root causes** before reaching for workarounds (allocator switches, GC hints)
- **`runtimeMode: 'portable'`** in all tests (avoids io_uring ring exhaustion)
- **Rebuild after Rust changes**: `npx napi build --platform --release` before running TS tests

### Code Style

- Rust: edition 2024, clippy pedantic enabled with domain-specific allows
- TypeScript: strict mode, ES2022 target, Node16 module resolution
- Tests: `node:test` (describe/it), `assert` or `assert/strict`, always cleanup in `after()` or `try/finally`
- FFI tests: import from `../support/native-test-helpers.js` (note `.js` extension)
- FFI tests: `after(() => { setTimeout(() => process.exit(0), 500).unref(); })` for TSFN cleanup

### Environment Variables for Tests

| Variable | Purpose |
|----------|---------|
| `HTTP3_LONGHAUL=1` | Enable 5-min longhaul tests |
| `HTTP3_BROWSER_E2E=1` | Enable Playwright browser tests |
| `HTTP3_WASM=1` | Enable wasm artifact tests (`test:wasm`); requires `dist/wasm/http3_client.wasm` (`pnpm run build:wasm`) |
| `MIMALLOC_PURGE_DELAY=0` | Aggressive RSS reclaim with mimalloc |

## Docker

```bash
pnpm run docker:build                    # build image
pnpm run docker:up                       # start on :8443 (H3+H2) + :8080 (health)
pnpm run docker:down                     # stop
```

## Driver Tracing

Enable compile-time tracing in the io_uring/poll/kqueue transport drivers:

```bash
cargo test --lib --features driver-tracing -- iouring  # trace io_uring driver
RUST_LOG=trace cargo run --features driver-tracing     # trace in production binary
```

The `driver-tracing` feature compiles to zero cost when disabled (default). It adds `log::trace!` calls at key points: RX buffer ring state, multishot arm/disarm, CQE processing, buffer exhaustion warnings.

**Buffer exhaustion warnings** (`log::warn!`) are always enabled regardless of feature flag — they indicate the kernel is silently dropping datagrams.

## Performance Profiling

```bash
pnpm run perf:linux:quic                  # perf record + flamegraph (Linux)
pnpm run perf:escalation                  # gradual load increase
pnpm run perf:analyze                     # analyze perf results
node --expose-gc scripts/heap-profile-h3-sustained.mjs results/  # heap snapshots
```

Results go in `results/` (gitignored).
