#!/usr/bin/env bash
# Fast local/CI safety smoke over UB checks, structure-aware fuzz targets, and
# Kani proof harnesses when the installed verifier is compatible.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

step() {
  printf '\n\033[1;36m==> %s\033[0m\n' "$1"
}

step "Miri unsafe-boundary tests"
cargo +nightly miri test --lib --no-default-features unsafe_boundary

step "cargo-fuzz smoke"
cargo +nightly fuzz run unsafe_wrappers -- -runs=1
cargo +nightly fuzz run unsafe_wrappers_structured -- -runs=1
cargo +nightly fuzz run pending_write_state -- -runs=1
cargo +nightly fuzz run pending_write_ops -- -runs=1
cargo +nightly fuzz run parse_recv_cmsgs -- -runs=1
cargo +nightly fuzz run parse_recv_cmsgs_structured -- -runs=1

step "Kani smoke"
bash scripts/rust-kani-smoke.sh
