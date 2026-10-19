# Installation

[Back to README](../README.md)

Prebuilt releases target Linux `x86_64-unknown-linux-gnu`. Rust is only needed for Cargo installation or source builds. GPU drivers and inference servers are separate prerequisites for their respective telemetry; OrsikTop does not install or configure them.

## Standalone release

The recommended installer command is in the [Quick start](../README.md#quick-start). It installs the main executable and updater into:

```text
~/.local/bin/
├── orsiktop
└── orsiktop-update
```

The final executable names in the installer output indicate the files installed, not commands you need to enter. Verify the installation with:

```bash
command -v orsiktop
orsiktop --version
```

Alternatively, download the Linux archive from [GitHub Releases](https://github.com/exclude-barrier/OrsikTop/releases/latest), verify it as described under [Release integrity](#release-integrity), and extract it. Keep the updater next to the main binary if you want to use `orsiktop update`.

## PATH setup

The installer can configure future shells, but cannot change the environment of the shell that launched it. If installation succeeded but `orsiktop` returns `command not found`, check that the binary exists:

```bash
ls -l "$HOME/.local/bin/orsiktop"
```

For the current Bash or Zsh session:

```bash
export PATH="$HOME/.local/bin:$PATH"
hash -r
orsiktop --version
```

For Fish:

```fish
fish_add_path "$HOME/.local/bin"
orsiktop --version
```

If a newly opened terminal still cannot find OrsikTop, add the directory to the startup file for your shell.

### Bash

```bash
grep -qxF 'export PATH="$HOME/.local/bin:$PATH"' ~/.bashrc || \
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

### Zsh

```bash
grep -qxF 'export PATH="$HOME/.local/bin:$PATH"' ~/.zshrc || \
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

### Fish

`fish_add_path` above persists the directory for future sessions unless you explicitly request a non-persistent scope.

## Cargo installation

With a current stable Rust toolchain, install directly from GitHub:

```bash
cargo install --git https://github.com/exclude-barrier/OrsikTop --locked
```

This builds the repository revision fetched by Cargo, which can be newer than the latest published release. To install the published version instead, use its release tag, for example:

```bash
cargo install --git https://github.com/exclude-barrier/OrsikTop --tag v0.4.2 --locked
```

From a local checkout:

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo install --path . --locked --force
```

Cargo normally installs executables into `~/.cargo/bin`, or into a custom Cargo install root if configured. For the current Bash or Zsh session:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
hash -r
```

For a persistent setup, use your shell's PATH instructions above with `~/.cargo/bin` instead of `~/.local/bin`. For builds without installation, see [Development](DEVELOPMENT.md#build-from-source).

## Updating

For standalone installations:

```bash
orsiktop update
```

This launches `orsiktop-update` next to the executable and updates from GitHub Releases. If the updater is missing, reinstall through the standalone installer. OrsikTop does not run automatic background update checks.

For a Cargo installation from the repository:

```bash
cargo install --git https://github.com/exclude-barrier/OrsikTop --locked --force
```

If you pin a release tag, change the tag to the desired release and pass `--force` when reinstalling.

## Uninstalling

For standalone installations:

```bash
orsiktop uninstall
```

This removes `orsiktop` and `orsiktop-update` from the executable's directory and prints the removed paths. Saved settings remain. To also remove the config directory:

```bash
orsiktop uninstall --purge
```

The config directory is `$XDG_CONFIG_HOME/orsiktop`, or `~/.config/orsiktop` when `XDG_CONFIG_HOME` is unset. `--purge` deletes that directory, not just the config file.

An installation in the standard Cargo binary directory is detected and left in place, with instructions to use:

```bash
cargo uninstall orsiktop
```

For a custom Cargo install root, use Cargo's uninstall command with the matching root rather than the standalone uninstaller.

## Release integrity

Standalone releases are built and published through GitHub Actions using `dist`. Each release includes the Linux archive, installer, updater, source archive and SHA-256 checksums. The generated installer verifies both the application archive and the separate `orsiktop-update` binary; `sha256.sum` also lists the updater. GitHub artifact attestations are published for release artifacts.

Download the archive and its matching `.sha256` file from the same release. In the download directory, verify the archive with:

```bash
sha256sum --check orsiktop-x86_64-unknown-linux-gnu.tar.xz.sha256
```

With the GitHub CLI installed, verify an artifact attestation with:

```bash
gh attestation verify orsiktop-x86_64-unknown-linux-gnu.tar.xz --repo exclude-barrier/OrsikTop
```

A checksum checks that your download matches the published checksum; the attestation connects the artifact to its GitHub build provenance.
