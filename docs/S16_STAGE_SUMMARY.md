# Stage Summary — S16 (llama.cpp server → GPU mapping)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/domain.rs` | `GpuVendor` moved here from `discovery.rs` (it is a normalized domain concept, now defined beside `DeviceId` and imported by discovery). New types: `GpuEvidence` (`NvmlCompute` / `RenderNodeFd`, kept for S18 diagnostics), `MappedGpu { device, name, vendor, evidence }` with `key()`, and `GpuMapping { None (default), Single(MappedGpu), Multi(Vec<MappedGpu>), Unknown }` with `is_empty()` / `len()`. `FastSnapshot` and `DashboardSnapshot` gained `pub gpu_map: GpuMapping`. New test `gpu_mapping_states_are_explicit` (1). |
| `src/gpu_map.rs` (new) | The evidence/combine module. `bdf_from_by_path_name` (parses `pci-<BDF>-render`/`-card`), `is_bdf`, `process_render_gpus<S: Sys>` (reads `/proc/<pid>/fd` via `Sys::read_dir` + `Sys::symlink_target`; no sysfs joins), `nvml_compute_gpus(Option<&Nvml>, pid)`, `combine_gpus(render, nvml, gpus) -> GpuMapping` (union by stable key, render evidence wins, sorted by key), `map_server_gpus(server_running, …)` (running gate + empty→Unknown). 12 fixture tests. |
| `src/discovery.rs` | Local `GpuVendor` definition removed; now `use crate::domain::{DeviceId, GpuVendor};`. Public surface unchanged. |
| `src/discovery_llm.rs` | New `pub fn selected_endpoint_pid(candidates, selected_endpoint) -> Option<u32>`: maps the *selected* endpoint back to its owning local PID (`Configured`/remote → `None`). PID is an attribution input only — never a selection criterion. |
| `src/main.rs` | New `pub(crate) fn resolve_server_full(settings) -> (String, Option<u32>)` scans `/proc` once and returns endpoint + PID; `resolve_server`/`resolve_server_pid` are thin wrappers. `main()` resolves the PID before terminal init and passes it to `app::run`. |
| `src/app.rs` | `run()` gained `server_pid: Option<u32>`; a `mpsc::channel::<Option<u32>>` (initial value sent up front, resent on settings edits) feeds the fast worker. `spawn_fast_worker` drains the channel each cycle, caches `static_gpus = discover_gpus(&RealSys)` and `Nvml::init().ok()` once, and recomputes `gpu_map` on the process-refresh cadence into `FastSnapshot.gpu_map`. The main loop merges `snapshot.gpu_map = next.gpu_map`. |
| `src/system.rs` | `#[allow(dead_code)]` removed from `Sys::symlink_target` (now used by `gpu_map.rs`). |
| `docs/S16_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Explicit mapping states, never a guess.** `GpuMapping` is an explicit enum —
  `None` (not computed yet / no local process to attribute), `Single`, `Multi`,
  `Unknown` (process is local but no GPU could be attributed, or not running).
  The mapping is only *determined* when the server PID appears in the worker's
  process table with a `start_time > 0` — S9's `pid + start_time` identity guards
  against PID reuse after the server restarts. A configured/remote endpoint has no
  local process, so it stays `None` (there is simply nothing to attribute), which is
  distinct from `Unknown` (we tried, found nothing).
- **Two independent evidence sources, unioned by stable key.**
  - *Render-node (DRM) path, vendor-neutral:* the last path component of each
    `/proc/<pid>/fd/*` target is either a stable render-node name (`renderD<N>`,
    matched against the discovered GPU's `render_node`) or a by-path name whose PCI
    BDF is embedded (`pci-0000:01:00.0-render`). No sysfs re-read is required — the
    fd target carries the identity. This is the path that works on AMD/Intel.
  - *NVML path, NVIDIA-authoritative:* each NVML device is asked
    `running_compute_processes()`; a PID match yields `pci_info().bus_id` (BDF) or the
    UUID. A CUDA process is reported on exactly the device(s) it has allocated, so
    this also captures MIG placements (S17). All NVML calls are `.ok()`/`Result`-wrapped;
    a non-NVIDIA host simply yields no NVML evidence and degrades to render-only.
  - `combine_gpus` unions both by stable key (BDF, else UUID). On conflict the
    render-node evidence wins (the more specific, filesystem-derived signal). Unkeyed
    devices get unique slots so they are never merged. Output is sorted by key and
    folded into `None` (0) / `Single` (1) / `Multi` (>1).
- **Where the work lives.** The mapping is computed in the **fast worker** (it owns the
  process table and the process-refresh cadence), not the render loop or the LLM
  worker. Static topology (`discover_gpus`) and the NVML handle are cached once; the
  evidence recompute is O(open fds + NVML devices) per process refresh, with no
  allocation-heavy work in rendering. The result flows worker → `FastSnapshot.gpu_map`
  → `DashboardSnapshot.gpu_map`, ready for the UI.
- **PID as attribution input, never selection.** The endpoint is still chosen by S15's
  port-based rule. `resolve_server_full` returns the PID *of the selected endpoint* and
  the app forwards it to the worker over a channel (initial value + a resend on each
  settings edit, so a server change re-attributes without restarting the worker).
- **Domain types live in `domain.rs`, evidence logic in `gpu_map.rs`.** `GpuVendor`,
  `GpuEvidence`, `MappedGpu` and `GpuMapping` are normalized domain concepts beside
  `DeviceId`; `gpu_map.rs` holds only the evidence/combine functions and their tests.
  No re-exports were needed; discovery now imports `GpuVendor` from the domain.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **139 passed, 0 failed, 1 ignored** (was
126; +13 = 12 in `gpu_map.rs` + 1 in `domain.rs`). New coverage in `gpu_map::tests`:
`render_fd_maps_process_to_single_gpu`, `render_fd_by_render_node_name_maps_to_gpu`,
`no_open_render_fd_yields_no_evidence`, `process_without_fd_dir_is_unknown`,
`server_not_running_is_unknown_even_with_evidence`, `multi_gpu_when_process_opens_two_render_nodes`
(2 render nodes → `Multi`, sorted by BDF), `nvml_evidence_alone_maps_single_gpu` (BDF),
`nvml_uuid_evidence_maps_single_gpu` (UUID, `vendor == Unknown`),
`render_and_nvml_agree_on_same_device` (dedup, render evidence wins),
`nvml_multi_device_yields_multi`, `combine_dedupes_by_stable_key`,
`bdf_parser_rejects_malformed_names`. Domain test
`gpu_mapping_states_are_explicit` pins the explicit-state contract (`Default == None`,
`None`/`Unknown` are `is_empty()`, `len()` is `Some(0)`/`None`/`Some(n)`, key == BDF).
`cargo test -- --ignored discovery_real` still passes on the real machine (Intel xe).
Binary smoke: `main()` runs the new `resolve_server_full` → `discover_processes(&RealSys)`
path before terminal init and reaches only the pre-existing ENXIO, confirming the real
`/proc` scan + `Nvml::init()` degradation does not panic when no server is running.

## 4. Known limitations

- **The NVML half is not exercised on this machine.** The development host has no
  NVIDIA GPU (Intel `xe` `0000:00:02.0`), so `Nvml::init()` fails and the NVML path
  degrades to empty exactly as designed. The NVML branch is verified by inspection and
  by the fixture tests that drive `combine_gpus`/`map_server_gpus` with NVML-shaped
  inputs; a real NVIDIA machine (or a future NVML provider fixture) would close the
  gap. All NVML calls are wrapped so this path cannot panic or take down the TUI.
- **Display is intentionally deferred to S23.** S16 delivers the *determination* logic,
  the explicit states, and the full data path (worker → `FastSnapshot.gpu_map` →
  `DashboardSnapshot.gpu_map`). The LLM panel is exactly 12 fixed rows with zero
  headroom, and the goal explicitly assigns "active inference GPU mapping where known"
  *rendering* to S23, so `ui::draw` was not changed — the mapping is carried but not yet
  drawn. `MappedGpu::key` / `GpuMapping::{is_empty,len}` carry `#[allow(dead_code)]`
  until S23/S18 consume them (same pattern as the `Metric` API).
- **Render-node attribution is by fd target name, not full fdinfo.** S16 resolves a
  process's GPU by the *name* of each open fd's target (by-path BDF or `renderD<N>`).
  The full DRM **fdinfo** process telemetry the goal lists (engine usage, memory
  accounting, per-client counters) is deliberately S8's scope; S16 only needs *which*
  device the process holds open, which the fd target already encodes.
- **`server_running` relies on the worker's process table.** A PID is considered live
  only while it appears in `process_stats` with `start_time > 0`. Between process
  refreshes (the process-refresh cadence, not the UI cadence) the mapping can lag one
  cycle behind a server start/stop — acceptable for a mapping panel and consistent with
  S9's identity design.
- The TUI cannot be smoke-tested in this headless shell (no TTY); the ENXIO
  terminal-init error is pre-existing and unrelated.

## 5. Newly discovered issues

- **Clippy `match_result_ok` on `Result::ok()` inside `if let Some`.** Writing
  `if let Some(uuid) = device.uuid().ok()` trips `clippy::match_result_ok` under
  `-D warnings`. The idiomatic fix is matching the `Result` directly
  (`if let Ok(uuid) = device.uuid()`), which is cleaner and drops the redundant
  `ok()`. The sibling `pci_info().ok().map(…)` is fine (a `.map` after `.ok()`, not a
  direct `Some` match).
- **`GpuVendor` is a domain concept, not a discovery detail.** It originally lived in
  `discovery.rs`; S16's `MappedGpu` (a domain type) needs it, so it moved to
  `domain.rs` beside `DeviceId`, with discovery re-importing it. No public change.
- **`Sys` already had the seam for the fd scan.** `read_dir` + `symlink_target`
  (the latter previously `#[allow(dead_code)]`) cover the `/proc/<pid>/fd` walk
  without any new trait method — the DRM-discovery fixture machinery now doubles for
  process→GPU attribution, and `process_render_gpus` is fully fixture-testable.
- **The worker is the right home for the mapping.** The process table (with
  `start_time`) only exists in the fast worker, so the `server_running` gate and the
  per-refresh recompute belong there. A tiny `Option<u32>` channel (drained each
  cycle, resent on settings edits) carries the PID from the UI/settings thread without
  coupling the two.
