#!/usr/bin/env python3
"""Emit a deterministic CycloneDX 1.5 SBOM from `cargo metadata`.

Usage: python3 scripts/sbom.py [--locked] > orsiktop-sbom.cdx.json
"""

from __future__ import annotations

import json
import subprocess
import sys


def main() -> int:
    command = ["cargo", "metadata", "--format-version", "1"]
    if "--locked" in sys.argv[1:]:
        command.append("--locked")
    metadata = json.loads(subprocess.run(command, check=True, capture_output=True).stdout)
    root = next(
        package
        for package in metadata["packages"]
        if package["id"] == metadata["resolve"]["root"]
    )
    components = []
    for package in metadata["packages"]:
        if package["id"] == root["id"] or package.get("source") is None:
            continue
        component = {
            "type": "library",
            "name": package["name"],
            "version": package["version"],
            "purl": f"pkg:cargo/{package['name']}@{package['version']}",
        }
        if package.get("license"):
            component["licenses"] = [{"expression": package["license"]}]
        components.append(component)
    components.sort(key=lambda item: (item["name"], item["version"]))
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": root["name"],
                "version": root["version"],
            }
        },
        "components": components,
    }
    json.dump(bom, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
