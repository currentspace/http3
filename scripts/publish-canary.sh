#!/usr/bin/env bash
set -euo pipefail

OTP="${1:?Usage: $0 <otp-code>}"
TAG="canary"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION=$(node -p "require('./package.json').version")

echo "Publishing @currentspace/http3-darwin-arm64@$VERSION ($TAG)..."
cd "$ROOT/npm/darwin-arm64"
npm publish --tag "$TAG" --access public --otp "$OTP"

echo "Publishing @currentspace/http3-linux-arm64-gnu@$VERSION ($TAG)..."
cd "$ROOT/npm/linux-arm64-gnu"
npm publish --tag "$TAG" --access public --otp "$OTP"

echo "Publishing @currentspace/http3@$VERSION ($TAG)..."
cd "$ROOT"
npm publish --tag "$TAG" --access public --otp "$OTP"

echo "Done! Verify with:"
echo "  npm info @currentspace/http3 dist-tags"
echo "  npm info @currentspace/http3-darwin-arm64 dist-tags"
echo "  npm info @currentspace/http3-linux-arm64-gnu dist-tags"
