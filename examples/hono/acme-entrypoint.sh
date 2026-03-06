#!/usr/bin/env bash
set -euo pipefail

log() {
  echo "[acme-entrypoint] $*"
}

APP_CMD=("node" "examples/hono/production-server.mjs")

if [[ "${ACME_ENABLED:-0}" != "1" ]]; then
  exec "${APP_CMD[@]}"
fi

: "${ACME_DOMAIN:?ACME_DOMAIN is required when ACME_ENABLED=1}"
: "${ACME_LOCK_TABLE:?ACME_LOCK_TABLE is required when ACME_ENABLED=1}"
: "${ACME_CERT_SECRET_ID:?ACME_CERT_SECRET_ID is required when ACME_ENABLED=1}"
: "${CF_TOKEN:?CF_TOKEN is required for Cloudflare DNS challenge}"

AWS_REGION="${AWS_REGION:-us-west-2}"
AWS_PROFILE="${AWS_PROFILE:-}"
ACME_EMAIL="${ACME_EMAIL:-ops@current.space}"
ACME_LOCK_KEY="${ACME_LOCK_KEY:-${ACME_DOMAIN}}"
ACME_LOCK_TTL_SEC="${ACME_LOCK_TTL_SEC:-300}"
ACME_BOOTSTRAP_RETRIES="${ACME_BOOTSTRAP_RETRIES:-45}"
ACME_RETRY_SLEEP_SEC="${ACME_RETRY_SLEEP_SEC:-10}"
ACME_RENEW_BEFORE_SEC="${ACME_RENEW_BEFORE_SEC:-2592000}" # 30 days
ACME_DNS_SLEEP="${ACME_DNS_SLEEP:-30}"
ACME_FAIL_OPEN="${ACME_FAIL_OPEN:-1}"
ACME_DIR="${ACME_DIR:-/var/run/http3-certs/${ACME_DOMAIN}}"
TLS_CERT_PATH="${ACME_DIR}/fullchain.pem"
TLS_KEY_PATH="${ACME_DIR}/privkey.pem"

mkdir -p "${ACME_DIR}"

aws_regional() {
  if [[ -n "${AWS_PROFILE}" ]]; then
    aws --profile "${AWS_PROFILE}" --region "${AWS_REGION}" "$@"
  else
    aws --region "${AWS_REGION}" "$@"
  fi
}

ensure_acme_client() {
  if command -v acme.sh >/dev/null 2>&1; then
    return
  fi
  log "Installing acme.sh..."
  curl -fsSL https://get.acme.sh | sh -s "email=${ACME_EMAIL}" --force >/dev/null
  if [[ ! -x "${HOME}/.acme.sh/acme.sh" ]]; then
    log "acme.sh installation failed"
    return 1
  fi
  ln -sf "${HOME}/.acme.sh/acme.sh" /usr/local/bin/acme.sh
}

cert_is_valid() {
  [[ -s "${TLS_CERT_PATH}" && -s "${TLS_KEY_PATH}" ]] &&
    openssl x509 -checkend "${ACME_RENEW_BEFORE_SEC}" -noout -in "${TLS_CERT_PATH}" >/dev/null 2>&1
}

hydrate_cert_from_secret() {
  local secret_string
  secret_string="$(aws_regional secretsmanager get-secret-value \
    --secret-id "${ACME_CERT_SECRET_ID}" \
    --query "SecretString" \
    --output text 2>/dev/null || true)"
  if [[ -z "${secret_string}" || "${secret_string}" == "None" ]]; then
    return 1
  fi

  python3 - <<'PY' "${secret_string}" "${TLS_KEY_PATH}" "${TLS_CERT_PATH}"
import json,sys
raw, key_path, cert_path = sys.argv[1], sys.argv[2], sys.argv[3]
doc = json.loads(raw)
key = doc.get("key")
cert = doc.get("cert")
if not key or not cert:
    raise SystemExit(1)
with open(key_path, "w", encoding="utf-8") as f:
    f.write(key)
with open(cert_path, "w", encoding="utf-8") as f:
    f.write(cert)
PY
}

publish_cert_to_secret() {
  local payload
  payload="$(python3 - <<'PY' "${TLS_KEY_PATH}" "${TLS_CERT_PATH}"
import json,sys
key_path, cert_path = sys.argv[1], sys.argv[2]
with open(key_path, encoding="utf-8") as f:
    key = f.read()
with open(cert_path, encoding="utf-8") as f:
    cert = f.read()
print(json.dumps({"key": key, "cert": cert}))
PY
)"

  aws_regional secretsmanager put-secret-value \
    --secret-id "${ACME_CERT_SECRET_ID}" \
    --secret-string "${payload}" >/dev/null
}

acquire_lock() {
  local now expires item expr_values
  now="$(date +%s)"
  expires="$((now + ACME_LOCK_TTL_SEC))"
  item="$(cat <<JSON
{"lockKey":{"S":"${ACME_LOCK_KEY}"},"owner":{"S":"${HOSTNAME:-unknown}"},"expiresAt":{"N":"${expires}"}}
JSON
)"
  expr_values="$(cat <<JSON
{":now":{"N":"${now}"}}
JSON
)"

  aws_regional dynamodb put-item \
    --table-name "${ACME_LOCK_TABLE}" \
    --item "${item}" \
    --condition-expression "attribute_not_exists(lockKey) OR expiresAt < :now" \
    --expression-attribute-values "${expr_values}" >/dev/null 2>&1
}

release_lock() {
  aws_regional dynamodb delete-item \
    --table-name "${ACME_LOCK_TABLE}" \
    --key "{\"lockKey\":{\"S\":\"${ACME_LOCK_KEY}\"}}" >/dev/null 2>&1 || true
}

issue_or_renew() {
  if [[ "${CF_TOKEN}" == "REPLACE_ME" || -z "${CF_TOKEN}" ]]; then
    log "Cloudflare token is missing/placeholder; skipping ACME issuance"
    return 1
  fi
  export CF_Token="${CF_TOKEN}"
  acme.sh --set-default-ca --server letsencrypt >/dev/null 2>&1 || true
  acme.sh --issue \
    --dns dns_cf \
    --dnssleep "${ACME_DNS_SLEEP}" \
    -d "${ACME_DOMAIN}" \
    --server letsencrypt \
    --key-file "${TLS_KEY_PATH}" \
    --fullchain-file "${TLS_CERT_PATH}"
}

bootstrap_cert() {
  ensure_acme_client

  if hydrate_cert_from_secret && cert_is_valid; then
    log "Loaded valid certificate from secret ${ACME_CERT_SECRET_ID}"
    return
  fi

  local attempt
  for attempt in $(seq 1 "${ACME_BOOTSTRAP_RETRIES}"); do
    if hydrate_cert_from_secret && cert_is_valid; then
      log "Certificate became available from secret"
      return
    fi

    if acquire_lock; then
      log "ACME lock acquired; issuing/renewing certificate (attempt ${attempt})"
      trap release_lock EXIT

      if ! cert_is_valid; then
        issue_or_renew
        publish_cert_to_secret
      fi

      release_lock
      trap - EXIT

      if cert_is_valid; then
        log "Certificate issued and stored to secret"
        return
      fi
    fi

    log "Waiting for certificate lock/cert availability (${attempt}/${ACME_BOOTSTRAP_RETRIES})"
    sleep "${ACME_RETRY_SLEEP_SEC}"
  done

  log "Failed to obtain a valid certificate for ${ACME_DOMAIN}"
  return 1
}

if bootstrap_cert; then
  # Force the app to load certs from filesystem paths (not stale injected JSON).
  unset HTTP3_TLS_SECRET_JSON
  export HTTP3_TLS_KEY_PATH="${TLS_KEY_PATH}"
  export HTTP3_TLS_CERT_PATH="${TLS_CERT_PATH}"
else
  if [[ "${ACME_FAIL_OPEN}" == "1" ]]; then
    log "ACME bootstrap failed; starting app with existing cert secret/env"
  else
    exit 1
  fi
fi

exec "${APP_CMD[@]}"
