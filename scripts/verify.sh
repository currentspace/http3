#!/usr/bin/env bash
# Canonical full-verification entry point. Invoked locally via
# `pnpm verify` and inside Dockerfile.verify on debian.
#
# Each step must pass — no `|| true`. Optional gates can be skipped with the
# VERIFY_SKIP_* env vars below for faster local iteration, but CI runs every
# step.
#
# Env knobs (all default off):
#   VERIFY_SKIP_BROWSER_E2E=1   skip Playwright (e.g. inside docker if you
#                               don't want to install browser deps yet)
#   VERIFY_SKIP_PERF_GATES=1    skip concurrency + load smoke gates
#   VERIFY_SKIP_SMOKE_INSTALL=1 skip pack-and-install smoke test
#   VERIFY_SKIP_BUILD=1         assume native + dist already built
#   VERIFY_SKIP_WASM=1          skip the wasm build+test step (it also
#                               self-skips whenever WASI_SDK_PATH is unset —
#                               see docs/WASM_CLIENT_PLAN.md D1b/D2)
#
# Flags:
#   --no-build      same as VERIFY_SKIP_BUILD=1
#   --fast          skip browser e2e + perf gates + smoke install
#   --help          print this header

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

for arg in "$@"; do
  case "$arg" in
    --no-build) export VERIFY_SKIP_BUILD=1 ;;
    --fast)
      export VERIFY_SKIP_BROWSER_E2E=1
      export VERIFY_SKIP_PERF_GATES=1
      export VERIFY_SKIP_SMOKE_INSTALL=1
      ;;
    --help|-h)
      sed -n '2,18p' "$0"
      exit 0
      ;;
    *)
      printf 'unknown flag: %s\n' "$arg" >&2
      exit 2
      ;;
  esac
done

START_TS=$(date +%s)
PASSED=()
SKIPPED=()

step() {
  printf '\n\033[1;36m==> %s\033[0m\n' "$1"
  CURRENT_STEP="$1"
}

mark_done() {
  PASSED+=("$CURRENT_STEP")
}

skip() {
  SKIPPED+=("$1")
  printf '\nSKIP: %s\n' "$1"
}

report() {
  local rc=$?
  trap - EXIT
  if [[ $rc -ne 0 ]]; then
    printf '\nFAILED: %s (exit %d)\n' "${CURRENT_STEP:-unknown}" "$rc" >&2
  fi
  printf '\nVerification: %d passed, %d failed, %d skipped (%ds)\n' \
    "${#PASSED[@]}" "$((rc != 0))" "${#SKIPPED[@]}" "$(($(date +%s) - START_TS))"
  printf '  passed: %s\n' "${PASSED[*]:-none}"
  printf '  skipped: %s\n' "${SKIPPED[*]:-none}"
  exit "$rc"
}
trap report EXIT

step "tool versions"
node --version
pnpm --version
rustc --version
cargo --version
mark_done

step "pnpm install (frozen lockfile, no optional prebuilds)"
pnpm install --frozen-lockfile --no-optional
mark_done

step "lint (eslint lib/ test/)"
pnpm run lint
mark_done

step "typecheck (lib + tests)"
pnpm run typecheck
mark_done

step "typecheck (workerd tsconfig — lib/wasm/** without @types/node)"
pnpm run typecheck:workerd
mark_done

step "Node-API boundary check"
pnpm run check:napi-boundary
mark_done

step "rust clippy (lib, default features)"
# Lib lints with default features (node-api). Cargo.toml [lints.clippy] sets
# pedantic to warn — we gate on errors only. Integration tests require the
# `bench-internals` feature so they're linted as a separate pass below.
cargo clippy --lib
mark_done

step "rust clippy (tests, bench-internals)"
cargo clippy --tests --features bench-internals --no-default-features
mark_done

step "rust unit tests"
pnpm run test:rust:unit
mark_done

step "rust loom tests"
cargo test --test buffer_recycler_loom --no-default-features
mark_done

step "rust mock-extended integration tests"
pnpm run test:rust:mock:extended
mark_done

if [[ "${VERIFY_SKIP_BUILD:-0}" != "1" ]]; then
  step "native + dist build"
  pnpm run build
  mark_done

else
  skip "native + dist build (VERIFY_SKIP_BUILD=1)"
fi

# wasm build+test (docs/WASM_CLIENT_PLAN.md D1b): the wasi-sdk toolchain is
# machine-specific and NOT installed in the verify.yml CI lanes (ubuntu x3
# Node, macOS, Dockerfile.verify) — the dedicated D2 CI job is the real
# enforcement point there. This step must never hard-fail just because the
# toolchain is absent; it self-skips with a clear notice instead.
if [[ "${VERIFY_SKIP_WASM:-0}" == "1" ]]; then
  skip "wasm build + test (VERIFY_SKIP_WASM=1)"
elif [[ -z "${WASI_SDK_PATH:-}" ]]; then
  skip "wasm build + test (WASI_SDK_PATH unset; dedicated CI lane required)"
else
  step "wasm build (build:wasm)"
  pnpm run build:wasm
  mark_done

  step "wasm test suite (HTTP3_WASM=1 test:wasm)"
  HTTP3_WASM=1 pnpm run test:wasm
  mark_done
fi

step "build:test (test → dist-test)"
pnpm run build:test
mark_done

step "TS test suite (core + runtime + interop + release + ffi)"
pnpm test
mark_done

if [[ "${VERIFY_SKIP_BROWSER_E2E:-0}" != "1" ]]; then
  step "playwright browsers (chromium + firefox + webkit)"
  pnpm exec playwright install --with-deps chromium firefox webkit
  mark_done

  step "browser e2e"
  pnpm run test:browser:e2e
  mark_done
else
  skip "browser e2e (VERIFY_SKIP_BROWSER_E2E=1)"
fi

if [[ "${VERIFY_SKIP_PERF_GATES:-0}" != "1" ]]; then
  step "concurrency gate"
  HTTP3_CONCURRENCY_MAX_MS="${HTTP3_CONCURRENCY_MAX_MS:-12000}" \
    pnpm run perf:concurrency-gate
  mark_done

  step "load smoke gate"
  HTTP3_LOAD_SMOKE_TOTAL="${HTTP3_LOAD_SMOKE_TOTAL:-150}" \
    HTTP3_LOAD_SMOKE_CONCURRENCY="${HTTP3_LOAD_SMOKE_CONCURRENCY:-25}" \
    HTTP3_LOAD_SMOKE_MAX_MS="${HTTP3_LOAD_SMOKE_MAX_MS:-10000}" \
    pnpm run perf:load-smoke-gate
  mark_done
else
  skip "performance gates (VERIFY_SKIP_PERF_GATES=1)"
fi

if [[ "${VERIFY_SKIP_SMOKE_INSTALL:-0}" != "1" ]]; then
  step "smoke install (pack + install)"
  pnpm run smoke:install
  mark_done
else
  skip "packed-install smoke (VERIFY_SKIP_SMOKE_INSTALL=1)"
fi
