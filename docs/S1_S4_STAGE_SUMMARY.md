# Stage Summary — S1..S4 (UI/domain separation → stable device identity)

Stages S1 (UI/domain separation), S2 (telemetry core + fs abstraction), S3
(vendor-neutral GPU discovery) and S4 (stable device IDs + config) are complete.
Each ended with the four gates green: `cargo fmt --check`, `cargo check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all`.

## 1. Files changed/created

| File | Stage | Change |
| --- | --- | --- |
| `src/domain.rs` (new) | S1/S2/S4 | `ProcessStats`, `SystemStats`, `FastSnapshot`, `DashboardSnapshot`, `Metric<T>`, `DeviceId`, `GpuSelector`, refresh constants. Re-exports `GpuStats`/`LlmStats`. |
| `src/system.rs` (new) | S2 | `Sys` trait (3 methods: `read_to_string`, `read_dir`, `symlink_target`), `RealSys`, `#[cfg(test)] FixtureSys` for fixture-driven tests. |
| `src/discovery.rs` (new) | S3 | `GpuVendor`, `DiscoveredGpu`, `discover_gpus<S: Sys>`, hand-rolled relative symlink resolution and BDF parsing (no regex dependency). |
| `src/ui.rs` | S1/S4 | Slimmed of acquisition logic; settings dialog now edits a selector string (any non-control char, ≤40); `settings_config` builds `GpuSelector`. |
| `src/app.rs` | S1/S4 | Imports from `domain`; `gpu_index: Arc<AtomicU64>` → `gpu_selector: Arc<RwLock<GpuSelector>>` shared with workers; sysfs readers take `&dyn Sys`. |
| `src/config.rs` | S2/S4 | `gpu_index: u32` → `gpu_selector: GpuSelector`; new `gpu=` key; legacy `gpu_index=` migrates to `Index` only when no `gpu=` key is present; `save()` writes `gpu=` via `as_string()` (omitted for `Auto`). |
| `src/gpu.rs` | S4 | `GpuMonitor::new(selector)`; `resolve_index()` enumerates NVML UUID/BDF and resolves; `sample()` errors when the selection matches no device; `GpuStats.index` is the resolved NVML index. |
| `src/main.rs` | S4 | `--gpu <BDF|UUID|index>` (env `ORSIKTOP_GPU`) + legacy `--gpu-index` alias (`conflicts_with = "gpu"`, env `ORSIKTOP_GPU_INDEX`). |
| `src/cpu.rs` | S2 | `detect_cpu_topology()` is a facade over `detect_topology<S: Sys>`. |
| `docs/S0_AUDIT.md` | S0 | Baseline audit (94 tests at HEAD). |

## 2. Architecture changes

- **UI/domain separation (S1):** `ui.rs` no longer owns data acquisition; it renders
  normalized snapshots. `domain.rs` holds the shared structures and the refresh
  constants (`MIN_REFRESH_MS=100`, `MAX_REFRESH_MS=10_000`, `REFRESH_STEP_MS=100`,
  `MIN_LLM_POLL_MS=250`). Behavior-preserving.
- **Sys abstraction (S2):** all fixture-testable parsing takes `&dyn Sys` /
  `S: Sys`. `RealSys` is the production impl; `FixtureSys` is test-only. Providers
  keep their own parsing; the abstraction is intentionally minimal.
- **Discovery (S3):** `discover_gpus` walks `/sys/class/drm` card symlinks, resolves
  them to the PCI device, and records vendor/device/class IDs, driver, render node,
  and connected outputs. Display class is `(class >> 16) & 0xff == 0x03`. Identity
  is the PCI BDF (stable, always present for PCI GPUs); UUID is optional (NVML, S5+).
  Verified on the real machine: `Intel card=card0 bdf=0000:00:02.0 driver=xe
  vendor=0x8086 device=0x64a0 class=0x030000 render=Some("renderD128")` with
  connectors eDP-1/DP-1/DP-2/DP-3.
- **Stable selection (S4):** `GpuSelector` (`Auto` | `Uuid` | `PciBusId` | `Index`)
  replaces the bare NVML index everywhere (config, CLI, UI, workers).
  `parse`: blank → `Auto`, numeric → `Index`, `GPU-…` → `Uuid`, else `PciBusId`.
  `resolve(devices: &[(index, uuid, bdf)])` picks the NVML index. Config
  migration: `gpu=` is authoritative; `gpu_index=` only migrates when `gpu=` is
  absent, independent of line order. `Default = Auto`; `Metric<T>`/`DeviceId`
  carry `#[allow(dead_code)]` until S5 consumes them.

## 3. Test results

Final state after S4: **113 passed, 1 ignored** (the ignored test is the
`RealSys`-backed discovery smoke, run separately and verified on real hardware:
`cargo test -- --ignored discovery_real` → 1 passed). All four gates green, zero
warnings. Test growth vs the 94-test baseline: 7 discovery fixture tests,
`Metric`/`DeviceId` tests, the `GpuSelector` parse/resolve/as_string/sanitized
suite, and config migration + round-trip tests.

## 4. Known limitations

- `Metric<T>`/`DeviceId`/`discovery` are plumbed but not yet consumed by the
  sampling path (S5). `mod discovery` carries a temporary `#[allow(dead_code)]`.
- `GpuMonitor::resolve_index` re-enumerates NVML per `sample()`; S5 may cache the
  enumeration keyed by BDF/UUID.
- Discovery covers display-class PCI GPUs. Non-PCI or render-only accelerator
  classes (e.g. some headless render nodes without a DRM card) are not modeled.
- `GpuSelector::parse` cannot disambiguate a raw UUID that does not start with
  `GPU-`; such strings are treated as BDFs and fail resolution cleanly
  (`sample()` errors rather than guessing).
- Config: a malformed `gpu=` value (e.g. text that is neither numeric, `GPU-…`,
  nor BDF-shaped) is stored as `PciBusId` and surfaces as a sampling error, not a
  config parse failure — the UI dialog accepts the same input shape.

## 5. Newly discovered issues

- **`is_display_class` `>> 8` bug (S3):** the first smoke test passed with a
  broken class mask because its asserts only checked BDF non-emptiness — a
  vacuous pass. Strengthened the smoke to assert `pci_vendor_id != 0` (proving
  the read path worked) and fixed the mask to `>> 16`.
- **`resolve()` tuple-binding bug (S4):** the `Uuid` arm bound the third tuple
  element (BDF) instead of the second (UUID); caught by the resolve test
  (`Uuid("GPU-b")` → `None`). Fixed and re-verified.
- **Relative symlink `..` clamping (S3):** resolution of `../../devices/…` from
  `/sys/class/drm` must clamp at `/` like the kernel does; hand-rolled
  `resolve_link` handles this (no regex, no new dependencies).
- **Edit-tool fragility on multi-line Rust constructs** was the dominant
  process cost; every edit is verified by re-reading the touched region and the
  gate suite is the final arbiter.
