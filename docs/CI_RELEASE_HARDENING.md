# CI and release hardening review

Ticket: CFA-100 (roadmap phase 3, EPIC 6, CI-01). Analysis only: no workflow,
release or repository setting was changed. Baseline: `origin/main` `536e424`
(v0.4.0 plus the GitHub Actions bumps #43–#45), checked on 2026-10-15.

Repository files were read directly; server-side settings (branch protection,
rulesets, code scanning, workflow token defaults) were read through the GitHub
REST API with a read-only call. Line numbers refer to the baseline.

> **Update 2026-10-17:** F1–F3 are resolved. Releases now publish only from
> manually pushed version tags, the tag trigger is narrowed, and token
> permissions are job-scoped. See [Applied hardening](#applied-hardening-2026-10-17).
> The findings below describe the audited baseline.

## Summary

| # | Area | Result | Risk |
| --- | --- | --- | --- |
| F1 | Auto-tag on `main` commits named `Release v…` publishes a release | resolved 2026-10-17 | — |
| F2 | Release tag trigger matches any tag containing `x.y.z` | resolved 2026-10-17 | — |
| F3 | `contents: write` granted workflow-wide in `release.yml` | resolved 2026-10-17 | — |
| F4 | Branch protection on `main` | present, review count 0 | low |
| F5 | Cargo audit in the required `rust` job | present | — |
| F6 | CodeQL | present (default setup) | — |
| F7 | Dependabot (cargo, github-actions) | present | — |
| F8 | Checksums and build attestations | present | — |
| F9 | CLI smoke tests | present | — |
| F10 | Reproducible release builds | source-reproducible only | low |
| F11 | Pull-request runs of the release workflow | plan only, never publish | — |
| F12 | `allow-dirty = ["ci"]` in `dist-workspace.toml` | finding | low |

Of these, only F1, together with F2, works against the acceptance criterion "no
release automation that allows an unintended publication". The existing security
checks (F5–F9, F11) are kept unchanged.

## Findings

### F1 — `Release v…` commit on `main` tags and publishes without a second gate

`.github/workflows/release.yml:10-16` runs on every push to `main`. The `prepare`
job (`:53-89`) acts on a `main` commit whose message starts with `Release v`. It
reads the version from `Cargo.toml`, creates the annotated tag `vX.Y.Z` with the
workflow token and pushes it, then sets `publishing=true`. The `host` job then
publishes the release with `gh release create` (`:310-323`).

The ruleset allows only squash and rebase merges. A squash merge of a PR whose
title starts with `Release v` therefore publishes a release, and no further
approval is involved. This is the documented release path (v0.1.1–v0.2.4 were
published this way). No `environment:` with required reviewers protects it.

Options, for the maintainer to decide (not implemented here):

- put `host` behind a protected `environment: release` with a required reviewer;
- or drop the auto-tag branch and publish only on manually pushed `v*` tags.

### F2 — tag trigger is broader than the version scheme

`release.yml:16` uses `'**[0-9]+.[0-9]+.[0-9]+*'`, which also matches
`foo1.2.3bar`. `prepare` sets `publishing=true` for every tag push (`:48-53`)
without checking that the tagged commit is on `main`. No tag ruleset exists. Any
account allowed to push tags can therefore publish from any commit.

Recommendation: narrow the pattern to `v[0-9]+.[0-9]+.[0-9]+*`. Add a tag ruleset
that restricts the creation of `v*` tags to maintainers.

### F3 — workflow-wide write permission

`release.yml:7-8` grants `contents: write` to every job. Only
`build-local-artifacts` narrows its permissions (`:149-152`: `contents: read`,
`attestations: write`, `id-token: write`). The repository default for
`GITHUB_TOKEN` is already `read`.

Recommendation: declare `contents: read` at workflow level and
`contents: write` only on `prepare` and `host`.

### F4 — branch protection (server side, verified)

`main` is protected twice:

- by classic branch protection, with the required status check `rust`
  (strict/up to date), `enforce_admins` on and force pushes off;
- by the active ruleset "Protect main", with deletion and non-fast-forward
  blocked, linear history required, merges limited to squash or rebase, the
  required check `rust` (strict) and PRs required.

The ruleset lets repository admins bypass it (`bypass_mode: always`). The
required approving review count is `0`, so GitHub does not enforce an
independent review. A review happens only by process. There is no `CODEOWNERS`.

Recommendation: no change is required for a single-maintainer repository.
If outside contributors are expected, require one approving review on `main`.

### F5 — Cargo audit (positive)

`.github/workflows/ci.yml:38-42` runs `actions-rust-lang/audit`, pinned by SHA,
with `denyWarnings: true` inside the required `rust` job. A new RustSec advisory
fails CI.

### F6 — CodeQL (positive, verified)

The repository contains no `codeql*.yml`, but code scanning default setup is
`configured` for `actions` and `rust` with the default query suite. Open
code-scanning alerts at review time: 0.

### F7 — Dependabot (positive)

`.github/dependabot.yml` schedules weekly updates for `cargo` and
`github-actions`. Open Dependabot alerts at review time: 0.

### F8 — artifacts, checksums, attestations (positive)

`dist-workspace.toml` sets `checksum = "sha256"` and
`github-attestations = true`. `release.yml:195` attests with `actions/attest`.
`SECURITY.md` and the README document how to check a download with
`sha256sum -c` and `gh attestation verify`. Artifacts are not GPG-signed; the
documentation says so.

### F9 — CLI smoke tests (positive)

`ci.yml:44-49` runs `cargo run --locked -- --help` and
`cargo run --locked -- diag` on a headless runner.

### F10 — reproducibility claim

`Cargo.lock` is committed, every CI step uses `--locked`, and
`[profile.dist]` inherits `release` with `lto = "thin"`. The build sets no
`--remap-path-prefix` or `SOURCE_DATE_EPOCH`, and no bit-identical double build
has been recorded. The accurate claim is therefore *source-reproducible*
(pinned dependencies). A bit-identical build is not established.

### F11 — PR runs never publish (positive)

For `pull_request`, `prepare` exits with `publishing=false` (`release.yml:44-46`).
`dist-workspace.toml` sets `pr-run-mode = "plan"`, so `build-local-artifacts`
(`:140`) and the publish jobs are skipped.

### F12 — `allow-dirty = ["ci"]`

This setting lets `dist` regenerate or ignore drift in the generated workflow
files. Hardening applied by hand to `release.yml` (F1–F3) could be lost on the
next `dist generate`. Any such change must be re-checked after a `dist` upgrade.

## Applied hardening (2026-10-17)

- **F1 resolved.** The auto-tag path on `main` was removed. A push to `main`
  only resolves `publishing=false`; releases are published only from an
  explicitly pushed version tag.
- **F2 resolved.** The tag trigger is narrowed to
  `v[0-9]+.[0-9]+.[0-9]+*`. A tag ruleset protects `refs/tags/v*` against
  deletion and non-fast-forward updates, with no bypass actor.
- The manual tag-triggered publish has not been exercised since the policy
  change. On the next release, confirm `prepare`, `plan` and `host` run and the
  tag/release is created. Do not use a throwaway `v*` tag: tag deletion is now
  protected and the run would publish a release.
- **F3 resolved.** The workflow default is `contents: read`. Write access is
  scoped to `plan` and `host`; `prepare` no longer needs it.
- The cargo-dist installer step no longer receives `GH_TOKEN`; the token is
  passed only to the `plan` step that invokes `dist host --steps=create`.
- GitHub Actions policy is `selected`: GitHub-owned actions plus SHA-pinned
  `dtolnay/rust-toolchain`, `Swatinem/rust-cache` and
  `actions-rust-lang/audit`. SHA pinning is required repository-wide.
- The release plan job downloads the pinned cargo-dist Linux archive, verifies
  its published SHA-256 checksum, and installs that binary directly. No
  installer script runs there.
- Still open: the build-matrix install step still uses the generated
  `matrix.install_dist.run` command (read-only token), the `announce` job is
  empty because no announcement target is configured, and secret-scanning
  non-provider patterns/validity checks could not be enabled on the current
  plan.
