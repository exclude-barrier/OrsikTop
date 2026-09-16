# Stage Summary — S12 (Adaptive sampling: metric-class cadence separation)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/app.rs` | New `SLOW_SENSOR_REFRESH_INTERVAL` (1000 ms) constant next to `SYSTEM_REFRESH_INTERVAL` (250 ms). The fast worker gains three pieces of worker-local state: `last_sensor_refresh: Option<Instant>` plus cached `cpu_temperature_c` / `cpu_power_w: Option<f64>`. Before the 250 ms system-refresh block, a new gate re-samples CPU temperature and RAPL power **only** when 1 s has elapsed, storing the results in the cache; the `SystemStats` construction now takes the **cached** values instead of sampling inline. Frequency stays on the 250 ms system cadence. |
| `CHANGELOG.md` | `[Unreleased]` gained the S12 Changed entry. |
| `docs/S12_STAGE_SUMMARY.md` | This summary. |

No domain-model, provider, UI, or config changes were needed: the metric
classes already lived behind the normalized `SystemStats` snapshot, so the
change is purely a worker-scheduling refinement (S12's goal: "the user's UI
refresh interval must not force every expensive telemetry source to run at that
same frequency").

## 2. Architecture changes

- **Metric classes, by cadence:**
  - *Fast (per UI cycle, floor 100 ms):* GPU provider sample (NVML/i915/xe
    sysfs reads — already cached internally per backend), snapshot publish.
  - *System (250 ms):* CPU usage, per-core usage, IOW, load average, memory,
    and **CPU frequency** (cpufreq `scaling_cur_freq` — a handful of small
    file reads, cheap enough for the fast cadence).
  - *Process (user-configurable, default 1 s):* `/proc` process table, DRM
    fdinfo sampler, server→GPU mapping.
  - ***Slow (1 s, new):*** **CPU temperature** (hwmon) and **RAPL package
    power** — the power sample blocks ~50 ms for its bounded two-read energy
    window, so it must not ride the 250 ms system cadence (that was ~20 % of
    the cycle blocked, every cycle).
  - *Static (once):* GPU discovery, CPU topology, sensor layout
    (`CpuSensors::discover`), NVML handle.
  - *LLM worker (250 ms floor, pre-existing):* `/metrics` HTTP polling.
- **Cached-reuse, latest-wins.** The slow-sensor cache is plain worker-local
  state: the slow block writes, the fast block reads. There is no locking —
  both run on the same worker thread. Before the first slow sample lands the
  cache is `None`, and the first iteration runs the slow gate first
  (`unwrap_or(true)`), so the first published snapshot already carries real
  values (no `None`→value flicker).
- **UI refresh is decoupled from every source except the GPU sample.** A fast
  UI refresh (100 ms) still only ever costs the GPU sample + one snapshot
  clone; temperature/power lag by at most 1 s, which is invisible for
  slow-changing thermal metrics and matches the S12 principle of reusing
  cached values safely.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **182 passed, 0 failed, 3 ignored**.
`cargo test -- --ignored` → **3 passed** (`discovery_real`, `intel_real`,
`drm_real`). No new fixture tests: the cadence split is worker-thread
scheduling (time-gated), not parseable logic — the existing `next_cycle_target`
tests already pin the clamp behavior they share. The sensor *sampling* logic
itself was unchanged and stays covered by the 12 `cpu_sensors::tests`.

Real-machine smoke: `cargo run` reaches the pre-existing ENXIO terminal-init
error (no TTY in this shell) — the worker path starts without regression. The
startup smoke cannot observe the 1 s cadence split directly in a non-TTY
shell; the behavioral guarantee is that temperature/power now update at most
once per second instead of every 250 ms.

## 4. Known limitations

- **The slow cadence is a fixed 1 s constant, not user-configurable.** Adding
  it to the config would be YAGNI: temperature/power change over seconds, 1 s
  is plenty, and the existing two knobs (`refresh_ms`, `process_refresh_ms`)
  already cover the user-facing trade-offs.
- **Stale-by-design thermal values.** The UI can show a temperature/power up
  to 1 s old (plus the ~50 ms RAPL window). That is inherent to the requested
  decoupling and invisible in a 100 ms–1 s UI.
- **GPU provider sampling still runs every UI cycle.** NVML and the sysfs
  backends are fast and internally cache their discovery; splitting the GPU
  sample onto its own cadence was considered and rejected — utilization is a
  *fast* metric by definition (S12's own example) and NVML calls are
  sub-millisecond.
- **Frequency stays fast even though it changes slowly.** Deliberate: its
  source is 8 small cpufreq file reads (no blocking window); moving it to the
  slow cadence would gain nothing and make the panel's frequency visibly
  coarser than the load/usage figures beside it.

## 5. Newly discovered issues

- **The RAPL two-read window was the real hot-path cost, not the file reads.**
  The S11 design bounded the `energy_uj` window to 50 ms, but on the 250 ms
  system cadence that meant the worker slept 50 ms out of every 250 ms cycle
  (~20 %) just to compute a number that changes over seconds. S12's cadence
  split removes that entirely from the fast path: power now costs one 50 ms
  block per second instead of one per 250 ms.
- **No `None`-flicker guard was needed for the first snapshot** because the
  slow gate (`unwrap_or(true)`) runs *before* the first system-refresh block in
  the same loop iteration — the cache is populated before the first
  `SystemStats` is built. Had the slow gate run after, the first published
  frame would have shown `—` for temp/power and then values, a visible blip.
