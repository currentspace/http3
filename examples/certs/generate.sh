#!/usr/bin/env bash
# Generate self-signed ECDSA certificates for HTTP/3 development.
# Usage: ./generate.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

openssl req -x509 \
  -newkey ec \
  -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout server.key \
  -out server.crt \
  -days 365 \
  -nodes \
  -subj "/CN=localhost"

echo "Generated server.key and server.crt in $SCRIPT_DIR"
