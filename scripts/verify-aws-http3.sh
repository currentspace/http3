#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <https-hostname> [health-base-url]"
  echo "example: $0 api.example.com http://10.0.2.15:8080"
  exit 1
fi

HOST="$1"
HEALTH_BASE="${2:-}"
URL="https://${HOST}/"

echo "[verify] checking HTTP/3 endpoint: ${URL}"
curl --http3-only -k -sS -D /dev/stderr -o /dev/null "${URL}"

echo "[verify] checking HTTP/2 endpoint: ${URL}"
curl --http2 -k -sS -D /dev/stderr -o /dev/null "${URL}"

if [[ -n "${HEALTH_BASE}" ]]; then
  echo "[verify] checking health endpoints: ${HEALTH_BASE}"
  curl -fsS "${HEALTH_BASE}/healthz" >/dev/null
  curl -fsS "${HEALTH_BASE}/readyz" >/dev/null
fi

echo "[verify] success: protocol and health checks passed"

