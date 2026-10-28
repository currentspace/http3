#!/usr/bin/env python3
"""Record bundled tools that Syft's package catalogers do not identify.

Run against a production-image CycloneDX SBOM built from this checkout.
Keep the source SBOM alongside it for the native addon's Rust/BoringSSL graph.
"""

import json
from pathlib import Path
import re
import sys

root = Path(__file__).resolve().parents[1]
path = Path(sys.argv[1])
document = json.loads(path.read_text())
acme = json.loads((root / "containers/acme-source.json").read_text())
aws = re.search(r"FROM amazon/aws-cli:([\d.]+)@(sha256:[a-f0-9]{64}) AS aws-cli",
                (root / "Dockerfile").read_text())
if not aws:
    raise SystemExit("Cannot resolve the production AWS CLI pin")

tools = [
    {
        "type": "application", "name": "acme.sh", "version": acme["version"],
        "bom-ref": "http3:bundled:acme.sh",
        "licenses": [{"license": {"id": "GPL-3.0-only"}}],
        "externalReferences": [{"type": "distribution", "url": acme["url"],
                                "hashes": [{"alg": "SHA-256", "content": acme["sha256"]}]}],
        "properties": [{"name": "http3:source-commit", "value": acme["commit"]}],
    },
    {
        "type": "application", "name": "aws-cli", "version": aws[1],
        "bom-ref": "http3:bundled:aws-cli",
        "licenses": [{"license": {"id": "Apache-2.0"}}],
        "properties": [{"name": "http3:source-image", "value": f"amazon/aws-cli:{aws[1]}@{aws[2]}"}],
    },
]
refs = {component["bom-ref"] for component in tools}
document["components"] = [c for c in document.get("components", [])
                          if c.get("bom-ref") not in refs] + tools
path.write_text(json.dumps(document, indent=2) + "\n")
print("Recorded pinned acme.sh and AWS CLI in container SBOM")
