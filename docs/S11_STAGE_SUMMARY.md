# Stage Summary — S11 (CPU frequency / thermal / power telemetry)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/cpu_sensors.rs` (new) | Capability-based CPU sensor telemetry. `CpuSensors { layout: Option<SensorLayout> }`; `SensorLayout { freq_policies, temp_sensors, power_zones }`. `discover<S: Sys>` runs the directory walks **once**; `sample_frequency_mhz`, `sample_temperature_c`, `sample_power_w` re-read only the dynamic counter values. Discovery: cpufreq `policy*` dirs (weight = `affected_cpus` CPU count via `crate::cpu::parse_cpu_list`), CPU hwmon `temp*_input` sensors (package/Tctl labels preferred), and the RAPL `intel-rapl` **package** zone (skips `intel-rapl-mmio*` mirror, sub-zones, and zones with no `udelay`/`max_energy_range_uj`). `is_dir_entry` treats a real dir **or a symlink** as walkable — `/sys/class/hwmon/*` and `/sys/class/powercap/*` are symlinks on real systems. `rapl_watts` computes `delta_uj / window_us` (µJ/µs = W) and clamps a counter reset to 0 W. |
| `src/cpu.rs` | `parse_cpu_list` made `pub(crate)` (was private) so the sensor module can parse `affected_cpus`. |
| `src/domain.rs` | `SystemStats` gains `pub cpu_power_w: Option<f64>` after `cpu_temperature_c` — `None` when no RAPL package zone is readable (root-only `energy_uj`), never a fake zero. |
| `src/app.rs` | Imports `CpuSensors`. The fast worker holds a `CpuSensors`, runs `discover(&RealSys)` once before the loop; in the 250 ms system-refresh block it samples `cpu_frequency_mhz` (cpufreq, falling back to `/proc/cpuinfo` `cpu MHz` via the retained `read_cpu_frequency_mhz`), `cpu_temperature_c`, and `cpu_power_w`. The old `read_cpu_temperature_c` fn and the `hwmon_fixture_tests` submodule are removed — the hwmon logic and its fixtures migrated into `cpu_sensors.rs` (plus the improved impossible-value test that now asserts the core fallback `55.0` rather than `None`). |
| `src/ui.rs` | `draw_system`'s full branch gains a `POWER` line (`{:.1} W`, `—` when `None`) below `LOAD`; `system_panel_height` bumped by 1 (`+10`/`13`). The narrow branch is unchanged. |
| `src/main.rs` | `mod cpu_sensors;` registered. |
| `CHANGELOG.md` | `[Unreleased]` gained the S11 Added entry. |
| `docs/S11_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Discovery is cached; sampling is cheap.** `discover` walks `/sys/devices/system/cpu/cpufreq`, `/sys/class/hwmon`, and `/sys/class/powercap` once at worker start. Each sample re-reads only the counter files that changed. This mirrors the S8/S16 pattern (static topology cached, dynamic values re-sampled) and keeps the 250 ms system refresh from re-walking the tree.
- **Frequency is cpufreq-first, `/proc/cpuinfo`-second.** The cpufreq `scaling_cur_freq` (kHz) per policy is averaged weighted by each policy's `affected_cpus` count — correct on both per-CPU-policy boxes (like this 288V, 8 singleton policies) and shared-policy boxes. When no policy reports a current frequency, the retained `/proc/cpuinfo` `cpu MHz` average is the fallback. A policy reading `0` (idle/parked) is skipped, never averaged in as zero.
- **Temperature prefers package/Tctl over per-core.** CPU hwmon drivers (`coretemp`, `k10temp`, `zenpower`, `x86_pkg`, and generic `cpu` names that are not `gpu`) are walked; `temp*_input` millidegrees are range-filtered to a physically plausible `−20..=150 °C`, and the maximum of the *preferred* (package/Tctl-labeled) sensors wins, with per-core sensors as the fallback. Out-of-range readings are dropped, not clamped — an impossible value means "ignore this sensor."
- **Power is an explicit `Option`, honest about root.** The RAPL package zone's `energy_uj` is read twice a short bounded window apart. On current kernels `energy_uj` is `root:root 0400` — a non-root OrsikTop gets `Permission denied`, so `sample_power_w` returns `None` and the UI shows `—`. The RAPL path is fully implemented and fixture-tested; it simply reports unavailable here, which is the correct honest state rather than a faked `0.0 W`.
- **The `udelay` vs `max_energy_range_uj` split.** Older kernels expose a `udelay` (µs) no-wrap window; newer ones (this 7.2 box) expose only `max_energy_range_uj`. Discovery accepts **either** as evidence of a real, bounded zone. The measurement window is `min(udelay/2, 50 ms)` when `udelay` exists, else a fixed `50 ms` — always far below any wrap window.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **182 passed, 0 failed, 3 ignored**
(S11 adds 12 `cpu_sensors::tests` and removes 2 migrated `hwmon_fixture_tests`;
net +2 over the pre-S11 172). `cargo test -- --ignored` → **3 passed**
(`discovery_real`, `intel_real`, `drm_real`).

- `frequency_is_weighted_average_over_policies` — two policies (4+2 CPUs) → `(4×2000+2×1000)/6 = 1666.67 MHz`.
- `frequency_none_when_policies_report_zero` — all-zero `scaling_cur_freq` → `None` (falls back to cpuinfo).
- `frequency_none_without_cpufreq_dir` — absent cpufreq tree → `None`.
- `temperature_prefers_package_sensor_and_skips_non_cpu` — coretemp package `63.0` wins; nvme hwmon ignored.
- `temperature_ignores_impossible_values_and_falls_back_to_core` — out-of-range package reading dropped; per-core `55.0` returned (migrated + improved from the old `None`-only test).
- `temperature_none_without_cpu_hwmon` — only a non-CPU hwmon present → `None`.
- `rapl_watts_delta_math_and_reset_clamp` — `250000 µJ / 50000 µs = 5 W`; a counter reset clamps to `0.0`; zero window guards divide-by-zero.
- `power_discovers_package_zone_and_ignores_subzones_and_mmio` — only the non-MMIO `intel-rapl:0` package zone is kept (core sub-zone + MMIO mirror dropped).
- `power_none_without_rapl` — absent powercap tree → `None`.
- `power_none_when_energy_counter_unreadable` — zone discovered but `energy_uj` absent (the root-only case) → `None`.
- `hwmon_and_powercap_entries_are_symlinks_and_still_discovered` — symlinked class entries are followed (the real-system shape).
- `cpu_list_parser_rejects_empty_input` — the newly-`pub(crate)` `parse_cpu_list`.

Real-machine smoke on **this** box (Intel Core Ultra 9 288V, 8 logical CPUs):
a temporary `#[ignore]`d live test ran `discover` + all three samples on `RealSys`
and printed `freq=Some(1505.9)` (≈1.5 GHz, cpufreq), `temp=Some(46.0)` (°C,
coretemp), `power=None` (root-only `energy_uj`), `policies=8 temps=9 zones=1` —
exactly the expected shape. The temporary test was removed afterwards.

## 4. Known limitations

- **CPU power is `None` on this box (and most non-root deployments).**
  `energy_uj` is `root:root 0400` on current kernels, so a non-root OrsikTop
  cannot read the RAPL energy counter. The UI shows `—`. The full RAPL sampling
  path is implemented and fixture-tested; it would report real watts under root
  or on a kernel exposing a world-readable energy counter.
- **Frequency is a *weighted* average, not per-core.** A mixed P/E box shows one
  aggregate MHz figure (the weighted mean across policies). Per-core frequency
  is not surfaced (YAGNI; the S12 adaptive-sampling stage may revisit).
- **Temperature is the package (or hottest core) reading, not per-core.** The
  UI shows a single `°C` figure — the preferred package/Tctl value, or the
  hottest core when no package sensor exists. Per-core temperatures are not
  rendered.
- **`udelay`-less boxes use a fixed 50 ms window.** Where `max_energy_range_uj`
  is present but `udelay` is not, the window is a fixed 50 ms (well under any
  realistic wrap window). This is fine for power estimation but is not the
  kernel's own no-wrap bound.

## 5. Newly discovered issues

- **`/sys/class/hwmon/*` and `/sys/class/powercap/*` are symlinks, not dirs.**
  The first implementation filtered on `is_dir`, which dropped *every* entry
  (these class entries are symlinks to the device dirs) — the live smoke showed
  `temps=0 zones=0`. Fixed with `is_dir_entry` (dir **or** symlink) in all three
  walkers. This was caught by the live smoke test, not the fixture gates (the
  fixtures used real dirs), and a regression test was added to pin the symlink
  behavior.
- **`energy_uj` is root-only on this kernel (7.2.3-arch1-3).** `stat` shows
  `-r-------- root root` on `/sys/class/powercap/intel-rapl:0/energy_uj`, so
  non-root reads fail with `Permission denied`. This confirms power must be an
  honest `None` here — the design decision (no fake zero) was validated against
  the real box.
- **This box uses `max_energy_range_uj`, not `udelay`.**
  `intel-rapl:0` exposes `max_energy_range_uj` (262143328850) with **no**
  `udelay` file. The discovery logic had to accept either as a bounded zone,
  and the measurement window falls back to the fixed 50 ms constant.
- **cpufreq is per-CPU here (8 singleton policies), not shared.** Each policy's
  `affected_cpus` is a single CPU. The weighted-average design handles both this
  shape and shared-policy boxes (where one policy's `affected_cpus` spans many
  CPUs), so no special-casing was needed.
