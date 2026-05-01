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

step() {
  printf '\n\033[1;36m==> %s\033[0m\n' "$1"
  CURRENT_STEP="$1"
}

mark_done() {
  PASSED+=("$CURRENT_STEP")
}

trap 'rc=$?; if [[ $rc -ne 0 ]]; then printf "\n\033[1;31m✗ FAILED: %s (exit %d)\033[0m\n" "${CURRENT_STEP:-unknown}" "$rc" >&2; fi' EXIT

step "tool versions"
node --version
pnpm --version
rustc --version
cargo --version
mark_done

step "pnpm install (frozen lockfile)"
pnpm install --frozen-lockfile
mark_done

step "lint (eslint lib/ test/)"
pnpm run lint
mark_done

step "typecheck (lib + tests)"
pnpm run typecheck
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

step "rust mock-extended integration tests"
pnpm run test:rust:mock:extended
mark_done

if [[ "${VERIFY_SKIP_BUILD:-0}" != "1" ]]; then
  step "napi release build"
  npx napi build --platform --release
  mark_done

  step "build:dist (lib → dist)"
  pnpm run build:dist
  mark_done
fi

step "build:test (test → dist-test)"
pnpm run build:test
mark_done

step "TS test suite (core + runtime + interop + release + ffi)"
pnpm test
mark_done

if [[ "${VERIFY_SKIP_BROWSER_E2E:-0}" != "1" ]]; then
  step "playwright browsers (chromium + firefox)"
  npx playwright install --with-deps chromium firefox
  mark_done

  step "browser e2e"
  pnpm run test:browser:e2e
  mark_done
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
fi

if [[ "${VERIFY_SKIP_SMOKE_INSTALL:-0}" != "1" ]]; then
  step "smoke install (pack + install)"
  pnpm run smoke:install
  mark_done
fi

trap - EXIT
END_TS=$(date +%s)
ELAPSED=$((END_TS - START_TS))

printf '\n\033[1;32m✓ ALL VERIFICATION PASSED\033[0m (%d steps in %ds)\n' "${#PASSED[@]}" "$ELAPSED"
printf '  steps: %s\n' "${PASSED[*]}"
