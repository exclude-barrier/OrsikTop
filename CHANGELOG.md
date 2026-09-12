# Changelog

All notable changes to OrsikTop will be documented here.

## [Unreleased]

### Added

- Rust/ratatui terminal UI for local LLM monitoring.
- llama.cpp `/metrics` support using current Prometheus metric names.
- llama.cpp `/slots` support for live slot and context telemetry.
- Cached `/props` metadata lookup, including configured slot-count fallback.
- Live prompt-processing and generation throughput from counter deltas.
- Average prompt/generation throughput from monotonic token/time counters with legacy gauge fallbacks.
- Prompt, prompt-cache and generated token counters.
- Active/deferred request and slot monitoring.
- Sampling-window speculative decoding / MTP acceptance telemetry.
- Direct NVIDIA NVML telemetry without per-refresh `nvidia-smi` subprocesses.
- GPU utilization, VRAM, clocks, temperature, power, P-state, fan, encoder/decoder and PCIe throughput.
- `--gpu-index` / `ORSIKTOP_GPU_INDEX` for selecting the monitored NVIDIA GPU.
- Fixed 60-second pixel history for GPU, VRAM, CPU and RAM.
- Linux CPU and RAM monitoring.
- Configurable 100–10,000 ms refresh interval.
- Clickable `[ - ]` and `[ + ]` refresh controls plus keyboard shortcuts.
- Separate fast GPU/system and llama.cpp telemetry workers so HTTP latency cannot delay GPU sampling.
- Unit tests and llama.cpp fixtures for metrics and slot telemetry.
- GitHub Actions CI for format, build, Clippy, tests and CLI smoke testing.
- Release validation that requires the Git tag to match the Cargo package version.

### Changed

- `/slots` failures are represented as unavailable instead of misleading `0/0` slot telemetry.
- Prometheus labels are preserved instead of implicitly collapsing all same-name series.
- Unsupported NVML values render as unavailable instead of false zeroes.
- PCIe throughput follows NVML's documented KB/s source unit and is displayed as MB/s.
- Fan telemetry no longer clamps valid NVML readings above 100%.
- CPU/RAM refresh uses only the required sysinfo data instead of `refresh_all()`.
- The footer is anchored to the bottom of the terminal and the offline LLM panel is more compact.
- Terminal state restoration is guarded so raw mode, mouse capture and cursor state are restored on errors/unwind.

### Scope

The first release intentionally targets Linux + NVIDIA + llama.cpp only.
