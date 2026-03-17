#!/usr/bin/env bash
set -euo pipefail

NEW_VERSION="${1:?Usage: $0 <new-version>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Read current version from package.json
CURRENT_VERSION=$(node -p "require('$ROOT/package.json').version")

if [ "$NEW_VERSION" = "$CURRENT_VERSION" ]; then
  echo "Already at version $NEW_VERSION"
  exit 0
fi

echo "Bumping $CURRENT_VERSION -> $NEW_VERSION"

# package.json — top-level version
sed -i.bak "s/\"version\": \"$CURRENT_VERSION\"/\"version\": \"$NEW_VERSION\"/" "$ROOT/package.json"
rm -f "$ROOT/package.json.bak"

# package.json — optionalDependencies
sed -i.bak "s/\"@currentspace\/http3-linux-arm64-gnu\": \"$CURRENT_VERSION\"/\"@currentspace\/http3-linux-arm64-gnu\": \"$NEW_VERSION\"/" "$ROOT/package.json"
sed -i.bak "s/\"@currentspace\/http3-darwin-arm64\": \"$CURRENT_VERSION\"/\"@currentspace\/http3-darwin-arm64\": \"$NEW_VERSION\"/" "$ROOT/package.json"
rm -f "$ROOT/package.json.bak"

# Cargo.toml
sed -i.bak "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" "$ROOT/Cargo.toml"
rm -f "$ROOT/Cargo.toml.bak"

# Platform-specific npm packages
for pkg in "$ROOT"/npm/*/package.json; do
  sed -i.bak "s/\"version\": \"$CURRENT_VERSION\"/\"version\": \"$NEW_VERSION\"/" "$pkg"
  rm -f "$pkg.bak"
done

echo "Updated files:"
echo "  package.json"
echo "  Cargo.toml"
for pkg in "$ROOT"/npm/*/package.json; do
  echo "  ${pkg#$ROOT/}"
done

echo ""
echo "Version bumped to $NEW_VERSION. Don't forget to:"
echo "  1. Add a CHANGELOG.md entry for $NEW_VERSION"
echo "  2. Commit and tag: git tag v$NEW_VERSION"
