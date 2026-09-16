# Stage Summary — S8 (DRM fdinfo process-GPU telemetry)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/drm.rs` (new) | Driver-generic DRM fdinfo parser + per-process sampler. `DrmProcessGpu` (per PCI BDF: `bdf`, `driver`, `client_count`, `resident_bytes`, `total_bytes`, `engines`), `DrmEngine` (per engine: `busy_ns` **or** `cycles_busy`/`cycles_total`, `capacity`, derived `utilization_pct`), `DrmSamplerState` (baseline counters keyed by `(ProcessIdentity, bdf, engine)`, pruned by `retain`). `sample_process_gpus<S: Sys>(sys, now, identity, state)` walks `/proc/<pid>/fd`, filters to symlinks whose target starts with `/dev/dri/`, reads each `/proc/<pid>/fdinfo/<fd>`, and `parse_fdinfo` decodes the canonical `drm-*` key set. Helpers `parse_u64` (first token → `Option<u64>`), `value_to_bytes` (bare / `KiB` / `MiB`, u128 saturating math), `utilization_ns`, `utilization_cycles`. 13 `FixtureSys` tests + 1 ignored live smoke. |
| `src/domain.rs` | `pub use crate::drm::DrmProcessGpu;`. `ProcessStats` gains `pub gpu: Vec<DrmProcessGpu>` (one entry per PCI BDF; empty when the process holds no DRM fd — explicit, never faked). New `ProcessStats::gpu_bytes()` sums `resident_bytes` across devices for the UI. |
| `src/main.rs` | `mod drm;` registered. |
| `src/app.rs` | `use … drm::{sample_process_gpus, DrmSamplerState};`. The fast worker holds a `DrmSamplerState` alongside the process cache; `collect_process_stats` now takes `&mut DrmSamplerState`, samples each live process's GPU usage, stores it on `ProcessStats.gpu`, and prunes both caches + the sampler with the live-identity set each cycle. |
| `src/ui.rs` | Wide process table only: a non-sortable `GPU` column (resident memory, `—` when no DRM fd) is appended after THR. `ProcessGroup` and `ProcessDisplayRow::Group` gain `gpu_bytes`; `grouped_process_rows_filtered` aggregates it; `process_header_wide`, `process_group_line_wide`, `process_line_wide` and the wide branch of `process_header_hits` all gain `+7` to their `fixed` width constant. Compact mode is byte-identical. New `gpu_cell` helper. The one exhaustive `Group` test match updated for the new field. |
| `src/gpu_map.rs` | Module doc: the "fdinfo telemetry is a later stage (S8)" note now points at the implemented `crate::drm`. |
| `CHANGELOG.md` | `[Unreleased]` gained the S8 Added entry. |
| `docs/S8_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **S8 is the *telemetry* half, not the attribution half.** S16's `src/gpu_map.rs::process_render_gpus` already does device *attribution* (open render-node fds → BDFs) for the server→GPU mapping. S8 does not duplicate that: it reads the kernel's own `/proc/<pid>/fdinfo` DRM-client records to get per-process **engine utilization** and **resident GPU memory**, which S16 never computed. The two are complementary; the richer engine-level UX is S23.
- **Driver-generic parser over the canonical `drm-` key set** (`Documentation/gpu/drm-usage-stats.rst`), not per-driver parsers. Engine names (`render`/`copy` vs `rcs`/`bcs` vs `gfx`/`compute`) and memory-region names differ per driver, so engines are stored generically in `Vec<DrmEngine>` and memory is summed to bytes. `drm-memory-*` (documented amdgpu "Legacy … alias to `drm-resident-*`"), `drm-active-*`/`drm-purgeable-*`/`drm-shared-*`, and driver-specific keys (`amd-*`, `pasid`, `i915-*`) are skipped — counting them would double-count.
- **Two utilization schemes, both handled, keyed by what the driver exposes.** i915/amdgpu emit `drm-engine-<name>: <ns>` (cumulative busy nanoseconds) → `% = Δns / Δwalltime`. xe emits `drm-cycles-<name>` + `drm-total-cycles-<name>` → `% = Δbusy / Δtotal` (wall-time independent). The baseline is kept per `(ProcessIdentity, BDF, engine)` in `DrmSamplerState.counters`; the **first sample is `utilization_pct: None`** (no baseline yet — the baseline is still stored). A counter that reads lower than before (kernel reset) saturates the delta to 0%, never negative.
- **`ProcessStats.gpu` is a `Vec`, not an `Option`.** A process can hold render nodes on more than one device, so one entry per PCI BDF is the correct shape; an empty `Vec` is the explicit "no DRM fd" state. The device key is fdinfo's own `drm-pdev` (the authoritative BDF, already in `DeviceId::key()` format) — the S16 fd walk is only reused to discover *which* fdinfos are DRM (symlink target under `/dev/dri/`).
- **Memory is region-agnostic on purpose.** Region names are not stored per-region (summed to `resident_bytes`/`total_bytes`) — YAGNI; per-region detail is an S23/UX concern.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **169 passed, 0 failed, 3 ignored**
(S8 adds 13 `FixtureSys` tests in `drm::tests` plus the ignored
`drm_real_system_smoke`).

- `i915_ns_format_parses_bdf_driver_engines_and_memory` — i915 `ns` counters: BDF, driver, resident sum, `busy_ns` per engine, first-sample `utilization_pct == None`.
- `xe_cycles_format_and_first_sample_utilization_none` — xe `cycles`/`total-cycles`, `65536 KiB` suffix math, first-sample `utilization_pct == None`.
- `amdgpu_skips_deprecated_memory_aliases` — `drm-resident-*` counted; `drm-memory-*`, `amd-*`, `pasid` **not** counted; `total_bytes == 0`; `gfx` engine present.
- `multiple_fds_to_same_bdf_are_aggregated` — two fds to one BDF: `client_count == 2`, resident summed.
- `fdinfo_without_pdev_is_rejected` — no `drm-pdev` → not a DRM fd.
- `utilization_ns_delta_math` / `utilization_cycles_delta_math` — the two `%` formulas in isolation.
- `second_sample_yields_utilization_and_monotonic_reset_is_clamped` — 20% on the second sample; a counter reset (read lower) clamps to 0.0, never negative.
- `retain_drops_exited_process_counters` — pruned identities lose their baselines.
- `no_fdinfo_dir_means_no_gpu`, `non_drm_fd_is_ignored`, `unknown_and_driver_prefixed_keys_are_ignored`, `value_to_bytes_handles_units` — the negative/edge cases.
- `drm_real_system_smoke` (`#[ignore]`) — prints any live process holding DRM fds; passes either way.

Real-machine smoke on **this** box (xe APU `0000:00:02.0`):
`cargo test -- --ignored drm_real` → **1 passed** (the display server's DRM fds are
found and parsed, or none exist — both acceptable). `cargo test -- --ignored`
overall → **3 passed** (`discovery_real`, `intel_real`, `drm_real`). Binary smoke:
`cargo run` reaches only the pre-existing ENXIO terminal-init error (no TTY in this
shell); the sampler + discovery path does not panic.

## 4. Known limitations

- **Per-engine utilization needs two samples.** The first sample of each
  `(process, BDF, engine)` is `None` by construction (no baseline). The UI shows
  resident memory from the first sample; utilization fills in on the second
  process-refresh cycle (≥ the process cadence).
- **Only resident memory is surfaced in the table.** `total_bytes`, per-engine
  counters, and per-region breakdown are collected in the domain but not rendered
  yet — the wide table shows the `GPU` resident column; the richer engine view is
  S23.
- **`drm-resident-*` is the driver's resident figure.** On some drivers this is
  the GPU-side (VRAM) residency; the parser does not split VRAM vs system memory
  (region names are dropped, see §2). That split, if wanted, is an S23 concern.
- **`drm-print` memory units.** The kernel's `drm_print_memory_stats` helper is not
  present in the tree on master; the ABI doc states the default is bytes with an
  optional `KiB`/`MiB` suffix, and `value_to_bytes` handles both, so the parser is
  robust either way.
- **Utilization for a *reset* engine reads 0%.** A counter that drops between
  samples (driver reset) yields a zero delta — honest, but not a true "idle"; it
  cannot be distinguished from real idle with fdinfo alone.

## 5. Newly discovered issues

- **`ParsedFd` originally stored engine names in both the map key and a redundant
  `ParsedEngine.name` field that was never populated** — the map was keyed by name,
  but the struct's `name` stayed `""`, so `merge_engine` (which keyed on
  `engine.name`) collapsed every engine into the single `""` bucket. Every
  multi-engine fixture failed on `engines.len()` / `find(name)`. Fixed by making
  `ParsedFd.engines` a `Vec<(String, ParsedEngine)>` (the key travels with the
  value) and taking the name as an explicit argument to `merge_engine`. This was
  caught by the fixture suite, not the gates.
- **fdinfo's own `drm-pdev` is the authoritative BDF** and is already in the
  `bus:slot.func` format that `DeviceId::key()` / `GpuMapping` use — so S8's
  device key aligns with S16's attribution without any conversion. This is what
  lets S8 and S16 later join on a single BDF string.
- **The `drm-` key set is a stable, driver-generic ABI** documented in
  `Documentation/gpu/drm-usage-stats.rst`; per-driver additions (`amd-*`, `pasid`)
  are additive and ignorable, so a single generic parser is correct rather than a
  per-driver one.
- **The edit tool mangled two regions in this session** (a `PUT` in `fit_cell`
  dropped its tail and left orphan lines; a test-match `PUT` duplicated the assert
  block). Both were caught immediately by re-reading the touched region after each
  edit; final state is verified by the gates, not by trust in the patch.
