# OrsikTop — Project Context for OMP

Compact standing rules. Long-term decisions live in Honcho, not here.

## What OrsikTop is

Small, fast Rust/ratatui TUI monitor for local LLM inference (Linux + NVIDIA
GPUs + NVML + llama.cpp). Local-only: no accounts, browsers, or daemons.

## Hard rules (YAGNI)

- YAGNI/KISS: only build what has concrete current benefit. No speculative
  architecture, no new dependencies without clear justification.
- Focus: local llama-server / llama serve processes, low monitoring overhead,
  compact terminal UI.
- No LAN discovery. Auto-discovery stays local process discovery (/proc).
- No nvidia-smi polling. Use the NVML API directly.
- Unavailable telemetry renders as "—", never as 0 or invented values.
- Telemetry errors must not crash the program (graceful degradation).
- GPU widget stays compact. No giant GPU panel.
- Refresh interval stays directly adjustable with [-] / [+] and keyboard.
- Keep monitoring overhead low: no redundant HTTP requests, /proc reads,
  NVML calls, or allocations without reason.
- Preserve existing behavior and code style unless a change is justified.

## Workflow

- Validate after change groups:
  `cargo fmt --all -- --check`, `cargo check --all-targets --locked`,
  `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --all-targets --locked`.
- Small, isolated, reviewable commits on the existing branch. No push without
  explicit approval.
