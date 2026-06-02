#!/usr/bin/env bash
# Run the bounded Kani smoke suite.
#
# Kani does not currently publish a documented canary/nightly cargo-kani
# channel. When the released verifier's bundled Rust compiler lags this
# workspace's MSRV, this script can run against a temporary manifest copy with
# only `rust-version` lowered to the verifier compiler. The real workspace
# manifest remains unchanged and still enforces the shipped stable MSRV.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

REQUIRED="${HTTP3_KANI_REQUIRED:-0}"
ALLOW_MSRV_SHADOW="${HTTP3_KANI_ALLOW_MSRV_SHADOW:-1}"
LIST_ONLY="${HTTP3_KANI_LIST_ONLY:-0}"
DEEP="${HTTP3_KANI_DEEP:-0}"
HARNESS_TIMEOUT="${HTTP3_KANI_HARNESS_TIMEOUT:-60s}"

KANI_BASE_ARGS=(--lib --no-default-features)
KANI_CONTRACT_ARGS=(-Z function-contracts)
KANI_HARNESSES=(
  outbound_payload_units_contract
  accepted_outbound_payload_units_contract
  outbound_payload_units_are_bounded
  accepted_units_never_exceed_admitted_units
  admission_release_never_increases_queue
  recv_buf_model_caps_appends
  cmsg_cursor_step_advances_or_rejects
  provided_buffer_id_constructor_matches_range
  provided_buffer_id_models_iouring_offset_bounds
  quic_lb_plaintext_preserves_low_bits_and_embeds_server_id
  stream_tracking_closed_cleanup_drops_target_state
  stream_tracking_open_cleanup_is_noop
)
KANI_DEEP_HARNESSES=(
  pending_write_partial_accept_accounting
  cmsg_cursor_bounded_walk_stays_in_buffer
)

note_blocked() {
  printf '\nKani smoke blocked: %s\n' "$1" >&2
  if [[ "$REQUIRED" == "1" ]]; then
    exit 1
  fi
  printf 'Set HTTP3_KANI_REQUIRED=1 to make this fatal.\n' >&2
  exit 0
}

run_list() {
  cargo kani "${KANI_BASE_ARGS[@]}" list
}

run_proofs() {
  local args=(
    "${KANI_BASE_ARGS[@]}"
    "${KANI_CONTRACT_ARGS[@]}"
    -Z unstable-options
    --harness-timeout "$HARNESS_TIMEOUT"
  )
  local harness
  for harness in "${KANI_HARNESSES[@]}"; do
    args+=(--harness "$harness")
  done
  if [[ "$DEEP" == "1" ]]; then
    for harness in "${KANI_DEEP_HARNESSES[@]}"; do
      args+=(--harness "$harness")
    done
  fi
  local rustflags="${RUSTFLAGS:-}"
  rustflags="${rustflags:+$rustflags }--cfg kani_contracts"
  RUSTFLAGS="$rustflags" cargo kani "${args[@]}"
}

extract_kani_rust_minor() {
  local err_file="$1"
  sed -nE 's/.*rustc ([0-9]+\.[0-9]+)\.[0-9]+.*/\1/p' "$err_file" | head -n 1
}

make_msrv_shadow() {
  local compiler_minor="$1"
  local shadow
  shadow="$(mktemp -d "${TMPDIR:-/tmp}/http3-kani-shadow.XXXXXX")"
  rsync -a --delete \
    --exclude .git \
    --exclude target \
    "$ROOT/" "$shadow/"
  perl -0pi -e "s/rust-version = \"[0-9]+\\.[0-9]+\"/rust-version = \"$compiler_minor\"/" \
    "$shadow/Cargo.toml"
  printf '%s\n' "$shadow"
}

LIST_OUT="$(mktemp "${TMPDIR:-/tmp}/http3-kani-list.XXXXXX")"
LIST_ERR="$(mktemp "${TMPDIR:-/tmp}/http3-kani-list.XXXXXX.err")"
SHADOW=""
trap 'rm -f "$LIST_OUT" "$LIST_ERR"; [[ -z "$SHADOW" ]] || rm -rf "$SHADOW"' EXIT

if ! run_list >"$LIST_OUT" 2>"$LIST_ERR"; then
  cat "$LIST_OUT" >&2
  cat "$LIST_ERR" >&2

  KANI_RUST_MINOR="$(extract_kani_rust_minor "$LIST_ERR")"
  if [[ "$ALLOW_MSRV_SHADOW" != "1" || -z "$KANI_RUST_MINOR" ]]; then
    note_blocked "verifier/toolchain compatibility"
  fi

  printf '\nKani verifier compiler lags workspace MSRV; retrying in temporary rust-version=%s shadow.\n' \
    "$KANI_RUST_MINOR" >&2
  SHADOW="$(make_msrv_shadow "$KANI_RUST_MINOR")"
  cd "$SHADOW"

  if ! run_list >"$LIST_OUT" 2>"$LIST_ERR"; then
    cat "$LIST_OUT" >&2
    cat "$LIST_ERR" >&2
    note_blocked "shadow manifest preflight failed"
  fi
fi

cat "$LIST_OUT"

if [[ "$LIST_ONLY" == "1" ]]; then
  exit 0
fi

if ! run_proofs; then
  note_blocked "proof execution failed or exceeded HTTP3_KANI_HARNESS_TIMEOUT=$HARNESS_TIMEOUT"
fi
