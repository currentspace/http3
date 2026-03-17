#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION=$(node -p "require('./package.json').version")
echo "Building prebuilds for @currentspace/http3@$VERSION"
echo ""

# Detect current platform
ARCH=$(uname -m)
OS=$(uname -s)

build_target() {
  local target="$1"
  local label="$2"
  echo "--- Building $label ($target) ---"
  npx napi build --release --platform --target "$target"
  echo ""
}

# Always build for the current platform
if [ "$OS" = "Darwin" ] && [ "$ARCH" = "arm64" ]; then
  build_target "aarch64-apple-darwin" "macOS ARM64"
elif [ "$OS" = "Darwin" ] && [ "$ARCH" = "x86_64" ]; then
  build_target "x86_64-apple-darwin" "macOS x64"
elif [ "$OS" = "Linux" ] && [ "$ARCH" = "aarch64" ]; then
  build_target "aarch64-unknown-linux-gnu" "Linux ARM64"
elif [ "$OS" = "Linux" ] && [ "$ARCH" = "x86_64" ]; then
  build_target "x86_64-unknown-linux-gnu" "Linux x64"
else
  echo "Unsupported platform: $OS/$ARCH"
  exit 1
fi

# Cross-compile additional targets if toolchains are installed
if [ "$OS" = "Darwin" ]; then
  # On macOS, try to cross-compile for the other arch
  if [ "$ARCH" = "arm64" ] && rustup target list --installed | grep -q x86_64-apple-darwin; then
    build_target "x86_64-apple-darwin" "macOS x64 (cross)"
  elif [ "$ARCH" = "x86_64" ] && rustup target list --installed | grep -q aarch64-apple-darwin; then
    build_target "aarch64-apple-darwin" "macOS ARM64 (cross)"
  fi
fi

# Build TypeScript
echo "--- Building TypeScript ---"
npx tsc
echo ""

# Copy prebuilds into platform-specific npm directories
echo "--- Copying prebuilds to npm/ ---"
for node_file in *.node; do
  case "$node_file" in
    http3.darwin-arm64.node)
      cp "$node_file" npm/darwin-arm64/
      echo "  -> npm/darwin-arm64/$node_file"
      ;;
    http3.linux-arm64-gnu.node)
      cp "$node_file" npm/linux-arm64-gnu/
      echo "  -> npm/linux-arm64-gnu/$node_file"
      ;;
  esac
done

echo ""
echo "Prebuilds ready:"
ls -lh *.node 2>/dev/null || echo "  (no .node files in root)"
echo ""
echo "Platform packages:"
ls -lh npm/darwin-arm64/*.node 2>/dev/null || echo "  npm/darwin-arm64/ — no prebuild"
ls -lh npm/linux-arm64-gnu/*.node 2>/dev/null || echo "  npm/linux-arm64-gnu/ — no prebuild"
