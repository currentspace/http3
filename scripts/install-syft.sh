#!/usr/bin/env bash
set -euo pipefail
# Pinned release assets: https://github.com/anchore/syft/releases/tag/v1.55.0
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=darwin_arm64; digest=1217c258bd5eb91ef3189cf22a82e02f9cac07e4ce934350f428e93aff674625 ;;
  Linux-x86_64) platform=linux_amd64; digest=bb65249542db5fedeeb4d3f9b018b75e7264dbfbbb10486f044581a92f90490d ;;
  *) echo "No verified Syft asset for this host" >&2; exit 1 ;;
esac
destination="${1:-target/tools}"
mkdir -p "$destination"
stage="$(mktemp -d "${TMPDIR:-/tmp}/http3-syft.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
curl --fail --show-error --silent --location \
  "https://github.com/anchore/syft/releases/download/v1.55.0/syft_1.55.0_${platform}.tar.gz" \
  -o "$stage/syft.tar.gz"
python3 - "$stage/syft.tar.gz" "$digest" <<'PY'
import hashlib, sys
with open(sys.argv[1], 'rb') as f:
    actual = hashlib.file_digest(f, 'sha256').hexdigest()
if actual != sys.argv[2]:
    raise SystemExit('Syft archive checksum mismatch')
PY
tar -xzf "$stage/syft.tar.gz" -C "$stage" syft
install -m 755 "$stage/syft" "$destination/syft"
"$destination/syft" version
