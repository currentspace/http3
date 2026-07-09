#!/usr/bin/env bash
# Build BoringSSL (boring-sys's own vendored source) for wasm32-wasip1.
#
# This is the proven recipe from the feasibility spike
# (spikes/quiche-wasm-wasip1/), fully scripted and cached:
#
#   1. Locate boring-sys's vendored `deps/boringssl` source via
#      `cargo metadata` (ABI-matched to the bindings boring-sys generates —
#      this is NOT a fresh upstream BoringSSL checkout).
#   2. Copy it to a scratch work directory and drop the socket BIO files
#      (crypto/bio/{connect,socket,socket_helper}.c) from CMakeLists.txt —
#      the wasip1 sysroot has no netdb.h, and quiche needs none of them.
#   3. Configure with wasi-sdk's CMake toolchain file (wasi-sdk-p1.cmake),
#      OPENSSL_NO_ASM=1 (no inline asm on wasm), and single-threaded /
#      getrandom-shim defines proven in the spike.
#   4. Build the `crypto` and `ssl` ninja targets and stage the resulting
#      libcrypto.a / libssl.a under a directory keyed by the boring-sys
#      version, so repeat runs and CI caching are fast.
#
# Output: <cache dir>/<boring-sys-version>/lib/{libcrypto.a,libssl.a}
# The resolved output directory is printed as the last line of stdout.
#
# Required: WASI_SDK_PATH pointing at a wasi-sdk 33 install (this script
# never downloads or hardcodes one — see docs/WASM_CLIENT_PLAN.md A5).
#
# Env overrides:
#   HTTP3_BSSL_WASI_CACHE_DIR  cache root (default: <repo>/target/bssl-wasi)
#   HTTP3_BSSL_WASI_FORCE=1    rebuild even if cached libs already exist

set -euo pipefail

log() { printf '%s\n' "$*" >&2; }
die() {
  printf 'build-bssl-wasi: error: %s\n' "$*" >&2
  exit 1
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

[[ -n "${WASI_SDK_PATH:-}" ]] || die "WASI_SDK_PATH is not set. Point it at a wasi-sdk 33 install (share/cmake/wasi-sdk-p1.cmake must exist under it). This script never downloads or assumes a path."
[[ -d "$WASI_SDK_PATH" ]] || die "WASI_SDK_PATH ($WASI_SDK_PATH) is not a directory"

TOOLCHAIN_FILE="$WASI_SDK_PATH/share/cmake/wasi-sdk-p1.cmake"
[[ -f "$TOOLCHAIN_FILE" ]] || die "wasi-sdk-p1.cmake not found under \$WASI_SDK_PATH/share/cmake — is WASI_SDK_PATH ($WASI_SDK_PATH) a wasi-sdk 33+ install?"

command -v cmake >/dev/null 2>&1 || die "cmake not found on PATH"
command -v ninja >/dev/null 2>&1 || die "ninja not found on PATH"

# Force-included header used by the BoringSSL C/C++ compile: maps
# getrandom -> getentropy, and stubs socket/setsockopt/connect to -1 (the
# BIO files that call them are dropped from the build below, but a few
# other translation units still reference the symbols at compile time).
# Reused verbatim from the proven spike rather than duplicated —
# spikes/quiche-wasm-wasip1/wasi-shim.h is the single source of truth.
SHIM_HEADER="$ROOT/spikes/quiche-wasm-wasip1/wasi-shim.h"
[[ -f "$SHIM_HEADER" ]] || die "expected shim header not found: $SHIM_HEADER"

log "build-bssl-wasi: resolving boring-sys version + vendored source via cargo metadata"
METADATA_JSON="$(cargo metadata --format-version=1 --manifest-path "$ROOT/Cargo.toml")"

BORING_SYS_MANIFEST="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  const pkg = meta.packages.find((p) => p.name === "boring-sys");
  if (!pkg) { console.error("boring-sys not found in cargo metadata output"); process.exit(1); }
  process.stdout.write(pkg.manifest_path);
' <<<"$METADATA_JSON")"
BORING_SYS_VERSION="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  const pkg = meta.packages.find((p) => p.name === "boring-sys");
  process.stdout.write(pkg.version);
' <<<"$METADATA_JSON")"

[[ -n "$BORING_SYS_MANIFEST" ]] || die "could not resolve boring-sys manifest path"
BORING_SYS_DIR="$(dirname "$BORING_SYS_MANIFEST")"
BSSL_SRC="$BORING_SYS_DIR/deps/boringssl"
[[ -d "$BSSL_SRC" ]] || die "vendored BoringSSL source not found at $BSSL_SRC (expected boring-sys crate layout)"

log "build-bssl-wasi: boring-sys $BORING_SYS_VERSION, source at $BSSL_SRC"

CACHE_ROOT="${HTTP3_BSSL_WASI_CACHE_DIR:-$ROOT/target/bssl-wasi}"
OUT_DIR="$CACHE_ROOT/$BORING_SYS_VERSION"
LIB_DIR="$OUT_DIR/lib"

if [[ -f "$LIB_DIR/libcrypto.a" && -f "$LIB_DIR/libssl.a" && -z "${HTTP3_BSSL_WASI_FORCE:-}" ]]; then
  log "build-bssl-wasi: cached libs already present at $LIB_DIR (set HTTP3_BSSL_WASI_FORCE=1 to rebuild)"
  printf '%s\n' "$OUT_DIR"
  exit 0
fi

WORK_DIR="$CACHE_ROOT/.work-$BORING_SYS_VERSION"
log "build-bssl-wasi: staging source to $WORK_DIR"
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR/src"
cp -R "$BSSL_SRC/." "$WORK_DIR/src/"

# Drop the socket BIO files: no netdb.h in the wasip1 sysroot, and quiche
# (a pure client/protocol-core consumer of libcrypto/libssl) needs none of
# them. Portable in-place edit — no BSD-only `sed -i ''`.
CMAKELISTS="$WORK_DIR/src/CMakeLists.txt"
[[ -f "$CMAKELISTS" ]] || die "expected CMakeLists.txt not found at $CMAKELISTS"
sed -e '/crypto\/bio\/connect\.c/d' \
    -e '/crypto\/bio\/socket\.c/d' \
    -e '/crypto\/bio\/socket_helper\.c/d' \
    "$CMAKELISTS" > "$CMAKELISTS.new"
mv "$CMAKELISTS.new" "$CMAKELISTS"

DEFINES="-DOPENSSL_NO_THREADS_CORRUPT_MEMORY_AND_LEAK_SECRETS_IF_THREADED -DFREEBSD_GETRANDOM -DGRND_NONBLOCK=0 -DSO_KEEPALIVE=0 -DSO_ERROR=0 -include $SHIM_HEADER"

log "build-bssl-wasi: configuring cmake (toolchain=$TOOLCHAIN_FILE)"
cmake -G Ninja -B "$WORK_DIR/build" -S "$WORK_DIR/src" \
  -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN_FILE" \
  -DCMAKE_BUILD_TYPE=Release \
  -DOPENSSL_NO_ASM=1 \
  -DCMAKE_C_FLAGS="$DEFINES" \
  -DCMAKE_CXX_FLAGS="$DEFINES" \
  >&2

log "build-bssl-wasi: building crypto + ssl (ninja)"
ninja -C "$WORK_DIR/build" crypto ssl >&2

mkdir -p "$LIB_DIR"
cp "$WORK_DIR/build/libcrypto.a" "$LIB_DIR/libcrypto.a"
cp "$WORK_DIR/build/libssl.a" "$LIB_DIR/libssl.a"

log "build-bssl-wasi: staged $(du -h "$LIB_DIR/libcrypto.a" | cut -f1) libcrypto.a + $(du -h "$LIB_DIR/libssl.a" | cut -f1) libssl.a -> $LIB_DIR"
printf '%s\n' "$OUT_DIR"
