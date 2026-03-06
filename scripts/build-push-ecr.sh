#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <aws-region> <aws-account-id> <tag>"
  echo "example: $0 us-west-2 123456789012 v0.1.0"
  exit 1
fi

REGION="$1"
ACCOUNT_ID="$2"
TAG="$3"
REPO_NAME="${REPO_NAME:-http3-server}"
IMAGE_URI="${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com/${REPO_NAME}:${TAG}"

aws ecr describe-repositories --region "${REGION}" --repository-names "${REPO_NAME}" >/dev/null 2>&1 || \
  aws ecr create-repository --region "${REGION}" --repository-name "${REPO_NAME}" >/dev/null

aws ecr get-login-password --region "${REGION}" | \
  docker login --username AWS --password-stdin "${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com"

docker build -t "${IMAGE_URI}" .
docker push "${IMAGE_URI}"

echo "Pushed ${IMAGE_URI}"

