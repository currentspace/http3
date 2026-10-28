# Dependency target inventory

Registry snapshot: 2026-10-28T16:27:22.735671+00:00. Targets are candidates until validated.

Sources: [npm registry](https://registry.npmjs.org/) and [Cargo sparse index](https://index.crates.io/config.json).

| Ecosystem | Dependency | Baseline resolved / declared | Stable candidate |
| --- | --- | --- | --- |
| cargo | `boring` | 4.22.0 | 5.3.0 |
| cargo | `bytes` | 1.12.0 | 1.12.1 |
| cargo | `criterion` | 0.8.2 | 0.8.2 |
| cargo | `crossbeam-channel` | 0.5.15 | 0.5.17 |
| cargo | `env_logger` | 0.11.11 | 0.11.11 |
| cargo | `io-uring` | 0.7.13 | 0.7.15 |
| cargo | `libc` | 0.2.186 | 0.2.191 |
| cargo | `libfuzzer-sys` | 0.4.13 | 0.4.13 |
| cargo | `log` | 0.4.33 | 0.4.34 |
| cargo | `loom` | 0.7.2 | 0.7.2 |
| cargo | `mio` | 1.2.1 | 1.2.4 |
| cargo | `napi` | 3.9.4 | 3.15.0 |
| cargo | `napi-build` | 2.3.2 | 2.6.0 |
| cargo | `napi-derive` | 3.5.7 | 3.6.13 |
| cargo | `nix` | 0.31.3 | 0.31.3 |
| cargo | `quiche` | 0.29.2 | 0.30.0 |
| cargo | `rcgen` | 0.14.8 | 0.14.10 |
| cargo | `ring` | 0.17.14 | 0.17.14 |
| cargo | `serde` | 1.0.228 | 1.0.229 |
| cargo | `serde_json` | 1.0.150 | 1.0.154 |
| cargo | `slab` | 0.4.12 | 0.4.12 |
| cargo | `socket2` | 0.6.4 | 0.6.5 |
| cargo | `tikv-jemalloc-ctl` | 0.7.0 | 0.7.0 |
| cargo | `tikv-jemallocator` | 0.7.0 | 0.7.0 |
| cargo | `arbitrary` | 1.4.2 | 1.5.0 |
| cargo | `cargo-deny` | tool | 0.20.2 |
| cargo | `cargo-audit` | tool | 0.22.2 |
| cargo | `cargo-llvm-cov` | tool | 0.9.1 |
| cargo | `cargo-fuzz` | tool | 0.13.2 |
| cargo | `kani-verifier` | tool | 0.68.0 |
| npm | `@cloudflare/workers-types` | 5.20260707.1 | 5.20261028.1 |
| npm | `@eslint/js` | 10.0.1 | 10.0.1 |
| npm | `@napi-rs/cli` | 3.7.2 | 3.10.9 |
| npm | `@types/node` | 26.0.1 | 26.6.5 |
| npm | `c8` | 11.0.0 | 12.0.0 |
| npm | `eslint` | 10.6.0 | 10.13.0 |
| npm | `playwright` | 1.61.1 | 1.64.1 |
| npm | `typescript` | 6.0.3 | 7.0.2 |
| npm | `typescript-eslint` | 8.62.1 | 8.72.1 |
| npm | `workerd` | 1.20260707.1 | 1.20261028.1 |
| npm | `wrangler` | 4.107.1 | 4.154.0 |
| npm | `express` | 5.2.1 | 5.3.0 |
| npm | `@types/express` | 5.0.6 | 5.0.6 |
| npm | `hono` | 4.12.27 | 4.13.14 |
| npm | `pnpm` | 11.1.2 | 12.16.1 |

First-party `@currentspace/http3-*` prebuilds stay synchronized with the root
release; they are not upgraded independently. Transitive dependencies are
refreshed through lockfile resolution and audited separately.

## Compatibility decisions

- Preserve the Node 24 runtime contract; use the latest Node 24 types (24.19.2)
  rather than allowing newer Node APIs through Node 26 types.
- TypeScript 7 needs its native platform package. The existing blanket
  `--no-optional` installs must be replaced by excluding only first-party
  prebuilds during source builds.
- TypeScript ESLint 8.72.1 requires the TypeScript 6 API. Adopt the upstream
  [side-by-side migration](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0/):
  a TypeScript 7 compiler alias plus the TypeScript 6 compatibility API alias.
  Remove the compatibility alias when the linter and napi generator support
  the stable TypeScript 7 API. Recheck with each tooling update.
- quiche 0.30 + boring 5.3 require one generated-binding correction for
  `ERR_error_string_n`; the source and provenance are committed in
  [vendor/quiche](../vendor/quiche/PATCHES.md), shared by native and WASM.
- WASI SDK 34 fails the command-module link with stable Rust 1.99.1
  (`__wasi_init_tp` missing). Retain checksum-verified SDK 33 until Rust >=1.100
  is stable, then retry both the spike and production artifact suites.
  See [upstream ABI report](https://github.com/napi-rs/napi-rs/issues/3491).
