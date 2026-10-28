#!/usr/bin/env python3
"""Bound the one maintenance exception unsupported by cargo-deny's schema."""
import datetime
import json
import subprocess


def validate(metadata, today):
    if today >= datetime.date(2026, 11, 28):
        raise ValueError("RUSTSEC-2024-0436 exception expired; review allocator dependencies")
    packages = {p["id"]: p for p in metadata["packages"]}
    paste = [p for p in packages.values() if p["name"] == "paste"]
    if len(paste) != 1 or paste[0]["version"] != "1.0.15":
        raise ValueError("paste graph changed; remove or re-review the exception")
    if not any("proc-macro" in t["kind"] for t in paste[0]["targets"]):
        raise ValueError("paste is no longer a compile-time proc macro")
    parents = {
        (packages[n["id"]]["name"], packages[n["id"]]["version"])
        for n in metadata["resolve"]["nodes"]
        if paste[0]["id"] in n["dependencies"]
    }
    if parents != {("tikv-jemalloc-ctl", "0.7.0")}:
        raise ValueError(f"paste exception parent scope changed: {parents}")


if __name__ == "__main__":
    graph = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version=1",
    ]))
    validate(graph, datetime.datetime.now(datetime.timezone.utc).date())
    print("Maintenance exception scope verified; expires 2026-11-28")
