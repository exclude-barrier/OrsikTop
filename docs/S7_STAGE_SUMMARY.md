# Stage Summary — S7 (Intel GPU/APU backend)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/providers/intel.rs` (new) | `IntelGpuProvider<S: Sys>`: the i915/xe backend. Holds the `DiscoveredGpu` (identity, card name, BDF, driver string), the filesystem `S` (production `RealSys`, tests `FixtureSys`), the hwmon name resolved once at construction, and the discovery position (`index`, display only). `GpuProvider::sample` reads the driver's GT sysfs (xe: `<pci>/tile0/gt0/freq0/cur_freq` + `freq0/throttle/reasons`; i915: `<card>/rps_cur_freq_mhz` + `throttle_reason_*` bools, with a `<card>/gt/gt0/` fallback) plus the matched hwmon (`temp1_input`, `power1_max`) via `fill_hwmon(&mut GpuStats)`. Helpers: `pci_device_dir`, `find_hwmon_for_bdf` (walks `/sys/class/hwmon/*/device`, BDF as a path component — same shape as the AMD provider), `path_component_is_bdf`, `gt_sysfs_dir`, `read_throttle_reason`, `read_text`/`read_u64`/`read_i64`, `sanitize`, `nonnegative`. 8 fixture tests + 1 ignored real-machine smoke. |
| `src/providers/amd.rs` | `sample()` fills the now-`Option` fields with `Some(…)` (no `unwrap_or(0.0)`); `sanitize` clamps via `.map(clamp_percent)`. Tests assert `Some(…)`; missing-VRAM test renamed to `…_is_none` and asserts `memory_total_mib: None`. |
| `src/providers/nvidia.rs` | `sample()` drops the now-redundant `.unwrap_or(0.0)` (the `.map` already yields an `Option`); `sanitize` uses `.map(clamp_percent)`; tests assert `Some(…)`. |
| `src/gpu.rs` | `new_gpu_provider` gains the `GpuVendor::Intel => Box::new(IntelGpuProvider::new(gpu.clone(), index))` dispatch arm (before the `_ → UnavailableGpuProvider` fallback). Module doc updated (Intel → i915/xe sysfs + hwmon). `dispatch_reports_explicit_errors_for_unbacked_vendors` re-pointed at a `GpuVendor::Other` device for the "no backend" case (Intel is now backed) and gained an Intel-backed assertion. |
| `src/ui.rs` | `draw_gpu`/`push_sample` are `Option`-aware: `vram_pct`/`used_text`/`vram_tint` handle `None`; the VRAM/usage meter rows render `unavailable_meter_line(…)` (a new `meter_line`-shaped placeholder, pre-suffix width `6 + width + 9` cells, so trailing data pairs stay column-aligned) when the value is `None`; MEMCTRL uses `optional_number(…)`. 2 test sites updated to `optional_number`. |
| `CHANGELOG.md` | `[Unreleased]` gained the AMD (S6) and Intel (S7) backend entries plus the `Option`-fields change. |
| `docs/S7_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Vendor-neutral dispatch is now three-way.** `new_gpu_provider` resolves the
  `GpuSelector` against the BDF-sorted discovery list (S6) and dispatches
  NVIDIA → NVML, AMD → amdgpu sysfs/hwmon, **Intel → i915/xe sysfs + hwmon**.
  The `_` fallback (explicit "no telemetry backend yet") now only catches
  `Other`/`Unknown`. Pre-S6 `Auto` (first NVIDIA, else first device) and
  `Uuid` (straight to NVML) semantics are unchanged and still tested.
- **`GpuStats` utilization/memory fields are now explicit `Option<f64>`.** This
  is the load-bearing decision of the stage. Neither Intel driver exposes a GPU
  busy counter, a memory-bandwidth utilization, VRAM total/used, an
  *instantaneous* power draw, or a fan-speed maximum through a value sysfs file
  (unlike amdgpu's `gpu_busy_percent` / `mem_info_vram_*`). Keeping those four
  fields as `f64` would have forced a fake `0.0` for Intel — directly violating
  the cardinal rule *missing metric = explicit state, never a fake zero*. They
  are now `Option<f64>` (`Default = None`), so Intel reports `None` honestly and
  NVIDIA/AMD keep filling `Some(…)`. Every consumer (amd.rs, nvidia.rs, ui.rs)
  was updated in one pass. The UI renders a `None` as `—` via
  `unavailable_meter_line`, keeping the row's fixed-width data pairs aligned.
- **Intel reads sysfs value files only** — no ioctls, no new dependencies.
  `IntelGpuProvider<RealSys>::new` resolves the hwmon once (BDF-component match
  against `/sys/class/hwmon/*/device`, the same helper shape as the AMD
  provider), so `sample` reads value files only. `impl<S: Sys + Send>
  GpuProvider` (the `+ Send` bound is what `Box<dyn GpuProvider>` requires).
- **Metric mapping (all `Option`-wrapped, `None` when the source is absent):**
  `graphics_clock_mhz` = xe `tile0/gt0/freq0/cur_freq` (MHz) **or** i915
  `rps_cur_freq_mhz` (MHz, `<card>/` then `<card>/gt/gt0/`); `limit_reason` =
  xe `freq0/throttle/reasons` passthrough (`""`/missing → `"none"`) **or** i915
  assembled from the `throttle_reason_*` bools (`pl1/pl2/pl4`→`power`,
  `thermal`→`thermal`, `prochot`, `ratl`, `vr_thermalert`→`vr-thermalert`,
  `vr_tdc`→`vr-tdc`, joined with `+`, empty → `"none"`); `temperature_c` =
  hwmon `temp1_input`/1000 (millidegrees → °C); `power_limit_w` = hwmon
  `power1_max`/1e6 (µW → W), **only when > 0** (PL1-disabled drivers emit `0`,
  which is not a real limit); `fan_percent` = `None` (no `fan1_max` in either
  hwmon); `power_w` = `None` (no `power1_input`); `memory_clock_mhz`,
  encoder/decoder utilization, all `pcie_*` = `None`; the four utilization/mem
  fields = `None`. `name` = `gpu.card` (neither driver exposes a product name
  in sysfs). `available: true` when the driver's GT sysfs base dir is readable.
- **hwmon is dGfx-only, so it is absent on an APU.** Both `xe_hwmon.c` and
  `i915_hwmon.c` register their hwmon only for discrete devices; on an APU there
  is no `xe`/`i915` hwmon device, so `find_hwmon_for_bdf` returns `None` and the
  temperature + power-limit degrade to `None` while the clock and throttle
  reason remain available from the GT sysfs. This is exactly the shape verified
  on this machine (see §3).

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **156 passed, 0 failed, 2 ignored**
(was 148 passed / 1 ignored at S6; +8 = the 8 fixture tests in
`providers::intel::tests`, and the ignored count 1 → 2 = the new
`intel_real_system_smoke`):

- `samples_xe_dgpu_with_hwmon` — full xe dGPU: 1250 MHz, reason `none`, 48.0 °C,
  280 W PL1 limit; all utilization/VRAM/`power_w`/`fan_percent` explicitly `None`.
- `xe_apu_without_hwmon_keeps_clock_and_reason` — the APU shape (GT sysfs
  present, no hwmon): clock `Some(800.0)`, reason `none`, temp/limit `None`.
- `xe_reports_throttle_reason_when_throttled` — reason passthrough `thermal
  prochot`.
- `samples_i915_from_card_directory` — i915 clock from `<card>/rps_cur_freq_mhz`,
  reason `none`.
- `i915_reports_throttle_reason_from_bool_files` — `throttle_reason_thermal=1`
  → reason `thermal`.
- `device_not_readable_reports_unavailable` — empty filesystem → `available:
  false`, error contains "not readable".
- `hwmon_is_matched_by_bdf_component` — two hwmons each matched by BDF;
  unmatched BDF → `None`.
- `pl1_limit_of_zero_is_treated_as_unavailable` — `power1_max=0` (PL1 disabled)
  → `power_limit_w: None` (not `0.0`).
- `dispatch_reports_explicit_errors_for_unbacked_vendors` (gpu.rs) — now asserts
  `Other` → "no telemetry backend yet", while AMD and Intel each get their own
  provider (device-specific error, not the fallback).
- `intel_real_system_smoke` (`#[ignore]`) — real-machine smoke, loops only Intel
  devices (robust on non-Intel machines).

Real-machine smoke on **this** box (xe APU `0000:00:02.0`):
`cargo test -- --ignored intel_real` → **1 passed**, printing
`Intel card=card0 bdf=0000:00:02.0 driver=xe available=true clock=Some(800.0)
reason=none temp=None limit=None`. The `clock=Some(800.0)` matches the live
`/sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/cur_freq` (800) read
independently from sysfs; `temp=None`/`limit=None` are correct because this APU
registers no `xe` hwmon (confirmed: `/sys/class/hwmon/` holds only `acpitz`,
`coretemp`, `nvme`, `BAT0`, `acpi_fan`, etc. — no `xe`/`i915`).
`cargo test -- --ignored discovery_real` still passes (1 passed). Binary smoke:
`cargo run` reaches only the pre-existing ENXIO terminal-init error (no TTY in
this shell); the dispatch + `discover_gpus` path does not panic.

## 4. Known limitations

- **i915 branch is fixture-only here.** The development host is a xe APU, so the
  i915 code path (`rps_cur_freq_mhz`, `throttle_reason_*`) is verified by
  fixtures built from the i915 kernel source, not against a live i915 device.
  A real i915 machine closes that gap. The xe branch *is* verified live (§3).
- **No utilization / VRAM / instantaneous power / fan on Intel.** Neither driver
  exposes these through value sysfs, so `GpuStats.{utilization,
  memory_utilization, memory_used_mib, memory_total_mib}`, `power_w` and
  `fan_percent` are always `None` for Intel (explicit "unavailable", rendered
  `—` in the UI). This is an honest absence, not a missing feature to backfill —
  the kernel provides no sysfs source for them (see §5).
- **hwmon-only metrics (temp, power limit) are `None` on APUs and on any kernel
  that registers the hwmon under a parent whose `device` symlink does not carry
  the BDF as a path component.** Core metrics (clock, throttle reason) still
  sample in both cases.
- **`power_limit_w` is `None` when PL1 is disabled** (`power1_max = 0`), since 0
  is not a real limit.
- **`name` is the DRM card name** (`card0`), not a product name — neither
  i915/xe exposes a product string in sysfs (unlike amdgpu `product_name`).
- **`GpuStats.index` for Intel is the discovery-list position** (BDF-sorted),
  display-only, same as AMD.
- **`GpuSelector::Uuid` remains NVIDIA-only by design** (discovery keys are PCI
  BDFs, not vendor UUIDs); a UUID selection on a non-NVIDIA machine yields the
  NVML provider's existing "no NVIDIA GPU matches the selection" error. Widening
  identity to vendor UUIDs belongs to S23.

## 5. Newly discovered issues

- **Neither Intel driver exposes a GPU busy counter, VRAM total/used,
  instantaneous power, or a fan-speed maximum through value sysfs.** This was
  confirmed against the kernel sources (torvalds/linux master) and is the reason
  the `GpuStats` utilization/memory fields had to become `Option`:
  - `drivers/gpu/drm/xe/xe_hwmon.c` — the hwmon channel table gives power
    `HWMON_P_MAX | HWMON_P_RATED_MAX | HWMON_P_LABEL | HWMON_P_CRIT | HWMON_P_CAP`
    (**no `power1_input`**), and fan `HWMON_F_INPUT` only (**no `fan1_max`**).
    Registered only for dGfx; `xe_hwmon_read_label` maps channel 0 to the
    `"pkg"` temp label (`temp1_input`).
  - `drivers/gpu/drm/i915/i915_hwmon.c` — `i915_hwmon_register` is guarded by
    `if (!IS_DGFX(i915)) return;` (APU ⇒ no hwmon); power is
    `HWMON_P_MAX | HWMON_P_RATED_MAX | HWMON_P_CRIT` (no instantaneous input);
    fan is input-only (no max).
  - `drivers/gpu/drm/i915/gt/intel_gt_sysfs_pm.c` — RPS attributes are in MHz
    (`rps_cur_freq_mhz`, …) plus the `throttle_reason_*` bool files
    (`INTEL_GT_RPS_BOOL_ATTR_RO`); no utilization, no VRAM, no power draw.
  - `drivers/gpu/drm/xe/xe_gt_freq.c` / `xe_gt_throttle.c` — xe freq0 attributes
    are in MHz and `freq0/throttle/reasons` reports `none` when idle.
- **i915 root-GT attrs live under the DRM *card* kobject, not the PCI kobject.**
  `gt_get_parent_obj` returns `i915->drm.primary->kdev->kobj`, and
  `drm_sysfs_minor_alloc` (`drivers/gpu/drm/drm_sysfs.c`) allocates the primary
  minor's `kdev` as a fresh `card%d` device in the `drm` class (`kdev->parent`
  is the physical device). So `intel_gt_sysfs_pm_init(gt, gt_get_parent_obj(gt))`
  for the root GT creates `rps_*`/`throttle_reason_*` directly under
  `/sys/class/drm/card0/`, and the per-GT `gt%d` kobjects hang under
  `i915->sysfs_gt = kobject_create_and_add("gt", &card0->kobj)`
  (`drivers/gpu/drm/i915/i915_sysfs.c`). The provider reads `<card>/` first with
  a `<card>/gt/gt0/` fallback, and tracks *which* directory actually supplied the
  clock so the throttle bools are read from the same place.
- **`GpuStats` field-type change is cross-file and breaking.** Changing four
  `f64` fields to `Option<f64>` touched the AMD and NVIDIA providers and the UI
  in one pass; `Option<f64>: Default = None` kept the `..Default::default()`
  sites compiling. A missed consumer would have been caught by `cargo check`
  (run clean after the full refactor, before `intel.rs` was added).
- **The edit tool mangled a line once in this session** (a `PUT 1:` replaced the
  `intel.rs` doc header with an assert, and a multi-hunk `gpu.rs` patch echoed a
  trailing line). Both were caught immediately by re-reading the touched region
  after each edit and fixed; final state is verified by the gates, not by trust
  in the patch.
