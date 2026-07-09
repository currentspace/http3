#!/usr/bin/env node
// Orchestrates the `pnpm run build:wasm` pipeline (docs/WASM_CLIENT_PLAN.md
// D1a): stage BoringSSL for wasm32-wasip1, vendor+patch quiche, cross-build
// `crates/http3-wasm`, optionally run wasm-opt, and copy the artifact to
// dist/wasm/http3_client.wasm.
//
// This is the one place the machine-specific, path-dependent env vars
// (BORING_BSSL_*_wasm32_wasip1, BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1,
// CARGO_TARGET_WASM32_WASIP1_RUSTFLAGS) get computed and set — never
// hand-typed on a command line, never committed to .cargo/config.toml
// (which cannot interpolate them; see docs/WASM_CLIENT_PLAN.md A5).
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, copyFileSync, statSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));

function log(msg) {
  console.log(`build-wasm: ${msg}`);
}

function die(msg) {
  console.error(`build-wasm: error: ${msg}`);
  process.exit(1);
}

function run(cmd, args, opts = {}) {
  return execFileSync(cmd, args, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'inherit'],
    ...opts,
  }).trim();
}

function runInherit(cmd, args, opts = {}) {
  execFileSync(cmd, args, { cwd: root, stdio: 'inherit', ...opts });
}

const wasiSdkPath = process.env.WASI_SDK_PATH;
if (!wasiSdkPath) {
  die(
    'WASI_SDK_PATH is not set. Point it at a wasi-sdk 33 install ' +
      '(share/cmake/wasi-sdk-p1.cmake must exist under it). Never hardcoded — ' +
      'see scripts/build-bssl-wasi.sh.',
  );
}
if (!existsSync(wasiSdkPath)) {
  die(`WASI_SDK_PATH (${wasiSdkPath}) does not exist`);
}

// 1. Stage BoringSSL for wasm32-wasip1 (cached; fast no-op on repeat runs).
log('staging BoringSSL for wasm32-wasip1 (scripts/build-bssl-wasi.sh)...');
const bsslOutDir = run('bash', [join(root, 'scripts/build-bssl-wasi.sh')]);
log(`bssl staged at ${bsslOutDir}`);

// 2. Vendor + patch quiche (cached; fast no-op on repeat runs).
log('preparing patched quiche source (scripts/prepare-quiche-wasm-patch.sh)...');
const patchedQuicheDir = run('bash', [join(root, 'scripts/prepare-quiche-wasm-patch.sh')]);
log(`patched quiche at ${patchedQuicheDir}`);

// 3. Resolve boring-sys's vendored include dir (ABI-matched headers for the
// libs staged in step 1) via cargo metadata — same technique as
// build-bssl-wasi.sh, so this never guesses a registry cache layout.
// `cargo metadata`'s output is multiple MB (the full dependency graph) —
// piped through execFileSync's stdout capture that reliably hits ENOBUFS
// on macOS, so redirect to a temp file instead and read it back.
const metadataTmpFile = join(root, 'target/.build-wasm-cargo-metadata.json');
runInherit('bash', [
  '-c',
  `cargo metadata --format-version=1 --manifest-path "${join(root, 'Cargo.toml')}" > "${metadataTmpFile}"`,
]);
const metadata = JSON.parse(readFileSync(metadataTmpFile, 'utf8'));
const boringSys = metadata.packages.find((p) => p.name === 'boring-sys');
if (!boringSys) {
  die('boring-sys not found in cargo metadata output');
}
const boringSysDir = dirname(boringSys.manifest_path);
const boringIncludePath = join(boringSysDir, 'deps/boringssl/src/include');
if (!existsSync(boringIncludePath)) {
  die(`expected boring-sys include dir not found: ${boringIncludePath}`);
}

// 4. Build the machine-specific env for the wasm32-wasip1 cargo invocation.
const sysroot = join(wasiSdkPath, 'share/wasi-sysroot');
const bsslLibDir = join(bsslOutDir, 'lib');
if (!existsSync(join(bsslLibDir, 'libcrypto.a')) || !existsSync(join(bsslLibDir, 'libssl.a'))) {
  die(`expected staged BoringSSL libs not found under ${bsslLibDir}`);
}

const bindgenClangArgs = [
  '--target=wasm32-wasip1',
  `--sysroot=${sysroot}`,
  `-isystem`,
  `${sysroot}/include/wasm32-wasip1`,
  '-fvisibility=default',
].join(' ');

// CARGO_TARGET_<TRIPLE>_RUSTFLAGS is whitespace-separated, same as RUSTFLAGS.
const rustflags = [
  '-L',
  `native=${join(sysroot, 'lib/wasm32-wasip1/noeh')}`, // wasi-sdk 33 keeps libc++ here
  '-L',
  `native=${join(sysroot, 'lib/wasm32-wasip1')}`,
  '-C',
  'link-arg=-lc++', // boring-sys omits the C++ runtime link line on non-unix targets
  '-C',
  'link-arg=-lc++abi',
  '-C',
  'link-arg=--max-memory=67108864',
].join(' ');

const wasmEnv = {
  ...process.env,
  BORING_BSSL_PATH_wasm32_wasip1: bsslOutDir,
  BORING_BSSL_INCLUDE_PATH_wasm32_wasip1: boringIncludePath,
  BORING_BSSL_SYSROOT_wasm32_wasip1: sysroot,
  BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1: bindgenClangArgs,
  CARGO_TARGET_WASM32_WASIP1_RUSTFLAGS: rustflags,
};

// 5. Cross-build crates/http3-wasm. The quiche FFI patch is applied via a
// `--config` override scoped to *this one invocation* — never written to
// the committed Cargo.toml — so a plain `cargo build --release` (native)
// is completely unaffected regardless of whether this script has ever run
// (see docs/WASM_CLIENT_PLAN.md A4 decision log).
log('cross-compiling crates/http3-wasm for wasm32-wasip1 (this can take a while on a cold cache)...');
runInherit(
  'cargo',
  [
    'build',
    '-p',
    'http3-wasm',
    '--target',
    'wasm32-wasip1',
    '--release',
    '--config',
    `patch.crates-io.quiche.path="${patchedQuicheDir}"`,
  ],
  { env: wasmEnv },
);

const builtWasmPath = join(root, 'target/wasm32-wasip1/release/http3_wasm.wasm');
if (!existsSync(builtWasmPath)) {
  die(`expected build output not found: ${builtWasmPath}`);
}
const preOptSize = statSync(builtWasmPath).size;
log(`built ${builtWasmPath} (${(preOptSize / 1024 / 1024).toFixed(2)} MiB pre-opt)`);

// 6. wasm-opt -Oz if available; skip gracefully (log only) if not — CI/
// packaging-time enforcement of its presence is a later-phase concern
// (D1b/D2), not this script's job.
const distWasmDir = join(root, 'dist/wasm');
mkdirSync(distWasmDir, { recursive: true });
const finalPath = join(distWasmDir, 'http3_client.wasm');

let wasmOptAvailable = true;
try {
  execFileSync('which', ['wasm-opt'], { stdio: 'ignore' });
} catch {
  wasmOptAvailable = false;
}

if (wasmOptAvailable) {
  log('running wasm-opt -Oz --strip-debug...');
  runInherit('wasm-opt', ['-Oz', '--strip-debug', builtWasmPath, '-o', finalPath]);
  const postOptSize = statSync(finalPath).size;
  log(
    `wasm-opt: ${(preOptSize / 1024 / 1024).toFixed(2)} MiB -> ${(postOptSize / 1024 / 1024).toFixed(2)} MiB`,
  );
} else {
  log('wasm-opt not found on PATH — skipping optimization (install binaryen for a smaller artifact); copying the unoptimized build');
  copyFileSync(builtWasmPath, finalPath);
}

log(`ready: ${finalPath} (${(statSync(finalPath).size / 1024).toFixed(0)} KiB)`);
