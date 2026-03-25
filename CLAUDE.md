# CLAUDE.md

## Package Manager

This project uses **pnpm**. Do not use npm or yarn.

```bash
pnpm install       # install dependencies
pnpm run build     # napi build + tsc
pnpm run build:debug  # debug build
pnpm test          # run full test suite
```

## Build

```bash
# Rust native module (release)
npx napi build --platform --release

# TypeScript (lib/ -> dist/)
pnpm run build:dist

# Test TypeScript (test/ -> dist-test/)
pnpm run build:test
```

## Test Commands

```bash
# Rust
pnpm run test:rust:unit          # cargo test --lib
pnpm run test:rust:mock           # mock pair integration tests
pnpm run test:rust:mock:extended  # all integration tests
pnpm run test:rust:full           # unit + interop + extended
pnpm run test:rust:stress         # 5-min stress tests (--ignored)

# TypeScript
pnpm test                         # core + runtime + interop + ffi
pnpm run test:ffi                 # FFI boundary tests
pnpm run test:ffi:h3              # H3 FFI coverage + stress
pnpm run test:interop             # all interop tests
pnpm run test:interop:h3          # H3 loopback + edge cases
pnpm run test:longhaul            # 5-min sustained load (needs HTTP3_LONGHAUL=1)
pnpm run test:coverage            # c8 coverage report
```

## Architecture

- **Rust core** (`src/`): QUIC/HTTP3 engine using Cloudflare quiche with BoringSSL
- **NAPI bridge**: `napi-rs` v3 connecting Rust workers to Node.js via ThreadsafeFunction
- **TypeScript API** (`lib/`): Public surface mirroring `node:http2`
- **Transport drivers**: io_uring (Linux fast), poll (Linux portable), kqueue (macOS)

## Key Conventions

- Use `runtimeMode: 'portable'` in tests (avoids io_uring ring exhaustion under rapid create/destroy)
- Never prefix shell commands with env vars — use `.cargo/config.toml` `[env]` section instead
- Never revert/checkout/reset to older code — always fix forward
- Fix the architectural root cause before reaching for workarounds (allocator switches, GC hints)
