# Spike: quiche 0.29.2 on wasm32-wasip1 (PROVEN 2026-07-08)

Artifacts from the feasibility spike that compiled quiche 0.29.2 + BoringSSL
(boring-sys 4.22.0's pinned fork) to `wasm32-wasip1` and ran it under Node,
producing a valid 1200-byte QUIC Initial packet (full TLS ClientHello, Initial
AEAD sealing, BoringSSL RNG) and a correct ~996 ms PTO from
`Connection::timeout()`. Module size: 1.53 MB release, pre-wasm-opt.

See `docs/WASM_CLIENT_PLAN.md` for the full build recipe and the plan that
consumes this spike. This directory is a record, not a build target — the
paths in `Cargo.toml` and `.cargo/config.toml` reference a now-deleted
scratchpad and must be re-pointed at a local wasi-sdk 33 install and a
patched quiche checkout before it will build again.

Files:

| File | Purpose |
|------|---------|
| `Cargo.toml` / `.cargo/config.toml` | Spike crate config. `[patch.crates-io]` points at a quiche checkout carrying the FFI patch; rustflags add wasi-sdk's `noeh` libc++ dirs and `-lc++ -lc++abi`. |
| `src/main.rs` | The proven test: `quiche::connect()` → `conn.send()` → prints Initial packet size + `conn.timeout()`. |
| `quiche-0.29.2-wasm-ffi.patch` | The 2-line quiche fix (`AES_ecb_encrypt` / `CRYPTO_chacha_20` declared `-> c_void` trap wasm's typed linking; drop the return type). Upstream to cloudflare/quiche. |
| `wasi-shim.h` | Force-included header for the BoringSSL cmake build: maps `getrandom` → `getentropy`, stubs `socket`/`setsockopt`/`connect` to -1. |
| `mini-shim.mjs` | Hand-rolled ~60-line WASI preview1 shim (no `node:wasi`) proven to run a Rust wasip1 cdylib: random_get, clock_time_get, environ_*, fd_write, proc_exit + defensive stubs. |
| `run-wasi.mjs` | Alternative runner using experimental `node:wasi` (dev only). |
| `inspect-imports.mjs` | Enumerates a module's `wasi_snapshot_preview1` imports (basis for the CI import-allowlist test). |

Key build facts proven (details in the plan doc):

- BoringSSL builds from boring-sys's own `deps/boringssl` with wasi-sdk 33's
  cmake toolchain, `-DOPENSSL_NO_ASM=1`, single-threaded defines, and the BIO
  socket files (`crypto/bio/{connect,socket,socket_helper}.c`) removed from
  CMakeLists (no `netdb.h` in the wasip1 sysroot).
- boring-sys consumes the staged libs via `BORING_BSSL_PATH` /
  `BORING_BSSL_INCLUDE_PATH` / `BORING_BSSL_SYSROOT` (no cmake run in-crate).
- bindgen emits ZERO functions for wasm32 targets unless
  `-fvisibility=default` is in `BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1`.
- The final module imports exactly 15 `wasi_snapshot_preview1` functions —
  no sockets, no threads, no poll.
