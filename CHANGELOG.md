# Changelog

All notable changes to OrsikTop will be documented here.

## [Unreleased]

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
