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
  if [[ "$FORCE_TOOL_INSTALL" == "1" || ! -x "$(command -v "$binary" 2>/dev/null || true)" ]]; then
    cargo install --locked "$crate" --force
  else
    printf '%s already installed; set HTTP3_SAFETY_FORCE_TOOL_INSTALL=1 to refresh it.\n' "$binary"
  fi
}

step "stable Rust"
rustup update stable
rustc --version
cargo --version

step "latest nightly with Miri and rust-src"
rustup toolchain install nightly --profile minimal --component miri --component rust-src
cargo +nightly --version
cargo +nightly miri setup

step "cargo safety tools"
install_cargo_tool cargo-fuzz cargo-fuzz
install_cargo_tool cargo-kani kani-verifier
cargo fuzz --version
cargo kani --version

step "Kani setup"
cargo kani setup

step "toolchain summary"
printf 'stable:  %s\n' "$(rustc --version)"
printf 'nightly: %s\n' "$(rustc +nightly --version)"
printf 'cargo:   %s\n' "$(cargo --version)"
printf 'fuzz:    %s\n' "$(cargo fuzz --version)"
printf 'kani:    %s\n' "$(cargo kani --version)"
