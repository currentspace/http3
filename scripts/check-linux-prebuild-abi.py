#!/usr/bin/env python3
"""Reject release addons that raise the published 0.9.2 glibc symbol floor."""

import os
import re
import subprocess
import sys


def main():
    if len(sys.argv) < 2:
        raise SystemExit("usage: check-linux-prebuild-abi.py <linux-addon.node> [...]")
    limit = (2, 34)
    for binary in sys.argv[1:]:
        result = subprocess.run(
            [os.environ.get("READELF", "readelf"), "--version-info", "--wide", binary],
            check=True, capture_output=True, text=True,
        )
        versions = {
            tuple(map(int, version.split(".")))
            for version in re.findall(r"\bName: GLIBC_(\d+(?:\.\d+)+)\b", result.stdout)
        }
        if not versions:
            raise SystemExit(f"{binary}: no glibc version requirements found")
        maximum = max(versions)
        rendered = ".".join(map(str, maximum))
        if maximum > limit:
            raise SystemExit(
                f"{binary}: requires GLIBC_{rendered}; release limit is GLIBC_2.34. "
                "Build against the supported older Linux sysroot."
            )
        print(f"{binary}: GLIBC_{rendered}, within release limit GLIBC_2.34")


if __name__ == "__main__":
    main()
