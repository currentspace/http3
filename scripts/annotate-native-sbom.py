#!/usr/bin/env python3
"""Add bundled BoringSSL provenance and verify complete Cargo coverage."""
import base64
import json
from pathlib import Path
import sys

bom_path, metadata_path = map(Path, sys.argv[1:])
bom = json.loads(bom_path.read_text())
metadata = json.loads(metadata_path.read_text())
sources = json.loads(Path("containers/native-sources.json").read_text())
components = bom["components"]
actual = {(p["name"], p.get("version")) for p in components}
missing = {(p["name"], p["version"]) for p in metadata["packages"]} - actual
if missing:
    raise SystemExit(f"Source SBOM missed Cargo packages: {sorted(missing)}")
boring = [p for p in metadata["packages"] if p["name"] == "boring-sys"]
if len(boring) != 1 or boring[0]["version"] != sources["boring-sys"]:
    raise SystemExit("Update containers/native-sources.json for the resolved boring-sys")
license_path = Path(boring[0]["manifest_path"]).parent / "deps/boringssl/LICENSE"
ref = "pkg:generic/boringssl@" + sources["boringsslCommit"]
components.append({
    "type": "library", "name": "BoringSSL", "version": sources["boringsslCommit"],
    "bom-ref": ref, "purl": ref,
    "licenses": [{"license": {"name": "BoringSSL license bundle", "text": {
        "contentType": "text/plain", "encoding": "base64",
        "content": base64.b64encode(license_path.read_bytes()).decode(),
    }}}],
    "externalReferences": [{"type": "vcs", "url": sources["boringsslRepository"] +
                            "/tree/" + sources["boringsslCommit"]}],
    "properties": [{"name": "http3:bundled-by", "value": "boring-sys@" + sources["boring-sys"]}],
})
for component in components:
    if component["name"] == "boring-sys":
        dependencies = bom.setdefault("dependencies", [])
        parent = next((d for d in dependencies if d["ref"] == component["bom-ref"]), None)
        if parent is None:
            parent = {"ref": component["bom-ref"], "dependsOn": []}
            dependencies.append(parent)
        parent.setdefault("dependsOn", []).append(ref)
    if component["name"] == "quiche":
        if component.get("version") != sources["quiche"]:
            raise SystemExit("Update quiche source provenance after its version changes")
        component.setdefault("properties", []).append({
            "name": "http3:source-patch", "value": sources["quichePatch"],
        })
bom_path.write_text(json.dumps(bom, indent=2) + "\n")
print(f"Verified {len(metadata['packages'])} Cargo packages and added bundled BoringSSL provenance")
