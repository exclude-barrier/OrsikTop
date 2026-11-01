"""Offline installer regressions; no downloads or real installation paths."""
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "patch_installer_updater.py"
spec = importlib.util.spec_from_file_location("patch_installer", SCRIPT)
patcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(patcher)

DOWNLOAD = '''            if ! downloader "$_updater_url" "$_updater_file"; then
                err "download failed"
            fi
'''
SHELL = '''#!/bin/sh
set -eu
err() { echo "$*" >&2; exit 1; }
check_cmd() { command -v "$1" >/dev/null 2>&1; }
downloader() { cp "$SOURCE" "$2"; }
verify_checksum() {
    actual=$(sha256sum "$1")
    [ "${actual%% *}" = "$3" ] || err "checksum mismatch"
}
_updater_url=fixture
_updater_file="$DEST"
''' + DOWNLOAD + '''printf installed > "$INSTALLED"
'''


class InstallerPatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="orsiktop-patch-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.installer = self.root / "orsiktop-installer.sh"
        self.updater = self.root / "orsiktop-x86_64-unknown-linux-gnu-update"
        self.installer.write_text(SHELL)
        self.updater.write_bytes(b"verified updater fixture")

    def patch(self, success=True):
        result = subprocess.run([sys.executable, "-B", str(SCRIPT), str(self.root)],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def test_repeated_patch_is_byte_identical_including_checksum_files(self):
        (self.root / "sha256.sum").write_text(
            f"{'0' * 64} *{self.installer.name}\n\n{'1' * 64}  {self.updater.name}\n"
            f"{'2' * 64} *unrelated.tar.xz\n")
        for file in (self.installer, self.updater):
            (self.root / f"{file.name}.sha256").write_text("old checksum\n")
        self.patch()
        first = {p.name: p.read_bytes() for p in self.root.iterdir()}
        for _ in range(3):
            self.patch()
            self.assertEqual(first, {p.name: p.read_bytes() for p in self.root.iterdir()})
        text = self.installer.read_text()
        self.assertEqual(text.count("if ! check_cmd sha256sum"), 1)
        self.assertEqual(text.count('verify_checksum "$_updater_file"'), 1)
        catalog = (self.root / "sha256.sum").read_text()
        self.assertIn(f"{'2' * 64} *unrelated.tar.xz", catalog)
        for file in (self.installer, self.updater):
            entry = f"{hashlib.sha256(file.read_bytes()).hexdigest()} *{file.name}\n"
            self.assertIn(entry, catalog)
            self.assertEqual((self.root / f"{file.name}.sha256").read_text(), entry)

    def test_migrates_duplicated_legacy_guards_and_updates_changed_updater(self):
        old = patcher.SHA256_GUARD * 3 + f'            verify_checksum "$_updater_file" "sha256" "{"0" * 64}"\n'
        self.installer.write_text(SHELL.replace(DOWNLOAD, DOWNLOAD + old))
        self.patch()
        self.updater.write_bytes(b"changed updater fixture")
        self.patch()
        text = self.installer.read_text()
        self.assertEqual(text.count("if ! check_cmd sha256sum"), 1)
        self.assertEqual(text.count('verify_checksum "$_updater_file"'), 1)
        self.assertIn(hashlib.sha256(self.updater.read_bytes()).hexdigest(), text)

    def test_shell_accepts_verified_updater_and_rejects_tampering_before_install(self):
        self.patch()
        for tamper in (False, True):
            installed = self.root / "installed"
            if installed.exists():
                installed.unlink()
            if tamper:
                self.updater.write_bytes(b"tampered")
            env = dict(os.environ, SOURCE=str(self.updater), DEST=str(self.root / "download"),
                       INSTALLED=str(installed))
            result = subprocess.run(["sh", str(self.installer)], env=env,
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode == 0, not tamper)
            self.assertEqual(installed.exists(), not tamper)

    def test_missing_ambiguous_or_incomplete_download_block_fails_without_writes(self):
        for text in ("no download block\n", SHELL + DOWNLOAD,
                     'if ! downloader "$_updater_url" "$_updater_file"; then\n'):
            self.installer.write_text(text)
            self.patch(success=False)
            self.assertEqual(self.installer.read_text(), text)

    def test_ambiguous_updater_inventory_fails_without_writes(self):
        (self.root / "orsiktop-another-update").write_bytes(b"other")
        self.patch(success=False)
        self.assertEqual(self.installer.read_text(), SHELL)


if __name__ == "__main__":
    unittest.main()
