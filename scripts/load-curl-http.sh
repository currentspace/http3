#!/usr/bin/env bash
set -euo pipefail

URL="${1:-https://127.0.0.1:8443/}"
PROTO="${PROTO:-h3}"          # h3|h2
TOTAL="${TOTAL:-200}"
CONCURRENCY="${CONCURRENCY:-20}"
MAX_TIME="${MAX_TIME:-5}"
INSECURE_TLS="${INSECURE_TLS:-1}" # 1 => -k

if [[ "${PROTO}" != "h3" && "${PROTO}" != "h2" ]]; then
  echo "PROTO must be h3 or h2" >&2
  exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required" >&2
  exit 2
fi

RESULTS_FILE="$(mktemp)"
LAT_FILE="$(mktemp)"
trap 'rm -f "${RESULTS_FILE}" "${LAT_FILE}"' EXIT

start_epoch="$(date +%s)"

seq "${TOTAL}" | xargs -I{} -P "${CONCURRENCY}" sh -c '
  proto="$1"
  url="$2"
  max_time="$3"
  insecure="$4"

  if [ "$proto" = "h3" ]; then
    proto_flag="--http3-only"
  else
    proto_flag="--http2"
  fi

  tls_flag=""
  if [ "$insecure" = "1" ]; then
    tls_flag="-k"
  fi

  result="$(curl $proto_flag $tls_flag -sS -o /dev/null --max-time "$max_time" -w "%{time_total}\t%{http_code}" "$url" 2>/dev/null || true)"
  if [ -z "$result" ]; then
    printf "NaN\t000\n"
  else
    printf "%s\n" "$result"
  fi
' _ "${PROTO}" "${URL}" "${MAX_TIME}" "${INSECURE_TLS}" >> "${RESULTS_FILE}"

end_epoch="$(date +%s)"
elapsed="$(( end_epoch - start_epoch ))"
if [ "${elapsed}" -lt 1 ]; then
  elapsed=1
fi

success="$(awk -F'\t' '$2 ~ /^2/ {c++} END {print c+0}' "${RESULTS_FILE}")"
failures="$(( TOTAL - success ))"

awk -F'\t' '$1 != "NaN" {print $1}' "${RESULTS_FILE}" | sort -n > "${LAT_FILE}"
samples="$(wc -l < "${LAT_FILE}" | tr -d ' ')"

percentile() {
  local p="$1"
  awk -v pct="${p}" '
    { vals[NR] = $1 }
    END {
      if (NR == 0) {
        print "NaN"
        exit
      }
      idx = int(((pct / 100.0) * (NR - 1)) + 1)
      if (idx < 1) idx = 1
      if (idx > NR) idx = NR
      printf "%.4f", vals[idx]
    }
  ' "${LAT_FILE}"
}

avg_ms="$(awk '{sum+=$1} END { if (NR == 0) print "NaN"; else printf "%.4f", (sum/NR)*1000.0 }' "${LAT_FILE}")"
p50_ms="$(percentile 50)"
p95_ms="$(percentile 95)"
p99_ms="$(percentile 99)"
rps="$(awk -v total="${TOTAL}" -v secs="${elapsed}" 'BEGIN { printf "%.2f", total / secs }')"

echo "protocol=${PROTO}"
echo "url=${URL}"
echo "total=${TOTAL}"
echo "concurrency=${CONCURRENCY}"
echo "elapsed_sec=${elapsed}"
echo "rps=${rps}"
echo "success=${success}"
echo "failures=${failures}"
echo "latency_samples=${samples}"
echo "avg_ms=${avg_ms}"
echo "p50_ms=${p50_ms}"
echo "p95_ms=${p95_ms}"
echo "p99_ms=${p99_ms}"

if [ "${failures}" -gt 0 ]; then
  exit 1
fi
