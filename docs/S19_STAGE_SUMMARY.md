# Stage Summary — S19 (Testability and fixtures)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/providers/amd.rs` | One new test in `providers::amd::tests`: `malformed_sysfs_values_degrade_to_none_not_fakes`. |
| `docs/S19_STAGE_SUMMARY.md` | This summary. |

S19 was executed as an audit first, then minimal targeted additions. The
fixture suite that prior stages already shipped covers almost the entire S19
checklist (see §3), so the stage adds exactly **one** new test, for the single
checklist item that had no coverage: **malformed sysfs data**.

## 2. Architecture changes

None. S19 is test-only; no production code, module structure, or behavior
changed. The stage's main output is the coverage map in §3, which documents
where each S19 requirement is already enforced.

## 3. Coverage map (S19 checklist → existing tests)

Every checklist item was located in the existing suite; each test name below
exists and is green:

| S19 requirement | Covered by |
| --- | --- |
| NVIDIA-like discovery | `discovery::discovers_single_nvidia_card` |
| AMD GPU (discrete) | `providers::amd::samples_amd_dgpu_full_sensor_set` |
| AMD APU | *No separate test — see §4.* The AMD provider has a single code path for dGPU and APU (same `mem_info_vram_*` files; the APU carveout is the kernel's own value). A dedicated APU fixture would re-assert identical assertions (padding). The APU shape is fixture-tested through the Intel APU analog (`providers::intel::xe_apu_without_hwmon_keeps_clock_and_reason`) for the "no dedicated VRAM/hwmon" degradation pattern, and the AMD path is the same code. |
| Intel i915 | `providers::intel::samples_i915_from_card_directory`, `i915_reports_throttle_reason_from_bool_files` |
| Intel xe | `providers::intel::samples_xe_dgpu_with_hwmon`, `xe_reports_throttle_reason_when_throttled` |
| iGPU + dGPU | `discovery::discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf` (two devices, BDF-sorted), `gpu::auto_falls_back_to_first_device_without_nvidia` (dispatch across an iGPU+dGPU list) |
| multi-GPU | `discovery::discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf`, `gpu::resolves_selector_against_discovered_gpus` (3-device list), `gpu_map::nvml_multi_device_yields_multi`, `gpu_map::multi_gpu_when_process_opens_two_render_nodes` |
| missing hwmon | `providers::amd::degrades_gracefully_when_hwmon_is_absent`, `providers::intel::xe_apu_without_hwmon_keeps_clock_and_reason`, `cpu_sensors::temperature_none_without_cpu_hwmon`, `power_none_without_rapl` |
| partial permissions | `cpu_sensors::power_none_when_energy_counter_unreadable` (root-only `energy_uj` → `None`), `providers::amd::device_not_readable_reports_unavailable`, `providers::intel::device_not_readable_reports_unavailable` |
| malformed sysfs/procfs data | **NEW** `providers::amd::malformed_sysfs_values_degrade_to_none_not_fakes` (non-numeric `gpu_busy_percent`/`mem_info_vram_total`, empty `product_name` → each metric `None`, name falls back to the card). Existing adjacent coverage: `cpu_sensors::temperature_ignores_impossible_values_and_falls_back_to_core`, `cpu::parse_cpu_list` rejection tests, `discovery::parse_bdf_accepts_valid_and_rejects_garbage`, `llama::parser_ignores_non_finite_values…`, `llama::rejects_non_array_slots_payload` |
| process PID reuse | `app::cache_treats_reused_pid_with_new_start_time_as_new_process`, `cache_reuses_stable_metadata_for_same_identity`, `cache_retain_drops_exited_processes` |
| DRM fdinfo variants | `drm::i915_ns_format_parses_bdf_driver_engines_and_memory`, `xe_cycles_format_and_first_sample_utilization_none`, `amdgpu_skips_deprecated_memory_aliases`, `multiple_fds_to_same_bdf_are_aggregated`, `fdinfo_without_pdev_is_rejected`, `unknown_and_driver_prefixed_keys_are_ignored`, `value_to_bytes_handles_units` |
| hybrid Intel CPU | `cpu::fixture_detects_hybrid_topology_without_kernel_core_files`, `fixture_detects_low_power_group_disjoint_from_atom`, `fixture_detects_performance_and_low_power_without_regular_e`, `fixture_p_e_without_low_power_keeps_zero_low_power` |
| homogeneous CPU | `cpu::fixture_detects_homogeneous_topology_as_unknown_kinds`, `capacity_classes_require_meaningful_difference` |
| AMD preferred-core data | *No test — see §4.* No `core_type` reader exists; S10 deliberately classifies via the architecture-neutral `cpu_capacity` (`capacity_classes_require_meaningful_difference`, `smt_classes_detect_two_thread_and_single_thread_cores`). There is no code path to test without inventing a feature. |
| llama.cpp endpoint responses | `llama::parses_current_llama_metrics_fixture`, `speculative_fixture_preserves_position_labels`, `idle_slots_fixture_reports_no_busy_slots`, `props_fixture_exposes_context_slots_and_model`, `slot_fallback_uses_processed_plus_decoded`, `rejects_non_array_slots_payload`, plus `tests/fixtures/*.prom|json` |
| server discovery | `discovery_llm::recognizes_both_llama_server_cli_forms`, `discovers_all_servers_and_ignores_non_llama`, `selection_is_by_port_not_pid`, `dedupes_identical_endpoints`, `discovered_beats_configured_when_auto_is_on`, `auto_discovery_off_excludes_processes`, `missing_proc_yields_no_candidates` |
| no dependency on dev hardware | All of the above run on `FixtureSys`/in-memory strings; the only `RealSys` tests are the three `#[ignore]` smokes (`discovery_real`, `intel_real`, `drm_real`), which are explicitly hardware-tolerant |

## 4. Design decisions (why two checklist items got no test)

- **AMD APU:** the provider is vendor-agnostic between dGPU and APU by
  construction (module doc §1: "On an APU the VRAM figures are the system-memory
  carveout the kernel itself reports through the same `mem_info_vram_*` files").
  There is no APU branch to exercise; an APU-named fixture would be a
  copy of `samples_amd_dgpu_full_sensor_set` with different byte counts — pure
  padding, which this project's test rules forbid.
- **AMD preferred-core (`core_type`):** S10's design explicitly rejects
  per-generation model tables in favor of the kernel's `cpu_capacity`, so no
  `core_type` reader exists in the tree. Testing "preferred-core data where
  available" would require first implementing a parser S10 chose not to have.
  If a future stage adds it, its test ships with it.

## 5. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **187 passed, 0 failed, 3 ignored**
(S19 adds 1 test). `cargo test --all -- --ignored` → **3 passed** (unchanged).

The new test's contract: a device directory that is *present but corrupt*
(truncated/non-numeric values, empty strings, absent files) must yield
`available: true` (the device exists) with every affected metric explicitly
`None` — the provider must not parse a partial number, clamp garbage into a
fake reading, or let one bad attribute poison the others. This is the AMD
provider's instance of the project-wide "missing data is explicit, never a
fake zero" rule (S2/S26).
