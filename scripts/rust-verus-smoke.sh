#!/usr/bin/env bash
# Run standalone Verus proof files when Verus is installed.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

REQUIRED="${HTTP3_VERUS_REQUIRED:-0}"
VERUS_BIN="${HTTP3_VERUS_BIN:-}"
VERUS_HOME="${HTTP3_VERUS_HOME:-$HOME/.local/share/http3-verus}"

if [[ -z "$VERUS_BIN" ]]; then
  if command -v verus >/dev/null 2>&1; then
    VERUS_BIN="$(command -v verus)"
  elif [[ -x "$VERUS_HOME/verus" ]]; then
    VERUS_BIN="$VERUS_HOME/verus"
  fi
fi

if [[ -z "$VERUS_BIN" || ! -x "$VERUS_BIN" ]]; then
  printf 'Verus smoke blocked: verus binary not found.\n' >&2
  printf 'Run scripts/rust-verus-bootstrap.sh or set HTTP3_VERUS_BIN.\n' >&2
  if [[ "$REQUIRED" == "1" ]]; then
    exit 1
  fi
  exit 0
fi

OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/http3-verus-smoke.XXXXXX")"
trap 'rm -rf "$OUT_DIR"' EXIT

for proof in proofs/verus/*.rs; do
  printf '\n==> Verus %s\n' "$proof"
  "$VERUS_BIN" "$proof" --crate-type=lib --out-dir "$OUT_DIR"
done
