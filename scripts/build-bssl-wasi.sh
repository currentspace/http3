#!/usr/bin/env bash
# Build boring-sys's ABI-matched BoringSSL for single-threaded wasm32-wasip1.
# boring 5 uses upstream OPENSSL_NO_SOCK and getentropy support: no source
# patch, socket stubs, or RNG shim is needed. WASI random_get supplies entropy.
# Requires WASI_SDK_PATH (validated with SDK 33), cmake, ninja, cargo, and node.
# Output: cache/<boring-sys-version>/<recipe-key>/lib/{libcrypto.a,libssl.a}.
# Only the output directory is printed on stdout; diagnostics go to stderr.
set -euo pipefail

log() { printf '%s\n' "$*" >&2; }
die() { log "build-bssl-wasi: error: $*"; exit 1; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

[[ -n "${WASI_SDK_PATH:-}" ]] || die "WASI_SDK_PATH must point at a wasi-sdk 33 installation"
TOOLCHAIN_FILE="$WASI_SDK_PATH/share/cmake/wasi-sdk-p1.cmake"
[[ -f "$TOOLCHAIN_FILE" ]] || die "wasi-sdk-p1.cmake not found in WASI_SDK_PATH"
for tool in cmake ninja cargo node; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool not found on PATH"
done

METADATA_JSON="$(cargo metadata --locked --format-version=1 --manifest-path "$ROOT/Cargo.toml")"
BORING_SYS_MANIFEST="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  const packages = meta.packages.filter(p => p.name === "boring-sys");
  if (packages.length !== 1) throw new Error("expected exactly one boring-sys version");
  process.stdout.write(packages[0].manifest_path);
' <<<"$METADATA_JSON")"
BORING_SYS_DIR="$(dirname "$BORING_SYS_MANIFEST")"
BORING_SYS_VERSION="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  process.stdout.write(meta.packages.find(p => p.name === "boring-sys").version);
' <<<"$METADATA_JSON")"
BSSL_SRC="$BORING_SYS_DIR/deps/boringssl"
[[ -f "$BSSL_SRC/include/openssl/base.h" ]] || die "expected boring 5 BoringSSL source layout"

# Include the SDK/compiler, CMake toolchain and this entire recipe in the key.
# Changes to options or SDK must never reuse archives built by an older recipe.
CACHE_KEY="$(node - "$WASI_SDK_PATH" "$TOOLCHAIN_FILE" "$ROOT/scripts/build-bssl-wasi.sh" <<'JS'
const { readFileSync } = require('node:fs');
const { execFileSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const { join } = require('node:path');
const [sdk, toolchain, recipe] = process.argv.slice(2);
const hash = createHash('sha256');
for (const path of [join(sdk, 'VERSION'), toolchain, recipe]) hash.update(readFileSync(path));
hash.update(sdk);
hash.update(execFileSync(join(sdk, 'bin/clang'), ['--version']));
hash.update(execFileSync('cmake', ['--version']));
process.stdout.write(hash.digest('hex').slice(0, 20));
JS
)"
CACHE_ROOT="${HTTP3_BSSL_WASI_CACHE_DIR:-$ROOT/target/bssl-wasi}"
OUT_DIR="$CACHE_ROOT/$BORING_SYS_VERSION/$CACHE_KEY"
LIB_DIR="$OUT_DIR/lib"
if [[ -f "$LIB_DIR/libcrypto.a" && -f "$LIB_DIR/libssl.a" && "${HTTP3_BSSL_WASI_FORCE:-0}" != "1" ]]; then
  log "build-bssl-wasi: using cached $BORING_SYS_VERSION / $CACHE_KEY"
  printf '%s\n' "$OUT_DIR"
  exit 0
fi

WORK_DIR="$OUT_DIR/build-source"
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR/src"
cp -R "$BSSL_SRC/." "$WORK_DIR/src/"
DEFINES="-DOPENSSL_NO_THREADS_CORRUPT_MEMORY_AND_LEAK_SECRETS_IF_THREADED -DOPENSSL_NO_SOCK"
log "build-bssl-wasi: building boring-sys $BORING_SYS_VERSION with $WASI_SDK_PATH"
cmake -G Ninja -B "$WORK_DIR/build" -S "$WORK_DIR/src" \
  -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN_FILE" \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_TESTING=OFF \
  -DOPENSSL_NO_ASM=1 \
  -DCMAKE_C_FLAGS="$DEFINES" \
  -DCMAKE_CXX_FLAGS="$DEFINES -fno-exceptions -fno-rtti" >&2
cmake --build "$WORK_DIR/build" --target crypto ssl --parallel "${CARGO_BUILD_JOBS:-8}" >&2
mkdir -p "$LIB_DIR"
cp "$WORK_DIR/build/libcrypto.a" "$WORK_DIR/build/libssl.a" "$LIB_DIR/"
log "build-bssl-wasi: staged archives at $LIB_DIR"
printf '%s\n' "$OUT_DIR"
