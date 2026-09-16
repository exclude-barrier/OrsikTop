# Stage Summary — S6 (AMD GPU/APU backend)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/providers/amd.rs` (new) | `AmdGpuProvider<S: Sys>`: the amdgpu backend. Holds the `DiscoveredGpu` (identity, card name, BDF), the filesystem `S` (production `RealSys`, tests `FixtureSys`), the hwmon name resolved once at construction, and the discovery position (`index`, display only). `GpuProvider::sample` reads `/sys/bus/pci/devices/<BDF>/` (`gpu_busy_percent`, `mem_busy_percent`, `mem_info_vram_total`/`_used`, `product_name`) + the matched hwmon (`temp1_input`, `power1_input`, `freq1_input`, `freq2_input`, `fan1_input`/`fan1_max`) via `fill_hwmon(&mut GpuStats)`. Helpers: `pci_device_dir`, `find_hwmon_for_bdf` (walks `/sys/class/hwmon/*/device`, BDF as a path component), `path_component_is_bdf`, `read_text`/`read_u64`/`read_i64`, `bytes_to_mib`, `sanitize`, `nonnegative`, `clamp_percent`. 6 fixture tests. |
| `src/providers/mod.rs` | `pub mod amd;` registered. |
| `src/gpu.rs` | `new_gpu_provider` is now vendor-neutral: `new_gpu_provider(selector, gpus: &[DiscoveredGpu])`. New local `resolve_discovered(&GpuSelector, &[DiscoveredGpu])` resolves against the BDF-sorted discovery list (position + device): `Auto` prefers the first *NVIDIA* device in BDF order (pre-S6 meaning) and falls back to the first device of any vendor; `Index` positional; `PciBusId` by device key. `Uuid` bypasses discovery and goes to the NVML provider, which resolves it against its own enumeration (discovery keys are BDFs, not vendor UUIDs). Dispatch: NVIDIA → NVML provider, AMD → `AmdGpuProvider::new(gpu, index)`, any other vendor → new `UnavailableGpuProvider` with an explicit "no telemetry backend yet" error. 3 new tests (resolution + error paths). Module doc updated. |
| `src/app.rs` | `static_gpus = discover_gpus(&RealSys)` moved to the top of the fast worker (before the first provider build); both `new_gpu_provider` call sites (initial + selector-change rebuild) pass `&static_gpus`. |
| `src/ui.rs` | GPU panel title de-coupled from NVIDIA: unavailable branch is `" GPU{} · unavailable "` (no error) or `" GPU · <error> "` (with error); footer status `"GPU/NVML · …"` → `"GPU · …"`. The available branch (`" GPU{} · <name> "`) is unchanged. |
| `docs/S6_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Vendor-neutral dispatch from discovery.** The backend is chosen by the
  *discovered device's* vendor, not hardcoded. `new_gpu_provider` resolves the
  user's `GpuSelector` against the BDF-sorted `discover_gpus` list and builds the
  matching backend. A vendor without a backend yet (Intel → S7, `Other`/`Unknown`)
  yields an explicit "no telemetry backend yet for GPU \<BDF\>" error — never a
  fake reading from another vendor's API (no silent NVIDIA fallback).
  Pre-S6 semantics are preserved: `Auto` still means "first NVIDIA device" (BDF
  order, falling back to the first device of any vendor on a machine without
  NVIDIA), and `Uuid` still resolves through NVML — both are tested.
- **AMD reads sysfs directly, keyed by PCI BDF.** amdgpu registers all PM/FRU/VRAM
  attributes on the PCI device (`device_create_file(adev->dev, …)`), so the provider
  reads `/sys/bus/pci/devices/<BDF>/*` plus one hwmon device. No DRM card dir, no
  ioctl, no new dependency.
- **hwmon discovery at construction, once.** `find_hwmon_for_bdf` walks
  `/sys/class/hwmon/*/device` symlinks and matches the BDF as a path component
  (the hwmon is registered on the GPU's PCI device). The hwmon set is stable per
  process, so `sample` reads value files only. No matching hwmon →
  temp/power/clocks/fan degrade to `None` (kernel/driver differences tolerated).
- **Injectable `Sys`, fixture-tested `sample`.** The provider holds a `S: Sys`;
  production is `RealSys`, tests are `FixtureSys`, so `GpuProvider::sample` itself is
  fixture-tested — no separate `sample_against` helper (same pattern as the NVML
  half's fixture coverage, but here the *sample path* is what runs).
- **APU VRAM is the kernel's own number.** amdgpu exposes the same
  `mem_info_vram_*` files on APUs — on a UMA APU they report the system-memory
  carveout the kernel itself exposes. The provider reads them as-is; it does not
  invent a "no dedicated VRAM" state.
- **Fan is a derived percentage.** `fan1_input`/`fan1_max` (both RPM); `None` when
  either is missing or max ≤ 0 — never fake.
- **Unit conversions at the provider edge.** hwmon `temp1_input` (millidegrees → °C),
  `power1_input` (microwatts → W), `freq1/2_input` (Hz → MHz); VRAM bytes → MiB.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **148 passed, 0 failed, 1 ignored** (was
139; +9 = 6 in `providers::amd::tests`, 3 in `gpu::tests`):

- `samples_amd_dgpu_full_sensor_set` — full dGPU set: 42 % / 17 % utilization, 16 GiB
  VRAM (4 GiB used), 55.2 °C, 180 W, 2100 MHz sclk, 1000 MHz mclk, 50 % fan; name
  falls back to the card name without `product_name`.
- `degrades_gracefully_when_hwmon_is_absent` — no hwmon → available, core metrics
  present, all hwmon-only metrics explicitly `None`.
- `product_name_overrides_card_name_and_missing_vram_is_zero` — `product_name` wins;
  missing VRAM files → 0.0.
- `device_not_readable_reports_unavailable` — empty filesystem → `available: false`,
  error contains "not readable".
- `hwmon_is_matched_by_bdf_component` — two hwmons, each matched by its BDF;
  unmatched BDF → `None`.
- `converts_bytes_to_mib` — byte→MiB math.
- `resolves_selector_against_discovered_gpus` (gpu.rs) — `Auto` → first NVIDIA in
  BDF order (not the first device), `Index` positional with out-of-range → `None`,
  `PciBusId` by key with unknown BDF → `None`, `Uuid` → `None` (not resolvable
  against discovery), empty list → `None`.
- `auto_falls_back_to_first_device_without_nvidia` — no NVIDIA present → `Auto`
  yields the first device (Intel) of any vendor.
- `dispatch_reports_explicit_errors_for_unbacked_vendors` (gpu.rs) — empty
  discovery → "no GPU matches the selection"; Intel device → "no telemetry backend
  yet for GPU 0000:00:02.0 (Intel)"; AMD device → the AMD provider is built (not
  the error provider) and reports its own device-specific error.

`cargo test -- --ignored discovery_real` still passes (real-machine Intel `xe`
discovery). Binary smoke: `cargo run` reaches only the pre-existing ENXIO
terminal-init error (no TTY in this shell) — on this machine the worker builds the
`UnavailableGpuProvider` (Intel has no backend yet) and samples it cleanly; the
dispatch + `discover_gpus` path does not panic.

## 4. Known limitations

- **No AMD hardware here → fixture-only.** The development host has no AMD GPU
  (Intel `xe` `0000:00:02.0`), exactly like the NVML half since S5/S16. The
  provider is verified by fixtures built from the kernel source surface
  (amdgpu `sysfs` attributes + the `amdgpu` hwmon, `drivers/gpu/drm/amd/`);
  a real AMD machine closes the gap. All reads are `Option`-wrapped, so this
  path cannot panic or take down the TUI.
- **hwmon-only metrics are `None` when no hwmon matches.** Some kernel/driver
  revisions may register the hwmon under a parent whose `device` symlink does not
  contain the BDF as a path component; those metrics degrade to `None` (explicit),
  core metrics still sample.
- **`fan_percent` only when `fan1_input` and `fan1_max` are both present** and max
  > 0 (many APUs have no fan at all → `None`, correct).
- **Not yet surfaced:** `unique_id`, `board_info`, `pcie_bw` (UNSUPPORTED on APUs
  per the kernel), SmartShift counters. amdgpu sysfs does not expose PCIe link
  speed/width, so `pcie_*` stay `None` for AMD. `power_limit_w`, pstate,
  throttle reasons and encoder/decoder utilization have no amdgpu sysfs source
  and stay `None`/empty (explicit).
- **`GpuSelector::Uuid` is NVIDIA-only by design.** Discovery keys are PCI BDFs
  (it does not collect vendor UUIDs), so a UUID selector is routed straight to the
  NVML provider, which resolves it against its own enumeration — the pre-S6
  behavior is preserved. On a non-NVIDIA machine a UUID selection yields the NVML
  provider's existing "no NVIDIA GPU matches the selection" error. Widening
  discovery identity to vendor UUIDs (for AMD/Intel) belongs to S23's
  heterogeneous UX pass.
- **`GpuStats.index` for AMD is the discovery-list position** (BDF-sorted), not a
  vendor enumeration index — display-only.

## 5. Newly discovered issues

- **`FixtureSys::read_dir` returns `None` for unregistered directories.** A
  directory that exists in the fixture only as a key of the *files* map is not
  listable — the fixture requires an explicit `dir_entry` for every directory a
  provider `read_dir`s. The AMD provider's "device not readable" gate
  (`read_dir(&pci).is_none()`) therefore depends on the fixture registering the PCI
  device dir; `add_pci` does this now.
- **Clippy `type_complexity` on a 5-`Option` return tuple.** Returning
  `(Option<f64>; 5)` from `read_hwmon` fails `-D warnings`; the fix was to invert
  the control flow: `fill_hwmon(&mut GpuStats)` writes the fields in place, which
  also drops the tuple destructuring in `sample`.
- **`GpuProvider: Send` constrains the provider's type parameter.** `AmdGpuProvider`
  is generic over `S: Sys`, but `Box<dyn GpuProvider>` requires `Send`, so the impl
  bound is `S: Sys + Send` (`RealSys` and `FixtureSys` both qualify).
- **Naive "first device wins" `Auto` silently changes behavior on mixed-vendor
  machines.** The first draft of the dispatch made `Auto` mean "first discovered
  device (BDF order)", which on a machine with an AMD card at a lower BDF than the
  NVIDIA one would switch the sampled GPU without any config change — and a first
  draft that resolved `Uuid` against discovery broke NVIDIA UUID configs entirely.
  Both are pinned by tests (`auto_falls_back_to_first_device_without_nvidia`,
  `resolves_selector_against_discovered_gpus`): `Auto` keeps its documented
  "first NVIDIA GPU" meaning with a non-NVIDIA fallback, and `Uuid` keeps going
  through NVML.
- **The edit tool mangled two multi-hunk patches in this session** (a duplicated
  `let` in `app.rs`, a duplicated `add_pci` header). Both were caught immediately
  by re-reading the touched region after each edit and fixed; final state is
  verified by the gates, not by trust in the patch.
