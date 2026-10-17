# Development

[Back to README](../README.md)

OrsikTop is a Rust TUI built with ratatui and crossterm. It reads Linux system interfaces and supported inference-server HTTP telemetry. No daemon, database or inference-server write path is required.

## Build from source

Use a current stable Rust toolchain on Linux:

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo build --release --locked
./target/release/orsiktop
```

For an installed development build, see [Cargo installation](INSTALLATION.md#cargo-installation). Prebuilt release targets and packaging are configured in [`dist-workspace.toml`](../dist-workspace.toml).

## Source map

| Area | Responsibility |
| --- | --- |
| [`src/main.rs`](../src/main.rs) | Terminal setup, CLI overrides, server-set assembly, subcommands and install lifecycle |
| [`src/config.rs`](../src/config.rs) | Persistent runtime settings and atomic saves |
| [`src/domain.rs`](../src/domain.rs) | Vendor-neutral telemetry models, GPU identity and server identity |
| [`src/app.rs`](../src/app.rs) | Event loop, worker lifecycle, snapshots, selected-server state and runtime synchronization |
| [`src/llama.rs`](../src/llama.rs) | Single-endpoint monitor, telemetry normalization and rate derivation |
| [`src/llama/`](../src/llama/) | HTTP, metrics, props, slots and speculative-decoding helpers |
| [`src/strata.rs`](../src/strata.rs) | Strata JSON telemetry adapter |
| [`src/discovery_llm.rs`](../src/discovery_llm.rs) | Local llama.cpp process discovery and configured endpoints |
| [`src/discovery.rs`](../src/discovery.rs) | Linux DRM/sysfs GPU discovery |
| [`src/gpu.rs`](../src/gpu.rs) and [`src/providers/`](../src/providers/) | GPU selection, provider dispatch, PCIe capacity math and NVIDIA/AMD/Intel sampling |
| [`src/gpu_map.rs`](../src/gpu_map.rs) | Evidence-based local server-to-GPU association |
| [`src/drm.rs`](../src/drm.rs) | Per-process GPU utilization and memory from DRM fdinfo |
| [`src/cpu.rs`](../src/cpu.rs) and [`src/cpu_sensors.rs`](../src/cpu_sensors.rs) | CPU topology, frequency, temperature and RAPL power |
| [`src/diagnostics.rs`](../src/diagnostics.rs) and [`src/redact.rs`](../src/redact.rs) | Bug-report diagnostics and shared display-safe endpoint rendering |
| [`src/server_editor.rs`](../src/server_editor.rs) | Pure additional-endpoint validation, identity, monitoring flags and masking helpers |
| [`src/system.rs`](../src/system.rs) | Filesystem access for real systems and fixtures |
| [`src/ui.rs`](../src/ui.rs) and [`src/ui/`](../src/ui/) | UI state, layout, dialogs, per-panel rendering and histories |

The UI helpers cover dialogs, GPU, LLM, processes, system, history and shared widgets. Keep focused parsing and rendering helpers in the corresponding existing submodule rather than growing another monolith.

## Runtime boundaries

The UI consumes snapshots from a fast system/GPU/process worker and an LLM coordinator. The coordinator polls a bounded set of servers concurrently, with a separate monitor and counter baseline per endpoint. HTTP sampling stays off the UI thread, so slow endpoints do not block keyboard or mouse input. Generation-tagged samples prevent old server-set results from appearing under newly selected configuration.

GPU discovery uses stable PCI BDF identity and vendor-specific providers. Server-to-GPU mapping requires local process evidence, not a matching model name or device-wide utilization. Sensor and LLM fields preserve unavailable values as `—`; the current history-header limitation is documented under [Reading telemetry](USAGE.md#reading-telemetry).

For user-visible contracts, use [Usage](USAGE.md), [Hardware support](HARDWARE_SUPPORT.md) and [Backend versions](BACKEND_VERSIONS.md). The [live vs fixture matrix](LIVE_VS_FIXTURE.md) distinguishes implemented paths from real-hardware or backend verification. Do not turn fixture coverage into a broad live-support claim.

## Validation

CI runs the following Rust checks:

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

CLI smoke commands:

```bash
cargo run --locked -- --help
cargo run --locked -- --version
cargo run --locked -- diag
```

`diag` is read-only and tolerates missing GPUs or unreachable LLM telemetry. For TUI changes, also launch the dashboard, exercise the changed controls and inspect the rendered result. Do not start, stop or reconfigure an existing inference server just to verify monitoring.

Opt-in hardware smokes are ignored by the default suite. Their evidence and remaining hardware gaps are recorded in [Hardware support](HARDWARE_SUPPORT.md) and the [hardware test matrix](HARDWARE_TEST_MATRIX.md). Passing fixture tests alone does not prove behavior on another GPU or server build.

## Documentation changes

Keep the README focused on the first run and everyday use. Installation details belong in [Installation](INSTALLATION.md), controls and metric meanings in [Usage](USAGE.md), and source responsibilities here. Link the existing evidence matrices for hardware and backend limits rather than duplicating their detailed tables.

The README screenshots show real TUI output, not fixture-fed metrics. Both captures run OrsikTop on `cf-desktop` during llama.cpp inference with Qwen3.8-27B-UD-Q4_K_M on an NVIDIA GeForce RTX 4090. The existing `startllm` launcher was started with explicit operator approval; its model, context, MTP and parallel settings were not changed. A bounded inference request supplied live traffic. GPU, CPU, RAM and process readings belong to the same host as the monitored server. The selector capture shows one real endpoint, not a synthetic multi-server setup.
