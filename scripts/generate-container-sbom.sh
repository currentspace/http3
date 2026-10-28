#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
image="${1:?Usage: generate-container-sbom.sh IMAGE OUTPUT.json}"
output="${2:?Usage: generate-container-sbom.sh IMAGE OUTPUT.json}"
mkdir -p "$(dirname "$output")"
"${SYFT_BIN:-syft}" scan "docker:$image" -o "cyclonedx-json=$output"
python3 scripts/annotate-container-sbom.py "$output"
