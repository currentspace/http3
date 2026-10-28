#!/usr/bin/env bash
# Install or refresh the pinned Verus binary release for standalone proofs.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERUS_HOME="${HTTP3_VERUS_HOME:-$HOME/.local/share/http3-verus}"
VERSION="${HTTP3_VERUS_VERSION:-release/0.2026.10.26.d17fbd0}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) ASSET_PATTERN='(arm|aarch64).*macos|macos.*(arm|aarch64)' ;;
  Darwin-x86_64) ASSET_PATTERN='(x86|x86_64).*macos|macos.*(x86|x86_64)' ;;
  Linux-x86_64) ASSET_PATTERN='(x86|x86_64).*linux|linux.*(x86|x86_64)' ;;
  *)
    printf 'Unsupported Verus binary platform: %s-%s\n' "$(uname -s)" "$(uname -m)" >&2
    exit 1
    ;;
esac

tmp="$(mktemp -d "${TMPDIR:-/tmp}/http3-verus.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

if [[ "$VERSION" == "latest" ]]; then
  release_url='https://api.github.com/repos/verus-lang/verus/releases/latest'
else
  release_url="https://api.github.com/repos/verus-lang/verus/releases/tags/$VERSION"
fi

metadata="$tmp/release.json"
curl --fail --show-error --silent --location "$release_url" -o "$metadata"

asset_url="$(
  awk -F '"' '/browser_download_url/ { print $4 }' "$metadata" \
    | grep -E "$ASSET_PATTERN" \
    | grep -E '\.zip$' \
    | head -n 1
)"

if [[ -z "$asset_url" ]]; then
  printf 'Could not find a Verus release asset matching %s in %s.\n' "$ASSET_PATTERN" "$release_url" >&2
  exit 1
fi

zip="$tmp/verus.zip"
curl --fail --show-error --silent --location "$asset_url" -o "$zip"
python3 - "$metadata" "$asset_url" "$zip" <<'PYVERIFY'
import hashlib, json, sys
metadata, url, archive = sys.argv[1:]
with open(metadata) as f:
    asset = next(a for a in json.load(f)['assets'] if a['browser_download_url'] == url)
expected = asset.get('digest', '')
if not expected.startswith('sha256:'):
    raise SystemExit('Verus release asset has no SHA-256 digest')
h = hashlib.sha256()
with open(archive, 'rb') as f:
    for block in iter(lambda: f.read(1024 * 1024), b''):
        h.update(block)
if 'sha256:' + h.hexdigest() != expected:
    raise SystemExit('Verus archive checksum mismatch')
print('Verified Verus archive SHA-256:', h.hexdigest())
PYVERIFY
unzip -q "$zip" -d "$tmp/extract"

verus_bin="$(find "$tmp/extract" -type f -name verus -perm -111 | head -n 1)"
if [[ -z "$verus_bin" ]]; then
  printf 'Downloaded Verus archive did not contain an executable named verus.\n' >&2
  exit 1
fi

install_src="$(dirname "$verus_bin")"
mkdir -p "$(dirname "$VERUS_HOME")"
rm -rf "$VERUS_HOME"
cp -R "$install_src" "$VERUS_HOME"

if [[ "$(uname -s)" == "Darwin" ]]; then
  if [[ -x "$VERUS_HOME/macos_allow_gatekeeper.sh" ]]; then
    if ! (cd "$VERUS_HOME" && bash macos_allow_gatekeeper.sh); then
      printf 'Verus gatekeeper helper failed; clearing quarantine attributes directly.\n' >&2
      xattr -dr com.apple.quarantine "$VERUS_HOME" 2>/dev/null || true
    fi
  else
    xattr -dr com.apple.quarantine "$VERUS_HOME" 2>/dev/null || true
  fi
fi

printf 'Installed Verus to %s\n' "$VERUS_HOME"
# This release's launcher selects 1.95.0 even though its build metadata names
# a newer compiler. Install the launcher's required sysroot explicitly.
rustup toolchain install "${HTTP3_VERUS_RUST_VERSION:-1.95.0}" --profile minimal
"$VERUS_HOME/verus" --version || "$VERUS_HOME/verus"
