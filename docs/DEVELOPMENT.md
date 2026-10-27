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
| [`src/app.rs`](../src/app.rs) | App loop skeleton, snapshot application, refresh constants and re-exports for the app submodules |
| [`src/app/`](../src/app/) | App submodules: `channels.rs` (non-blocking channel drains, drop counting, bounded worker join), `workers.rs` (fast system/GPU worker and LLM poll worker), `loop_events.rs` (snapshot pumping, key and mouse handling), `process.rs` (process stats collection) and `server.rs` (server-set reconciliation, identity and poll slots) |
| [`src/llama.rs`](../src/llama.rs) | Single-endpoint monitor, telemetry normalization and rate derivation |
| [`src/llama/`](../src/llama/) | HTTP, the `LlamaMonitor` state machine (monitor), metrics, props, slots and speculative-decoding helpers |
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
| [`src/ui.rs`](../src/ui.rs) and [`src/ui/`](../src/ui/) | UI state, dialogs, per-panel rendering and histories |
| [`src/ui/draw.rs`](../src/ui/draw.rs) | Dashboard layout shell: full/compact/too-small draw paths, header, LLM/system, bottom and footer panels |
| [`src/ui/dialogs.rs`](../src/ui/dialogs.rs) | Help/settings popup rendering |
| [`src/ui/llm.rs`](../src/ui/llm.rs) | LLM panel rendering: rows, metrics, unavailable-value fallbacks |
| [`src/ui/processes.rs`](../src/ui/processes.rs) | Process panel rendering: grouping, filtering, row targets |
| [`src/ui/state_*.rs`](../src/ui/) | `UiState` behavior in four domains: `state_shell.rs` (drop counters, help), `state_llm.rs` (histories, server selector), `state_process.rs` (navigation, selection, search, clicks) and `state_settings.rs` (settings dialog, server-list editor) |
| [`src/ui/tests/`](../src/ui/tests/), [`src/app/tests/`](../src/app/tests/) and [`src/llama/tests/`](../src/llama/tests/) | Test modules split by unit under test; shared test helpers live in each `tests/mod.rs` |

The UI helpers cover dialogs, GPU, LLM, processes, system, history and shared widgets. Keep focused parsing and rendering helpers in the corresponding existing submodule rather than growing another monolith. `UiState` behavior lives in the `state_*.rs` modules as separate `impl UiState` blocks: add a method to the block of its domain, and if it is called from outside that module, give it `pub(crate)`. Types shared between `ui.rs` and a state module are `pub(super)` in the state module and re-imported in `ui.rs` so submodules and tests keep their existing `super::` paths. The app submodules follow the same rule: helpers moved out of `app.rs` are `pub(super)` and re-imported there so the app loop, sibling modules and the test modules keep their existing `super::` paths, and names only the tests need stay behind `#[cfg(test)]`. Tests follow the same pattern: add tests to the matching theme file under `tests/`, and put fixtures or helpers used by more than one theme file in that `tests/mod.rs`.

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

The README screenshots show real TUI output, not fixture-fed metrics. Both captures run OrsikTop on `test-desktop` during llama.cpp inference with Qwen3.8-27B-UD-Q4_K_M on an NVIDIA GeForce RTX 4090. The existing `startllm` launcher was started with explicit operator approval; its model, context, MTP and parallel settings were not changed. A bounded inference request supplied live traffic. GPU, CPU, RAM and process readings belong to the same host as the monitored server. The selector capture shows one real endpoint, not a synthetic multi-server setup.
