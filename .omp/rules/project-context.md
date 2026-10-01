# OrsikTop — Project Context for OMP

Compact standing rules. Long-term decisions live in Honcho, not here.

## What OrsikTop is

Small, fast Rust/ratatui TUI monitor for local LLM inference on Linux:
llama.cpp (`llama-server` / `llama serve`) plus GPU telemetry for NVIDIA,
AMD and Intel (discrete GPUs and APUs/iGPUs) plus system/process load.
Local-only: no accounts, browsers, or daemons.

## Architecture (current)

- Vendor-neutral GPU layer. `discovery.rs` finds every DRM card from
  `/sys/class/drm` + PCI sysfs; the PCI BDF is the stable identity
  (enumeration order is never identity). `gpu::new_gpu_provider` dispatches
  on the discovered vendor: NVIDIA → NVML (`providers/nvidia.rs`), AMD →
  amdgpu sysfs + hwmon (`providers/amd.rs`), Intel → i915/xe sysfs + hwmon
  (`providers/intel.rs`). The app and UI only ever see the `GpuProvider`
  trait + normalized `GpuStats` (`providers/mod.rs`); no vendor API leaks
  out. Vendors without a backend get an explicit "no backend" error, never a
  fake reading from another vendor's API.
- All filesystem access goes through the `Sys` trait (`system.rs`, RealSys /
  FixtureSys) so hardware parsing is fixture-testable without physical GPUs.
- `domain.rs` is the normalized-type re-export hub for workers, snapshots and
  UI (refresh constants, `DeviceId`, `GpuVendor`, `GpuMapping`,
  `GpuSelector`).
- Process→GPU: `drm.rs` reads DRM fdinfo per process (per-engine busy
  ns/cycles + resident memory, per PCI BDF); `gpu_map.rs` combines open DRM
  render-node fds + NVML compute processes into an explicit
  Single/Multi/Unknown mapping — evidence, never guessing.
- CPU: `cpu.rs` topology (vendor/model, P/E/LP-E core classes, physical
  cores) and `cpu_sensors.rs` (cpufreq frequency, hwmon package temperature,
  RAPL package power). Sensor discovery runs once; samples re-read only the
  dynamic counters.
- llama.cpp: `llama.rs` (`LlamaMonitor`) polls `/metrics` (Prometheus),
  `/slots` (optional), `/props` (cached). `discovery_llm.rs` auto-discovers
  the local server from `/proc` — endpoint-based selection, never by PID.
- Two worker threads (fast: system/GPU/processes; LLM: HTTP) publish
  snapshots; the UI renders only the latest snapshot. `orsiktop diag`
  (`diagnostics.rs`) prints a no-secrets state dump for bug reports.

## Hard rules (YAGNI/KISS)

- YAGNI/KISS: build only what has concrete current benefit. No speculative
  architecture, no new dependencies without clear benefit.
- Rust: idiomatic code, clean ownership/borrowing, no unnecessary
  clones/allocations, robust error handling, and no panics in telemetry
  paths — a broken sensor must degrade gracefully, never crash the TUI.
- Unavailable telemetry is `None` in the domain model and renders as `—`.
  Never a fake `0`, never an invented value. A metric the driver does not
  expose (e.g. Intel iGPU: no busy counter, no VRAM, no power, no fan) stays
  `None` — that is driver behavior, not a detection failure.
- APUs/iGPUs are first-class devices. On a machine without an NVIDIA GPU,
  legacy numeric selection falls back to the n-th device overall; on mixed
  machines it stays the n-th NVIDIA device.
- GPU identity is PCI BDF (vendor UUID secondary); `card0`/NVML ordinals are
  never identity.
- Keep the vendor seam: app/UI must not reference NVML or vendor sysfs paths
  directly. New GPU metrics land in `GpuStats` + the matching provider,
  sampled through `Sys`.
- Keep monitoring overhead low: no redundant HTTP requests, `/proc` or sysfs
  reads, or NVML calls without reason. Static topology/discovery is cached
  once; slow CPU sensors run on their own 1 s cadence; the LLM `/metrics`
  poll has a 250 ms floor regardless of UI refresh; process metadata is
  cached by (pid, start_time).
- UI: compact ratatui/crossterm TUI with keyboard + mouse (scroll, selection,
  search, pin), resize-safe (small terminals degrade gracefully), and minimal
  redraw/CPU overhead — render the latest snapshot, no extra work per frame.
- Focus: local llama.cpp processes, low monitoring overhead, compact
  terminal UI. No LAN discovery — auto-discovery stays local `/proc` process
  discovery.
- No `nvidia-smi` polling; NVIDIA goes through the NVML API directly.
- GPU widget stays compact. Refresh interval stays directly adjustable with
  [-] / [+] and the keyboard.
- Preserve existing behavior and code style unless a change is justified.

## llama.cpp telemetry

- `/metrics` must be enabled on the server; parse current Prometheus names.
  `/props` is cached (30 s); `/slots` is optional — fall back gracefully
  instead of inventing slot/context values.
- Telemetry: model + context size/usage/high-watermark, live prefill and
  decode throughput plus lifetime averages, prompt/cached/generated token
  counters, active and queued (deferred) requests, active/total slots,
  cumulative prompt/generation time, 60 s prefill/decode history, connection
  state with offline-grace (transient reconnects do not flip to offline).
- Speculative decoding / MTP: counter metrics require llama-server ≥b10700;
  the local process's CLI args/env are probed (loopback only) and re-scanned
  at most every 30 s.

## Workflow

- Validate after change groups:
  `cargo fmt --all -- --check`, `cargo check --all-targets --locked`,
  `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --all-targets --locked`.
- Small, isolated, reviewable commits on the existing branch. No push
  without explicit approval.
- Larger work stages get a summary in `docs/S*_STAGE_SUMMARY.md`.
