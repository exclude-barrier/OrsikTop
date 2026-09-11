# Changelog

All notable changes to OrsikTop will be documented here.

## [Unreleased]

### Added

- Rust/ratatui terminal UI for local LLM monitoring.
- llama.cpp `/metrics` support using current Prometheus metric names.
- llama.cpp `/slots` support for live slot and context telemetry.
- Cached `/props` metadata lookup.
- Live and average prompt/generation throughput.
- Prompt and generated token counters.
- Active/deferred request and slot monitoring.
- Speculative decoding / MTP acceptance telemetry.
- Direct NVIDIA NVML telemetry without per-refresh `nvidia-smi` subprocesses.
- GPU utilization, VRAM, clocks, temperature, power, P-state, fan, encoder/decoder and PCIe throughput.
- Pixel-style GPU history and utilization display.
- Linux CPU and RAM monitoring.
- Configurable 100–10,000 ms refresh interval.
- Clickable `[ - ]` and `[ + ]` refresh controls plus keyboard shortcuts.
- Background telemetry worker so network polling cannot block the TUI event loop.
- Unit tests for llama.cpp parsing, slot context calculation, refresh controls and telemetry helpers.
- GitHub Actions CI for build and tests.

### Scope

The first release intentionally targets Linux + NVIDIA + llama.cpp only.
