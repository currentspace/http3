#!/usr/bin/env bash
set -euo pipefail
# Retired: this entry point bypassed release gates and omitted native sidecars.
echo 'Use pnpm run release:latest -- --dist-tag canary after completing docs/RELEASE_RUNBOOK.md. This legacy script no longer publishes.' >&2
exit 1
