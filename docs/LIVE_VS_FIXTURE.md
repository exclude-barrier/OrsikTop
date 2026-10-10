# Live vs. fixture verification matrix

Scope: for every functional area of OrsikTop, state honestly which behaviour
has been **verified live on real hardware / a real backend** and which is only
covered by **fixtures and unit tests** (no live evidence recorded). A third
state, **unknown**, marks anything for which neither live nor fixture evidence
exists.

This file is the compact, explicit matrix the development roadmap asked for on
top of `docs/HARDWARE_SUPPORT.md`. It does not replace it: `docs/HARDWARE_SUPPORT.md`
holds per-capability hardware detail; this file is the per-functional-area
live/fixture ledger. The source code and its tests are the source of truth.

This document is read-only evidence. It changes no production logic.

## Evidence levels

Three levels; a capability is classified by the strongest evidence it has:

- **Live** — executed against real hardware or a real inference backend; the
  box / backend is named. Must be backed by a recorded run (commit message,
  stage summary, `docs/HARDWARE_SUPPORT.md`). A provider abstraction existing
  is not evidence.
- **Fixture** — exercised only by unit tests (`FixtureSys`, in-process fake
  HTTP servers, or checked-in fixture files). Runs in CI; never observed live.
- **Unknown** — neither live nor fixture evidence: not measured on hardware and
  not covered by any test. Must stay explicitly unknown.

Environments (both non-root, kernel `7.2.5-omarchy`; see
`docs/HARDWARE_SUPPORT.md`):

- **Dev box** — Intel Core Ultra 9 288V (hybrid P+E), Intel Arc iGPU
  (xe driver, PCI `0000:00:02.0`). No NVIDIA/AMD hardware.
- **cf-desktop** — Intel Core i9-12900K (8P+8E), Intel iGPU (i915,
  `0000:00:02.0`), NVIDIA GeForce RTX 4090 (`0000:01:00.0`), a local
  `llama serve` (b10909, `--parallel 2`). Captured 2026-09-18 (S24).
- One live backend run (Strata) names no box (commit text only).

Baseline `main @ c06bded`, 467 tests green on cf-desktop; the one regression
test added by this document brings the suite to 468. Ignored-by-default
live smokes: `discovery_real_system_smoke` (`src/discovery.rs:452`),
`intel_real_system_smoke` (`src/providers/intel.rs:581`),
`drm_real_system_smoke` (`src/drm.rs:904`) — run with `cargo test -- --ignored`.

## Matrix

| Functional area | Live (real hardware / backend) | Fixture only | Unknown (neither) | Key evidence |
| --- | --- | --- | --- | --- |
| LLM telemetry — llama.cpp | Single-slot: a **remote** single-slot server observed from the dev box (`0 busy / 1 total`); multi-slot `--parallel 2`: remote b10909 over Tailscale + local cf-desktop (S24) | Prometheus/slots/props parsing, slot pairing, spec/MTP, counter reset; candidate ordering across **>1** server (tested, never observed live) | — | `tests/fixtures/*.prom`, `slots_*.json`, `props.json`; `src/llama.rs` tests |
| LLM telemetry — Strata backend | `/metrics` against a real Strata build `0.1.40` (box not named) — totals completion-batched, `live.tok_s` windowed | Wire-format dispatch, phase normalization, old/new `/slots` schema (incl. the newer `n_prompt_tokens` shape: captured, not live-re-verified) | — | `src/strata.rs` tests; `tests/fixtures/slots_strata_{old,new}.json` |
| GPU — NVIDIA discrete | RTX 4090 on cf-desktop, under active inference | Parsers, sanitizers, MIG topology; multi-NVIDIA index selection (`index_selects_the_nth_nvidia_on_igpu_dgpu_machines`, `src/gpu.rs:285`); UUID half of identity | — | `src/providers/nvidia.rs` tests |
| GPU — AMD (dGPU and APU) | — | Full amdgpu sysfs + hwmon shapes, carveout, malformed values | — | `src/providers/amd.rs` tests |
| GPU — Intel xe (iGPU/APU) | xe iGPU on dev box (discovery, clock, throttle; temp/power `None`) | xe dGfx-hwmon + APU shapes (`samples_xe_dgpu_with_hwmon`, `xe_apu_without_hwmon_keeps_clock_and_reason`) | — | `intel_real_system_smoke`; `src/providers/intel.rs` tests |
| GPU — Intel i915 / Arc dGPU | — | i915 root-GT and `gt/gt0` fallback, dGfx hwmon (`temp1_input`, PL1) | — | `src/providers/intel.rs::tests` |
| GPU — NVIDIA MIG | — | Topology, stable child keys, process attribution, UI matching | Exact `MIG-…` UUID form a driver emits; `Device::uuid()` on a child handle | `src/providers/nvidia.rs`, `src/gpu_map.rs`, `src/domain.rs`, `src/ui.rs` |
| CPU / sensors | Hybrid P+E topology, cpufreq, hwmon temperature, RAPL `None` under non-root (dev box) | LP-E class; conventional (all-P) Intel; **AMD CPU thermal sensors** (`k10temp`/`Tctl`); capacity fallback | — | `src/cpu.rs`, `src/cpu_sensors.rs` tests |
| Multi-server | — (only a **single** server was ever auto-discovered; parallel polling/isolation/selector never observed live) | Concurrency, per-server isolation, selector rendering, diagnostics inventory | — | `src/app.rs`, `src/discovery_llm.rs`, `src/ui.rs`, `src/diagnostics.rs` tests |
| TUI | Observed on both boxes (rendered against live data) | 116 `TestBackend` render/interaction tests | — | `src/ui.rs::tests` |
| Discovery / DRM / GPU mapping / domain | DRM/sysfs discovery + per-process fdinfo on the dev box; BDF identity on both boxes; NVML compute-app mapping on cf-desktop | Parser/shape branches; multi-render-node attribution; render-fd mapping branch; `Multi` mapping state | — | `discovery_real_system_smoke`, `drm_real_system_smoke`; `src/discovery.rs`, `src/drm.rs`, `src/gpu_map.rs`, `src/domain.rs` tests |
| Config / diagnostics / redaction | `orsiktop diag` on cf-desktop (`docs/HARDWARE_SUPPORT.md:48`, `:59`) and the dev box (`docs/S18_STAGE_SUMMARY.md:72`) | Parsing, sanitizing, no-secrets rendering; headless CI smoke (no GPU, no server) | — | `src/config.rs`, `src/diagnostics.rs`, `src/redact.rs` tests |

## Area detail

### LLM telemetry — llama.cpp

- **Live.** A **single-slot** remote llama.cpp server was observed from the dev
  box (`docs/V0_2_6_NOTES.md`, "slots 0 busy / 1 total"). Multi-slot
  `--parallel 2` was verified against a remote server (build b10909) over
  Tailscale and against the local cf-desktop server (S24): SLOTS 1/2, per-slot
  CTX `98,307/115,200`, MTP3, speculative counters (310,316 draft / 187,897
  accepted, 60.6%), aggregate live TPS, spec/MTP acceptance hold. The
  cf-desktop server is `--parallel 2` (`docs/HARDWARE_SUPPORT.md:24`), so it is
  **not** single-slot evidence. Recorded in `docs/HARDWARE_SUPPORT.md`
  (multi-slot row).
- **Fixture.** Parsing and aggregation are pinned to checked-in bodies:
  `tests/fixtures/metrics_current.prom`, `metrics_speculative.prom`,
  `metrics_partial.prom`, `slots_active.json`, `slots_idle.json`, `props.json`.
  Anchors: `parses_current_llama_metrics_fixture` (`src/llama.rs:1657`),
  `speculative_fixture_preserves_position_labels` (`src/llama.rs:1686`),
  `derives_live_context_from_slots_fixture` (`src/llama.rs:1779`),
  `derives_live_throughput_from_matching_slot_task` (`src/llama.rs:1802`),
  `idle_slots_fixture_reports_no_busy_slots` (`src/llama.rs:1842`),
  `props_fixture_exposes_context_slots_and_model` (`src/llama.rs:1859`),
  `context_pair_stays_with_most_used_busy_slot_when_capacities_differ`
  (`src/llama.rs:1899`), `context_slot_id_is_the_slot_api_id_not_the_array_position`
  (`src/llama.rs:1976`). Candidate ordering across several servers is tested
  (`src/discovery_llm.rs`, `src/app.rs`) but was never observed live.
- **Unknown.** None — every implemented llama.cpp path is live- or
  fixture-covered.

### LLM telemetry — Strata backend

- **Live.** Commit `0629648` records: "live validation against Strata 0.1.40
  confirmed totals are completion-batched and `tok_s` is the windowed rate."
  The schema in `src/strata.rs` is marked "verified against Strata build
  `0.1.40`". No box name is recorded for this run.
- **Fixture.** `maps_the_verified_strata_schema` (`src/strata.rs:439`, "Shape
  captured from Strata 0.1.40 (values synthetic)"), `live_rates_*`
  (`src/strata.rs:607` ff.), plus end-to-end fake-server tests
  `sample_reads_strata_json_metrics_end_to_end` (`src/llama.rs:1406`),
  `a_wire_format_switch_resets_the_counter_baseline` (`src/llama.rs:1505`),
  `sample_keeps_strata_context_occupancy_unknown_but_tracks_progress`
  (`src/llama.rs:1592`). The older/newer `/slots` schemas are checked in as
  `tests/fixtures/slots_strata_old.json` (0.1.40, no `n_prompt_tokens`) and
  `slots_strata_new.json` (adds `n_prompt_tokens`), covered by
  `strata_old_slots_report_capacity_only_never_occupancy` (`src/llama.rs:2208`)
  and `strata_new_slots_report_direct_occupancy` (`src/llama.rs:2228`). The
  newer `/slots` shape is captured, not live-re-verified.
- **Unknown.** None.

### GPU — NVIDIA discrete

- **Live.** RTX 4090 on cf-desktop under active inference: 97% util,
  VRAM 23.4/24.0 GiB, 369/450 W, 64 °C, fan 58%, cross-checked against a
  same-moment `nvidia-smi`. `docs/HARDWARE_SUPPORT.md` (NVIDIA row).
- **Fixture.** `src/providers/nvidia.rs::tests` (`src/providers/nvidia.rs:536`):
  unit conversion (`converts_nvml_pcie_kb_to_mb_per_second`), pstate/throttle
  formatting, `sanitize` clamping/dropping, slow-properties last-good
  retention, and MIG child-key construction. Multi-NVIDIA index selection and
  the UUID identity half are fixture-only (BDF is always primary).
- **Unknown.** None.

### GPU — AMD

- **Live.** None recorded — no AMD hardware on either box.
- **Fixture.** `src/providers/amd.rs::tests` (`src/providers/amd.rs:209`),
  e.g. `samples_amd_dgpu_full_sensor_set` (`:292`),
  `degrades_gracefully_when_hwmon_is_absent`,
  `malformed_sysfs_values_degrade_to_none_not_fakes`,
  `hwmon_is_matched_by_bdf_component`. APU shared-VRAM carveout shape is
  fixture-covered.
- **Unknown.** None.

### GPU — Intel

- **Live (xe iGPU).** Dev box: discovery, display name, live clock
  (`tile0/gt0/freq0/cur_freq` = 800 MHz), missing temp/power/fan/VRAM/util
  render `—`; `intel_real_system_smoke` (`src/providers/intel.rs:581`) and
  `docs/HARDWARE_SUPPORT.md` (Intel iGPU row).
- **Fixture.** `src/providers/intel.rs::tests` (`src/providers/intel.rs:266`):
  `samples_xe_dgpu_with_hwmon` (`:373`), `xe_apu_without_hwmon_keeps_clock_and_reason`,
  `samples_i915_from_card_directory` (`:471`),
  `i915_reads_clock_and_reasons_from_gt_gt0_fallback`,
  `missing_throttle_source_is_unavailable_not_none`,
  `pl1_limit_of_zero_is_treated_as_unavailable`. The Intel Arc dGPU (dGfx
  hwmon branch) and the i915 branch have no such hardware on either box.
- **Unknown.** None.

### GPU — NVIDIA MIG

- **Live.** None — no MIG-capable (A100/H100-class) hardware on either box.
- **Fixture.** Topology/keys in `src/providers/nvidia.rs` (`mig_child_key_*`,
  `src/providers/nvidia.rs:653` ff.); attribution in `src/gpu_map.rs`
  (`attribute_mig_child_when_instance_evidence_matches`,
  `opaque_mig_child_without_instance_evidence_never_matches`,
  `mig_child_evidence_maps_to_single_child` at `src/gpu_map.rs:671`); identity
  in `src/domain.rs` (`mig_uuid_detection` at `src/domain.rs:672`,
  `mig_instance_and_parent_come_only_from_the_legacy_slash_form`); UI matching
  in `src/ui.rs` (`gpu_in_mapping_matches_mig_parent_and_child`,
  `src/ui.rs:3723`).
- **Unknown.** The exact `MIG-…` UUID form a driver emits, and whether
  `Device::uuid()` on a child handle returns a `MIG-…` value at all. No MIG
  hardware is available, so neither can be settled by test.

### CPU and sensors

- **Live (dev box).** Hybrid P+E topology (`docs/S10_STAGE_SUMMARY.md`: P=4
  E=4 LP=0), cpufreq weighted average (`docs/S11_STAGE_SUMMARY.md`:
  `freq=Some(1505.9)` MHz), hwmon package/core temperature (coretemp, 46.0 °C),
  and RAPL power `None` under non-root (observed live absence, reported as `—`).
- **Fixture.** `src/cpu.rs` — `mod tests` (`src/cpu.rs:491`) holds the vendor
  identity tests (`parses_intel_identity_and_cleans_model` at `:508`,
  `parses_amd_identity` at `:516`); `mod fixture_tests` (`src/cpu.rs:596`)
  holds `fixture_detects_hybrid_topology_without_kernel_core_files` (`:630`),
  `fixture_detects_low_power_group_disjoint_from_atom`,
  `capacity_fallback_and_core_grouping_tolerate_offline_holes`.
  `src/cpu_sensors.rs::tests` (`src/cpu_sensors.rs:351`):
  `frequency_is_weighted_average_over_policies` (`:380`),
  `temperature_prefers_package_sensor_and_skips_non_cpu` (`:438`),
  `temperature_reads_an_amd_k10temp_tctl_sensor` (`:470`),
  `power_none_without_rapl` (`:569`), `power_none_when_energy_counter_unreadable`.
- **Unknown.** None. (RAPL under root / world-readable `energy_uj` is
  implemented and fixture-tested, but only ever observed `None` live under
  non-root; CPU fan telemetry is not implemented.)

### Multi-server

- **Live.** **Capability not verified.** Only a **single** local llama.cpp
  server was ever auto-discovered (cf-desktop S24: `LLM ENDPOINT
  127.0.0.1:8081 ·auto`, `diag` candidate with PID attribution). Parallel
  polling of several servers, per-server isolation, and the multi-server
  selector have no live observation. Single-server discovery is live; that is
  not multi-server operation.
- **Fixture.** Concurrency/isolation via in-process fake servers:
  `real_worker_polls_multiple_servers_independently` (`src/app.rs:2873`),
  `an_unreachable_server_does_not_block_a_healthy_one` (`src/app.rs:2921`),
  `a_slow_server_does_not_delay_a_healthy_servers_delivery` (`src/app.rs:3044`),
  `changed_identity_keys_detects_only_restarted_servers`. Per-server summaries:
  `build_server_summaries_distinguishes_unavailable_slots_from_real_zero`
  (`src/app.rs:2688`), `build_server_summaries_derives_phase_age_and_backend`
  (`src/app.rs:2774`). Discovery/ordering: `src/discovery_llm.rs::tests`
  (`selection_is_by_port_not_pid`, `discovers_all_servers_and_ignores_non_llama`).
  Selector rendering: `server_selector_shows_activity_phase_per_server`
  (`src/ui.rs:2661`), `server_selector_shows_measurement_age_and_interval`
  (`src/ui.rs:2710`), `server_selector_renders_unavailable_slots_without_a_fake_zero`
  (`src/ui.rs:2406`). Diagnostics:
  `multi_report_separates_states_and_bounds_the_inventory`
  (`src/diagnostics.rs:1001`),
  `probe_servers_reports_a_healthy_server_while_an_offline_one_is_isolated`
  (`src/diagnostics.rs:1200`).
- **Unknown.** None.
- **Known contradiction (needs a separate fix).** `docs/HARDWARE_SUPPORT.md:61`
  still says the multi-slot row has "one active `LlamaMonitor` … no
  simultaneous polling", which is stale and contradicts the implemented and
  tested behaviour (`real_worker_polls_multiple_servers_independently`,
  `src/app.rs:2873`). This new file follows the code and tests;
  `docs/HARDWARE_SUPPORT.md` should be corrected to match (follow-up outside
  this doc-only ticket).

### TUI

- **Live.** The TUI was observed rendering live data on both boxes (Intel
  iGPU/xe state, LLM slot state, `—` for unavailable metrics) per
  `docs/HARDWARE_SUPPORT.md` and `docs/V0_2_6_NOTES.md`.
- **Fixture.** `src/ui.rs::tests` (`src/ui.rs:1538`, 116 tests) render to a
  ratatui `TestBackend` buffer and assert on cells, covering the GPU panel PWR
  `—` semantics (`gpu_panel_renders_pwr_unavailable_without_power_telemetry`
  at `src/ui.rs:3863`), LLM panel/slot overview
  (`llm_panel_multi_slot_shows_per_slot_overview_below_the_ctx_row` at
  `src/ui.rs:4415`), absent metrics
  (`absent_metrics_render_as_unavailable_not_zero` at `src/ui.rs:4137`),
  compact layout thresholds, and interaction.
- **Unknown.** None.

### Supporting infrastructure

- **Discovery / DRM / GPU mapping / domain.** Live: DRM/sysfs discovery and
  per-process fdinfo on the dev box (`discovery_real_system_smoke`,
  `drm_real_system_smoke`), BDF identity on both boxes, NVML compute-app
  mapping on cf-desktop. Fixture: `src/discovery.rs::tests`
  (`discovers_single_nvidia_card` at `:272`,
  `discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf` at `:299`),
  `src/drm.rs` (17 `#[test]`: 16 plus the ignored smoke at `:904`),
  `src/gpu_map.rs::tests` (`:293`,
  `render_fd_maps_process_to_single_gpu` at `:345`), `src/domain.rs::tests`.
  Multi-render-node attribution, the render-fd mapping branch, and the `Multi`
  mapping state are fixture-only.
- **Config / diagnostics / redaction.** `orsiktop diag` is live on cf-desktop
  (`docs/HARDWARE_SUPPORT.md:48`, `:59`) and the dev box
  (`docs/S18_STAGE_SUMMARY.md:72`). Rendering redaction and parsing are
  fixture-tested (`src/diagnostics.rs::tests` at `:666`,
  `render_maps_local_server_process_to_render_gpu` at `:733`,
  `cpu_section_renders_fixture_topology` at `:805`; `src/redact.rs::tests`).
  The CI CLI smoke (`.github/workflows/ci.yml`, "CLI smoke test") runs on a
  headless runner with no GPU and no server, so it is a CI/fixture check, not a
  live evidence point.
- **Filesystem seam.** `system::Sys` (`src/system.rs:21`) with test-only
  `FixtureSys` (`src/system.rs:82`, `impl Sys for FixtureSys` at `:118`) is
  what makes the CPU, sensor, GPU-parser and diagnostics tests hardware-free;
  `real_read_dir_distinguishes_unreadable_from_empty` (`src/system.rs:203`)
  covers the real path.

## Audit method

Read-only. Every functional area was mapped to (a) its unit tests and fixture
files and (b) the recorded live runs in `docs/HARDWARE_SUPPORT.md`,
`docs/V0_2_6_NOTES.md`, the stage summaries and the commit messages. No live
probe, no server contact, no benchmark was performed for this matrix. Where a
live claim could not be traced to a recorded run, it was not asserted.

## Test-gap conclusion

The audit found exactly one concrete, unit-test-closable gap and it is now
closed by **one** targeted test:

- **AMD CPU thermal sensor names.** `is_cpu_thermal_driver`
  (`src/cpu_sensors.rs:315`) accepts `k10temp`, `zenpower` and `x86_pkg`, and
  `is_package_label` (`src/cpu_sensors.rs:325`) accepts `Tctl`, but every prior
  hwmon fixture used only `coretemp` with the label `Package id 0` (e.g.
  `src/cpu_sensors.rs:418`), and AMD-CPU coverage was otherwise limited to
  `parses_amd_identity` (`src/cpu.rs:516`). No test exercised the AMD driver
  name or the `Tctl` package label.
- **Added:** `temperature_reads_an_amd_k10temp_tctl_sensor`
  (`src/cpu_sensors.rs:470`) — a `FixtureSys` hwmon with `name=k10temp`,
  `temp1_label=Tctl` and a higher per-CCD reading, asserting
  `sample_temperature_c` returns the preferred `Tctl` value (47 °C) and that
  the AMD driver name is recognised. It fails if either the driver filter or
  the package-label preference stops accepting the AMD shape.

No other unit-test-closable gap was found. The remaining gaps are
**live-hardware gaps**, not test gaps: NVIDIA data-center GPUs, multi-NVIDIA
index selection, the UUID identity half, AMD dGPU/APU, Intel Arc dGPU + i915,
NVIDIA MIG (incl. the unknown `MIG-…` UUID form), AMD CPU, LP-E, and RAPL under
root all require hardware not present on either box
(`docs/HARDWARE_SUPPORT.md`, "Gaps → future work"). Multi-real-server
integration needs real endpoints, and long-running/benchmark tests are
explicitly out of scope for this ticket. The CFA-21 redraw throttling already
has a real regression test (`idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick`,
`src/app.rs:3162`).

If future hardware (AMD, MIG, Arc dGPU, an all-P Intel, root RAPL, or a second
real server) becomes available, the corresponding rows above should be promoted
from *fixture* to *live* with the box named.

## Cross-references

- `docs/HARDWARE_SUPPORT.md` — per-capability hardware matrix and evidence
  levels (detail; note the stale "no simultaneous polling" wording at `:61`).
- `docs/V0_2_6_NOTES.md` — validation record and pending live items.
- `docs/S10_STAGE_SUMMARY.md`, `docs/S11_STAGE_SUMMARY.md` — CPU topology,
  cpufreq, RAPL and sensor live validation.
- `docs/S18_STAGE_SUMMARY.md` — live `diag` report on the dev box.
