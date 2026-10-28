#!/usr/bin/env bash
# Install or refresh the Rust safety toolchain used by Miri, fuzzing,
# sanitizers, and Kani proof harnesses.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FORCE_TOOL_INSTALL="${HTTP3_SAFETY_FORCE_TOOL_INSTALL:-0}"

step() {
  printf '\n\033[1;36m==> %s\033[0m\n' "$1"
}

install_cargo_tool() {
  local binary="$1"
  local crate="$2"
  local version="$3"
  local installed="$("$binary" --version 2>/dev/null || true)"
  if [[ "$installed" != *" $version"* || "$FORCE_TOOL_INSTALL" == "1" || ! -x "$(command -v "$binary" 2>/dev/null || true)" ]]; then
    cargo install --locked "$crate" --version "=$version" --force
  else
    printf '%s already installed; set HTTP3_SAFETY_FORCE_TOOL_INSTALL=1 to refresh it.\n' "$binary"
  fi
}

step "pinned stable Rust"
rustup toolchain install 1.99.1 --profile minimal --component clippy --component rustfmt
rustup component add llvm-tools-preview --toolchain 1.99.1
rustc --version
cargo --version

step "pinned nightly with Miri and rust-src"
rustup toolchain install nightly-2026-10-28 --profile minimal --component miri --component rust-src
cargo +nightly-2026-10-28 --version
cargo +nightly-2026-10-28 miri setup

step "cargo safety tools"
install_cargo_tool cargo-fuzz cargo-fuzz 0.13.2
install_cargo_tool cargo-kani kani-verifier 0.68.0
install_cargo_tool cargo-llvm-cov cargo-llvm-cov 0.9.1
cargo fuzz --version
cargo kani --version

step "Kani setup"
cargo kani setup

step "toolchain summary"
printf 'stable:  %s\n' "$(rustc --version)"
printf 'nightly: %s\n' "$(rustc +nightly-2026-10-28 --version)"
printf 'cargo:   %s\n' "$(cargo --version)"
printf 'fuzz:    %s\n' "$(cargo fuzz --version)"
printf 'kani:    %s\n' "$(cargo kani --version)"
