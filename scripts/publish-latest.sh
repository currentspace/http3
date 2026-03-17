#!/usr/bin/env bash
set -euo pipefail

OTP="${1:?Usage: $0 <otp-code>}"
TAG="latest"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION=$(node -p "require('./package.json').version")

echo "============================================"
echo "  Publishing @currentspace/http3@$VERSION"
echo "  dist-tag: $TAG"
echo "============================================"
echo ""

# Pre-flight checks
echo "--- Pre-flight checks ---"

# Verify versions match across all package files
DARWIN_VER=$(node -p "require('./npm/darwin-arm64/package.json').version")
LINUX_VER=$(node -p "require('./npm/linux-arm64-gnu/package.json').version")
CARGO_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

if [ "$DARWIN_VER" != "$VERSION" ]; then
  echo "ERROR: npm/darwin-arm64/package.json version ($DARWIN_VER) != $VERSION"
  exit 1
fi
if [ "$LINUX_VER" != "$VERSION" ]; then
  echo "ERROR: npm/linux-arm64-gnu/package.json version ($LINUX_VER) != $VERSION"
  exit 1
fi
if [ "$CARGO_VER" != "$VERSION" ]; then
  echo "ERROR: Cargo.toml version ($CARGO_VER) != $VERSION"
  exit 1
fi
echo "  Versions consistent: $VERSION"

# Verify prebuilds exist in platform directories
if [ ! -f "npm/darwin-arm64/http3.darwin-arm64.node" ]; then
  echo "ERROR: Missing npm/darwin-arm64/http3.darwin-arm64.node"
  echo "Run: bash scripts/build-prebuilds.sh"
  exit 1
fi
if [ ! -f "npm/linux-arm64-gnu/http3.linux-arm64-gnu.node" ]; then
  echo "ERROR: Missing npm/linux-arm64-gnu/http3.linux-arm64-gnu.node"
  echo "Run: bash scripts/build-prebuilds.sh (on Linux ARM64 or via CI)"
  exit 1
fi
echo "  ARM64 prebuilds present"

# Verify dist/ exists (TypeScript compiled)
if [ ! -d "dist" ]; then
  echo "ERROR: dist/ not found — run: npm run build"
  exit 1
fi
echo "  TypeScript build present"

echo ""

# Publish platform-specific packages first
echo "--- Publishing platform packages ---"

echo "Publishing @currentspace/http3-darwin-arm64@$VERSION ($TAG)..."
cd "$ROOT/npm/darwin-arm64"
npm publish --tag "$TAG" --access public --otp "$OTP"

echo "Publishing @currentspace/http3-linux-arm64-gnu@$VERSION ($TAG)..."
cd "$ROOT/npm/linux-arm64-gnu"
npm publish --tag "$TAG" --access public --otp "$OTP"

# Publish main package
echo ""
echo "--- Publishing main package ---"
cd "$ROOT"
echo "Publishing @currentspace/http3@$VERSION ($TAG)..."
npm publish --tag "$TAG" --access public --otp "$OTP"

echo ""
echo "============================================"
echo "  Published @currentspace/http3@$VERSION"
echo "  dist-tag: $TAG"
echo "============================================"
echo ""
echo "Verify with:"
echo "  npm info @currentspace/http3 dist-tags"
echo "  npm info @currentspace/http3-darwin-arm64 dist-tags"
echo "  npm info @currentspace/http3-linux-arm64-gnu dist-tags"
echo ""
echo "Quick install test:"
echo "  npm install @currentspace/http3@$VERSION"
