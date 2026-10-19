#!/usr/bin/env python3
"""Embed the updater's SHA-256 in cargo-dist's generated shell installer.

cargo-dist checksums the application archive but downloads the separately
published `<app>-update` binary without a checksum. This script runs in the
release workflow after `dist build` and before `dist host --steps=upload`,
tightening the generated installer without changing cargo-dist itself.

Usage: python3 scripts/patch_installer_updater.py target/distrib
"""

from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path

CHECKSUM_LINE = re.compile(
    r'^[ \t]*verify_checksum "\$_updater_file" "sha256" "[0-9a-f]{64}"[ \t]*\n',
    re.MULTILINE,
)


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "target/distrib")
    installers = sorted(root.glob("orsiktop-installer.sh"))
    updaters = sorted(root.glob("orsiktop-*-update"))
    if len(installers) != 1 or len(updaters) != 1:
        print(
            f"expected one installer and one updater in {root}, "
            f"found {installers!r} and {updaters!r}",
            file=sys.stderr,
        )
        return 1

    installer, updater = installers[0], updaters[0]
    digest = hashlib.sha256(updater.read_bytes()).hexdigest()
    text = installer.read_text()
    text = CHECKSUM_LINE.sub("", text)

    opener = 'if ! downloader "$_updater_url" "$_updater_file"; then'
    start = text.find(opener)
    if start == -1:
        print(f"updater download block not found in {installer}", file=sys.stderr)
        return 1
    fi = text.find("            fi\n", start)
    if fi == -1:
        print(f"updater download block is incomplete in {installer}", file=sys.stderr)
        return 1
    insert_at = fi + len("            fi\n")
    guard = (
        '            if ! check_cmd sha256sum; then\n'
        '                err "sha256sum is required to verify orsiktop-update"\n'
        "            fi\n"
        f'            verify_checksum "$_updater_file" "sha256" "{digest}"\n'
    )
    text = text[:insert_at] + guard + text[insert_at:]
    installer.write_text(text)

    sums = root / "sha256.sum"
    if sums.is_file():
        lines = [
            line
            for line in sums.read_text().splitlines()
            if not line.endswith(f"*{updater.name}")
        ]
        lines.append(f"{digest} *{updater.name}")
        sums.write_text("\n".join(lines) + "\n")

    print(f"patched {installer.name}: {updater.name} sha256={digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
