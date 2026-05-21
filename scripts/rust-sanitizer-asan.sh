#!/usr/bin/env bash
# AddressSanitizer/LeakSanitizer smoke for Rust-only safety-sensitive tests.
# CI can pin HTTP3_ASAN_TARGET; local runs default to the active nightly host.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST_TARGET="$(rustc +nightly -vV | awk '/^host:/ { print $2 }')"
TARGET="${HTTP3_ASAN_TARGET:-$HOST_TARGET}"
export RUSTFLAGS="${RUSTFLAGS:-} -Zsanitizer=address"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -Zsanitizer=address"
export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=1:halt_on_error=1}"

cargo +nightly test -Zbuild-std --target "$TARGET" --lib --no-default-features unsafe_boundary
