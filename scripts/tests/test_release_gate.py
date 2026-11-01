"""Fail-closed checks for the checked-in workflow's local reusable CI gate.

Only the deliberately fixed, inline job/needs syntax used here is recognized;
unrecognized rewrites fail the test rather than silently bypassing a gate.
YAML grammar/expression validation is additionally performed with actionlint.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


def jobs(text):
    body = text.split("\njobs:\n", 1)[1]
    parts = re.split(r"^  ([\w-]+):\s*$", body, flags=re.MULTILINE)
    return dict(zip(parts[1::2], parts[2::2]))


class ReleaseGateTests(unittest.TestCase):
    def assert_gate(self, release, ci):
        release_jobs = jobs(release)
        self.assertIn("uses: ./.github/workflows/ci.yml", release_jobs["validate"])
        self.assertIn("contents: read", release_jobs["validate"])
        self.assertNotIn("write", release_jobs["validate"])
        self.assertIn("  workflow_call:", ci.split("\njobs:\n", 1)[0])
        for name in ("prepare", "plan", "host"):
            match = re.search(r"^    needs: \[([^\]]+)\]$", release_jobs[name], re.MULTILINE)
            self.assertIsNotNone(match, name)
            self.assertIn("validate", [item.strip() for item in match[1].split(",")])
        self.assertIn("if: ${{ needs.validate.result == 'success' &&", release_jobs["plan"])
        # `always()` must never let publication bypass a failed/skipped gate.
        self.assertIn("if: ${{ always() && needs.validate.result == 'success' &&", release_jobs["host"])
        ci_jobs = jobs(ci)
        self.assertIn("msrv", ci_jobs)
        self.assertIn("cargo check --locked --all-targets", ci_jobs["msrv"])
        self.assertNotIn("continue-on-error:", ci)
        for command in ("cargo fmt --all -- --check", "cargo check --all-targets --locked",
                        "cargo clippy --all-targets --locked -- -D warnings",
                        "cargo test --all-targets --locked", "cargo deny check",
                        "python3 -B -m unittest discover -s scripts/tests -v"):
            self.assertIn(command, ci_jobs["rust"])
        self.assertIn("uses: actions-rust-lang/audit@", ci_jobs["rust"])
        self.assertIn("denyWarnings: true", ci_jobs["rust"])
        for block in re.split(r"^      - name:", ci_jobs["rust"], flags=re.MULTILINE)[1:]:
            self.assertNotIn("\n        if:", block, "a required CI step must not be conditional")

    def test_tagged_revision_must_pass_tests_security_and_msrv_before_any_publication(self):
        self.assert_gate((ROOT / ".github/workflows/release.yml").read_text(),
                         (ROOT / ".github/workflows/ci.yml").read_text())

    def test_gate_check_rejects_removed_dependencies_or_security_checks(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        for broken_release, broken_ci in (
            (release.replace("needs: [validate]", "needs: []"), ci),
            (release.replace("always() && needs.validate.result == 'success' &&", "always() &&"), ci),
            (release, ci.replace("cargo deny check", "echo skipped")),
            (release, ci.replace("denyWarnings: true", "denyWarnings: false")),
            (release, ci.replace("run: cargo test", "if: false\n        run: cargo test")),
        ):
            with self.assertRaises(AssertionError):
                self.assert_gate(broken_release, broken_ci)


if __name__ == "__main__":
    unittest.main()
