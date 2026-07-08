#!/usr/bin/env bash
# Local, reproducible vendoring of the quiche wasm FFI fix.
#
# quiche 0.29.2 declares two BoringSSL FFI functions (`AES_ecb_encrypt`,
# `CRYPTO_chacha_20` in src/crypto/boringssl.rs) as returning `c_void`
# where the C side returns `void`. Every other target tolerates the
# mismatch; wasm's typed function-pointer linking does not — it traps the
# first time either function is called (i.e. the first `encrypt_pkt`).
# The fix is a 2-line patch (drop the incorrect `-> c_void`), already
# reviewed and committed at
# spikes/quiche-wasm-wasip1/quiche-0.29.2-wasm-ffi.patch.
#
# This script does NOT touch crates.io, GitHub, or any upstream fork —
# creating a real `cloudflare/quiche` fork and opening a PR requires the
# repo owner's own GitHub credentials and is out of scope for automation
# (see docs/WASM_CLIENT_PLAN.md A4 decision log). Instead it:
#
#   1. Locates the pinned quiche source already fetched into the local
#      cargo registry cache (via `cargo metadata` — works whether that
#      cache was populated by a prior `cargo build`/`cargo fetch`, no
#      network access performed by this script itself).
#   2. Copies it into a generated, git-ignored directory under target/
#      (target/ is already gitignored repo-wide).
#   3. Applies the committed patch file into that copy.
#
# The generated directory is deliberately NOT wired into the *default*
# dependency resolution (see root Cargo.toml + docs/WASM_CLIENT_PLAN.md A4
# decision log for why a workspace-wide `[patch.crates-io]` is unsafe for a
# fresh clone before this script has ever run). `scripts/build-wasm.mjs`
# / `pnpm run build:wasm` invokes this script first, then passes
# `--config patch.crates-io.quiche.path=...` to the *specific* wasm cargo
# invocation only — never to the default native build.
#
# Output: <repo>/target/quiche-wasm-patched/<quiche-version>/
# The resolved directory is printed as the last line of stdout.
#
# Env overrides:
#   HTTP3_QUICHE_WASM_PATCH_FORCE=1   re-copy + re-apply even if cached

set -euo pipefail

log() { printf '%s\n' "$*" >&2; }
die() {
  printf 'prepare-quiche-wasm-patch: error: %s\n' "$*" >&2
  exit 1
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PATCH_FILE="$ROOT/spikes/quiche-wasm-wasip1/quiche-0.29.2-wasm-ffi.patch"
[[ -f "$PATCH_FILE" ]] || die "expected patch file not found: $PATCH_FILE"

command -v patch >/dev/null 2>&1 || die "'patch' not found on PATH"

log "prepare-quiche-wasm-patch: resolving quiche source via cargo metadata"
METADATA_JSON="$(cargo metadata --format-version=1 --manifest-path "$ROOT/Cargo.toml")"

QUICHE_MANIFEST="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  const pkg = meta.packages.find((p) => p.name === "quiche");
  if (!pkg) { console.error("quiche not found in cargo metadata output"); process.exit(1); }
  process.stdout.write(pkg.manifest_path);
' <<<"$METADATA_JSON")"
QUICHE_VERSION="$(node -e '
  const meta = JSON.parse(require("fs").readFileSync(0, "utf8"));
  const pkg = meta.packages.find((p) => p.name === "quiche");
  process.stdout.write(pkg.version);
' <<<"$METADATA_JSON")"

[[ -n "$QUICHE_MANIFEST" ]] || die "could not resolve quiche manifest path"
QUICHE_SRC="$(dirname "$QUICHE_MANIFEST")"
[[ -d "$QUICHE_SRC" ]] || die "quiche source not found at $QUICHE_SRC"

if [[ "$QUICHE_VERSION" != "0.29.2" ]]; then
  log "prepare-quiche-wasm-patch: WARNING quiche resolved to $QUICHE_VERSION, but the patch file is pinned to 0.29.2 hunks against src/crypto/boringssl.rs. If this fails, regenerate the patch (see spikes/quiche-wasm-wasip1/README.md) against the new version."
fi

OUT_ROOT="$ROOT/target/quiche-wasm-patched"
OUT_DIR="$OUT_ROOT/$QUICHE_VERSION"
MARKER="$OUT_DIR/.wasm-ffi-patch-applied"

if [[ -f "$MARKER" && -z "${HTTP3_QUICHE_WASM_PATCH_FORCE:-}" ]]; then
  log "prepare-quiche-wasm-patch: already prepared at $OUT_DIR (set HTTP3_QUICHE_WASM_PATCH_FORCE=1 to redo)"
  printf '%s\n' "$OUT_DIR"
  exit 0
fi

log "prepare-quiche-wasm-patch: copying quiche $QUICHE_VERSION source to $OUT_DIR"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
cp -R "$QUICHE_SRC/." "$OUT_DIR/"
# The vendored copy carries its own Cargo.toml with its own crate name/
# version — untouched, so `path = "..."` patches resolve to the identical
# `quiche 0.29.2` identity crates.io would have provided.

TARGET_FILE="$OUT_DIR/src/crypto/boringssl.rs"
[[ -f "$TARGET_FILE" ]] || die "expected $TARGET_FILE not found in vendored copy"

log "prepare-quiche-wasm-patch: applying wasm FFI fix"
# Apply directly against the copied file (not via the recorded diff
# headers, which are absolute and machine-specific from whichever machine
# generated the patch) so this works regardless of where the registry
# cache or this repo happen to live.
if ! patch --forward --silent "$TARGET_FILE" < "$PATCH_FILE" 2>/tmp/prepare-quiche-wasm-patch.err; then
  if grep -qi "previously applied\|ignored" /tmp/prepare-quiche-wasm-patch.err 2>/dev/null; then
    log "prepare-quiche-wasm-patch: patch already applied (ok)"
  else
    cat /tmp/prepare-quiche-wasm-patch.err >&2
    die "failed to apply $PATCH_FILE to $TARGET_FILE"
  fi
fi
rm -f /tmp/prepare-quiche-wasm-patch.err

touch "$MARKER"
log "prepare-quiche-wasm-patch: ready at $OUT_DIR"
printf '%s\n' "$OUT_DIR"
