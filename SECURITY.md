# Security Policy

## Supported versions

OrsikTop is currently early-stage software. Security fixes are provided for the latest released version and the current `main` branch.

| Version | Supported |
| --- | --- |
| 0.1.x | Yes |
| < 0.1 | No |

## Reporting a vulnerability

Please do **not** report security vulnerabilities in a public GitHub issue.

Use GitHub's private vulnerability reporting / Security Advisory flow for this repository when available. This allows the issue to be discussed privately until a fix is ready.

A useful report should include:

- the affected OrsikTop version or commit
- the operating system and relevant environment details
- a clear description of the vulnerability and potential impact
- reproduction steps or a minimal proof of concept, when safe to provide
- any suggested mitigation or fix, if known

Please avoid including secrets, credentials, private model data, or unrelated personal information in a report.

## Response and disclosure

Security reports will be reviewed as soon as practical. The goal is to confirm receipt within 7 days and, where possible, provide an initial assessment within 14 days.

Please allow time for investigation and a coordinated fix before publicly disclosing a vulnerability. Confirmed security issues may be documented in a GitHub Security Advisory and release notes after a fix is available.

## Scope

This policy covers vulnerabilities in OrsikTop itself, including its Rust code, release workflow, configuration handling, process discovery, telemetry parsing, and interactions with configured LLM endpoints.

Issues that exist solely in third-party components such as llama.cpp, NVIDIA drivers/NVML, the Linux kernel, Rust dependencies, or the operating system should normally be reported to the respective upstream project. If OrsikTop uses a third-party component in a way that creates an additional security problem, that is in scope for this project.

## Release integrity

Official release artifacts are published through the GitHub Releases page. Release archives include a matching `.sha256` checksum file so downloads can be verified before use.

Example:

```bash
sha256sum -c orsiktop-x86_64-unknown-linux-gnu.tar.xz.sha256
```

Release artifacts are also covered by GitHub artifact attestations, which provide cryptographic evidence of where and how each artifact was built. Attestations are attached to the release assets and can be verified locally with:

```bash
gh attestation verify orsiktop-x86_64-unknown-linux-gnu.tar.xz \
  --repo exclude-barrier/OrsikTop
```

At this time, release artifacts are checksummed and attested but not GPG-signed.
