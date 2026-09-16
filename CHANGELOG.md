# Changelog

All notable changes to OrsikTop will be documented here.

## [0.2.0] - 2026-09-16

### Added

- AMD GPU/APU telemetry backend (`src/providers/amd.rs`) reading the amdgpu
  sysfs and its hwmon: utilization, memory bandwidth, VRAM total/used, package
  temperature, power draw, GPU/memory clocks, and fan.
- Intel GPU/APU telemetry backend (`src/providers/intel.rs`) reading the i915 and
  xe kernel sysfs plus the Intel hwmon: graphics clock (MHz) and throttle reason
  from the GT sysfs, package temperature and the PL1 power limit from the hwmon.
  `new_gpu_provider` now dispatches NVIDIA → NVML, AMD → amdgpu sysfs, Intel →
  i915/xe sysfs, choosing the backend from the *discovered device's* vendor.
- Process-level GPU telemetry via DRM fdinfo (`src/drm.rs`): for every process
  holding a DRM render node, per-engine utilization (i915/amdgpu busy-nanoseconds
  over wall time; xe busy/total cycles) and resident GPU memory, one entry per
  PCI BDF. The wide process table gains a `GPU` resident-memory column (`—` when
  the process has no DRM fd); compact mode is unchanged.
- Intel low-power E-core (LP-E) classification: the kernel's `cpu_lowpower`
  perf-PMU mask (mutually disjoint from `cpu_core`/`cpu_atom`) now yields a
  distinct `LowPower` core kind, counted in the new `low_power_cores`
  topology field. The CPU heatmap marks LP cores with an `L` suffix and the
  panel title appends `+{n}L` when LP cores are present; P/E-only boxes are
  unchanged.
- CPU sensor telemetry (`src/cpu_sensors.rs`): the SYSTEM panel now reports CPU
  frequency, temperature, and package power. Frequency is the cpufreq
  `scaling_cur_freq` weighted average across policies (falling back to
  `/proc/cpuinfo` `cpu MHz`); temperature is the CPU hwmon package/Tctl reading
  (per-core fallback); power is the RAPL `intel-rapl` package zone read twice a
  short window apart. Sensor discovery runs once and is cached; samples only
  re-read the dynamic counters. An unavailable metric (no cpufreq, no CPU
  hwmon, or a root-only `energy_uj` counter) reports an explicit `None` and
  renders as `—`, never a fake zero. `SystemStats` gains `cpu_power_w`.
- `orsiktop diag` subcommand: a human-readable, no-secrets diagnostics dump for
  bug reports. It prints CPU topology (vendor/model/logical/physical, P/E/LP-E
  classes, hybrid state, per-core kinds), DRM GPU discovery (card, BDF,
  vendor/device IDs, PCI class, driver, render node, outputs), the one-shot
  `Auto` provider capability matrix (which normalized metrics the chosen backend
  can expose vs. report `None`), CPU sensor discovery, and llama.cpp
  endpoint/server discovery plus the computed server→GPU mapping. Missing data is
  an explicit `—`/`not computed`/`unknown` state, never a fake zero; process
  evidence is PID-only (command lines are never printed), and the only network
  contact is one bounded read-only probe of the resolved llama endpoint.

### Changed

- GPU utilization, memory-bandwidth utilization, and VRAM used/total in
  `GpuStats` are now explicit `Option`s. A vendor whose driver does not expose a
  given metric (e.g. Intel has no GPU-busy counter, no VRAM figures, no
  instantaneous power, or no fan-speed maximum in sysfs) reports `None` — an
  honest "unavailable" — instead of a fake `0.0`. The UI renders these as `—`.
- Adaptive sampling: CPU temperature and RAPL power now update on a separate
  1 s cadence and are reused on the 250 ms system cycles, instead of
  resampling (and, for power, blocking ~50 ms for the RAPL two-read window)
  every 250 ms. CPU frequency, usage, load and memory keep the fast cadence;
  GPU, process-table, and llama.cpp polling cadences are unchanged. A fast UI
  refresh no longer forces the slow sensor sources to run at that rate.
- Owned worker shutdown: the two telemetry workers (system/GPU and
  llama.cpp) are now joined after `stop` is set, with a bounded (5 s) wait,
  so the process exits with both workers verified-stopped instead of detached.
  The render loop and exit path never block on a worker longer than that
  bound.
- `orsiktop update` no longer blocks forever if the `orsiktop-update` helper
  hangs: the updater now runs under a bounded 10 minute deadline (polling
  `try_wait`), and a hung process is terminated and reported instead of
  wedging the command.
- The local llama-server spec-decoding probe (CLI args + the two
  spec-decoding env vars, loopback endpoints only) is cached per
  `LlamaMonitor` and re-scanned at most every 30 s, instead of re-walking
  `/proc` on every 250 ms LLM sample.

## [0.1.4] - 2026-09-15

### Changed

- GPU telemetry now runs behind a vendor-neutral `GpuProvider` interface
  (`src/providers/`). The NVIDIA NVML backend is the first implementation;
  application code and the UI no longer reference NVML or any vendor API
  directly, which is the seam for AMD and Intel backends.
- NVML device enumeration (PCI BDF / UUID per device) is cached when the
  provider is created instead of re-enumerated on every sample.
- Each GPU sample now carries the stable device identity (PCI BDF + vendor
  UUID) used for selection; rendering it in the dashboard is a follow-up.

## [0.1.3] - 2026-09-15

### Added

- Vendor-neutral GPU discovery from Linux DRM/sysfs: PCI BDF, vendor/device
  and class IDs, bound driver, render node and connected outputs, independent of
  enumeration order.
- Stable GPU selection by PCI BDF or vendor UUID through the new `--gpu`
  option (env `ORSIKTOP_GPU`) and the in-app settings; `--gpu-index` is kept as
  a legacy alias.

### Changed

- GPU selection no longer depends on a fragile ordinal NVML index. The config
  stores a stable selector (`gpu=`) and migrates legacy `gpu_index=` values
  automatically (a `gpu=` key always wins).
- Telemetry is split into normalized domain models behind a small filesystem
  abstraction so hardware parsing is fixture-testable without physical GPUs.

- `orsiktop uninstall` removes a standalone installation
  (`~/.local/bin/orsiktop` and `~/.local/bin/orsiktop-update`) and prints
  every file it removed. `orsiktop uninstall --purge` additionally removes
  the config directory. Cargo installations are detected and answered with
  `cargo uninstall orsiktop` instead of being touched.

- README restructured around a quick start (install, start llama.cpp with
  metrics, run orsiktop), with a dedicated uninstalling section and
  condensed installation/PATH text.
- SECURITY.md release integrity section now references the actual release
  artifact names (`orsiktop-x86_64-unknown-linux-gnu.tar.xz`) and documents
  GitHub artifact attestations with a verified `gh attestation verify`
  command, replacing the outdated "not cryptographically signed" statement.

## [0.1.2] - 2026-09-14

### Fixed

- Standalone installer no longer executes `orsiktop` / `orsiktop update` during
  installation. Backticks in the `dist` install success message were embedded in
  a double-quoted shell line and expanded as command substitutions, which made
  the installer appear to hang after "installing to ~/.local/bin".

### Security

- Pinned `rustls` 0.23.45 in Cargo.lock, fixing RUSTSEC-2026-0285 (medium:
  TLS 1.3 handshake messages accepted across encryption level boundaries).
  No manifest or API changes.

## [0.1.1] - 2026-09-14

### Added

- Standalone Linux x86_64 installation through a `dist`-generated shell installer.
- Installation into `~/.local/bin` with PATH setup when required.
- Standalone `orsiktop-update` updater and the user-facing `orsiktop update` command.
- SHA-256 release checksums and GitHub artifact attestations for release artifacts.
- RustSec dependency auditing in CI, with warnings such as soundness advisories treated as failures.
- Dependabot configuration for Cargo and GitHub Actions.
- Repository security policy.

### Changed

- Release packaging and GitHub Release publishing now use `dist` 0.33.0.
- `ratatui` updated from 0.29.0 to 0.30.2, removing the vulnerable transitive `lru` version.
- `crossterm` updated from 0.28.1 to 0.29.0.
- `sysinfo` updated from 0.33.1 to 0.39.6.
- Cargo installation remains available as a developer and fallback installation path.
- CLI package description now consistently uses “Fast terminal monitoring for local LLM Orks”.

## [0.1.0] - 2026-09-13

### Added

- Fast Rust/ratatui terminal UI for monitoring local LLM inference, NVIDIA GPU telemetry, Linux system load and processes.
- llama.cpp `/metrics` support using current Prometheus metric names.
- llama.cpp `/slots` support for live slot and context telemetry.
- Cached `/props` metadata lookup, including configured slot-count fallback.
- Local llama.cpp auto discovery from running `llama-server` / `llama serve` processes.
- Configurable local or remote llama.cpp endpoint with persistent in-app settings.
- Live prompt-processing / prefill and decode / generation throughput.
- Average prompt/generation throughput from monotonic counters with legacy gauge fallbacks.
- Prompt, prompt-cache and generated token counters.
- Active/deferred request and slot monitoring.
- Sampling-window speculative decoding / MTP state and acceptance telemetry.
- 60-second prefill and decode history graphs.
- LLM connection state, uptime, smoothed polling latency and transient reconnect grace handling.
- Direct NVIDIA NVML telemetry without per-refresh `nvidia-smi` subprocesses.
- GPU utilization, VRAM, clocks, temperature, power, P-state, fan, encoder/decoder and PCIe throughput.
- Runtime GPU selection via settings and `--gpu-index` / `ORSIKTOP_GPU_INDEX`.
- 60-second GPU and VRAM history.
- Linux CPU, per-core, RAM, swap, load-average and I/O-wait monitoring.
- Intel P-core / E-core grouping when exposed by Linux.
- 60-second CPU and RAM history.
- Grouped process view for processes with the same program name.
- Expand/collapse process groups with right-click.
- Process sorting, keyboard navigation, mouse scrolling and process pinning.
- `/` process search by program, command or PID.
- Configurable 100–10,000 ms main refresh interval.
- Separate process refresh interval and offline grace period.
- Clickable `[ - ]` and `[ + ]` refresh controls plus keyboard shortcuts.
- Persistent settings for endpoint, GPU index, refresh intervals, offline grace and auto discovery.
- Help and settings overlays with mouse/keyboard controls.
- Separate fast GPU/system and llama.cpp telemetry workers so HTTP latency cannot delay GPU sampling.
- Unit tests and llama.cpp fixtures for metrics and slot telemetry.
- GitHub Actions CI for format, build, Clippy, tests and CLI smoke testing.
- Automated Linux x86_64 release workflow with checksum generation and tag/version validation.

### Changed

- `/slots` failures are represented as unavailable instead of misleading `0/0` slot telemetry.
- Prometheus labels are preserved instead of implicitly collapsing all same-name series.
- Unsupported NVML values render as unavailable instead of false zeroes.
- PCIe throughput follows NVML's documented KB/s source unit and is displayed as MB/s.
- Fan telemetry no longer clamps valid NVML readings above 100%.
- CPU/RAM refresh uses only the required sysinfo data instead of `refresh_all()`.
- Footer/help keybindings now use `Esc` to quit and `q` to open settings.
- Terminal state restoration is guarded so raw mode, mouse capture and cursor state are restored on errors/unwind.
- README now documents the current feature set, controls, settings, installation flow and includes a repository-hosted screenshot.

### Scope

The first release intentionally targets Linux + NVIDIA + llama.cpp only.
