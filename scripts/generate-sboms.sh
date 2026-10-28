#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
output="${1:-artifacts/sboms}"
mkdir -p "$output"
pnpm sbom --sbom-format cyclonedx --sbom-spec-version 1.6 --lockfile-only --out "$output/javascript.cdx.json"
cargo metadata --locked --format-version=1 > "$output/cargo-metadata.json"
"${SYFT_BIN:-syft}" scan dir:. \
  --source-name '@currentspace/http3' --source-version "$(node -p 'require("./package.json").version')" \
  --exclude './artifacts/**' --exclude './target/**' --exclude './node_modules/**' \
  --exclude './vendor/quiche/Cargo.lock' --exclude './.git/**' \
  --exclude './dist/**' --exclude './dist-test/**' --exclude './coverage/**' \
  --exclude './fuzz/corpus/**' --exclude './fuzz/artifacts/**' \
  -o "cyclonedx-json=$output/source.cdx.json"
python3 scripts/annotate-native-sbom.py "$output/source.cdx.json" "$output/cargo-metadata.json"
