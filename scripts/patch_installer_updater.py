#!/usr/bin/env python3
"""Embed the updater's SHA-256 in cargo-dist's generated shell installer.

cargo-dist checksums the application archive but downloads the separately
published `<app>-update` binary without a checksum. This script runs in the
release workflow before the assets are published, applied to both
`target/distrib/` and the `artifacts/` copy that the release step uploads,
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
SHA256_GUARD = (
    '            if ! check_cmd sha256sum; then\n'
    '                err "sha256sum is required to verify orsiktop-update"\n'
    "            fi\n"
)


def patch_installer(text: str, digest: str) -> str:
    # Migrate both pristine and previously patched installers, including old
    # versions that duplicated the availability guard on every invocation.
    text = CHECKSUM_LINE.sub("", text).replace(SHA256_GUARD, "")
    opener = 'if ! downloader "$_updater_url" "$_updater_file"; then'
    if text.count(opener) != 1:
        raise ValueError("expected exactly one updater download block")
    start = text.index(opener)
    fi = text.find("            fi\n", start)
    if fi == -1:
        raise ValueError("updater download block is incomplete")
    insert_at = fi + len("            fi\n")
    guard = SHA256_GUARD + f'            verify_checksum "$_updater_file" "sha256" "{digest}"\n'
    return text[:insert_at] + guard + text[insert_at:]


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
    try:
        text = patch_installer(installer.read_text(), digest)
    except ValueError as error:
        print(f"cannot patch {installer}: {error}", file=sys.stderr)
        return 1
    installer.write_text(text)

    installer_digest = hashlib.sha256(installer.read_bytes()).hexdigest()
    checksums = {installer.name: installer_digest, updater.name: digest}

    sums = root / "sha256.sum"
    if sums.is_file():
        lines = [
            line
            for line in sums.read_text().splitlines()
            if not line.split() or line.split(maxsplit=1)[-1].lstrip("*") not in checksums
        ]
        lines.extend(f"{value} *{name}" for name, value in checksums.items())
        sums.write_text("\n".join(lines) + "\n")

    for name, value in checksums.items():
        standalone = root / f"{name}.sha256"
        if standalone.is_file():
            standalone.write_text(f"{value} *{name}\n")

    print(f"patched {installer.name}: {updater.name} sha256={digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
