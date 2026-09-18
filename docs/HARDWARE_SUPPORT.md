# Hardware support matrix

Scope: which hardware and telemetry capabilities OrsikTop implements at the
current HEAD, and which of them are live-verified versus fixture-tested only.
The source code is the source of truth; this matrix reflects it, not the
vendor abstractions.

## Evidence levels

Two levels:

- **implemented** — code plus fixture tests that run in CI.
- **live-verified** — additionally executed on real hardware.

Two live-verification environments; each live claim names its box:

- **Dev box** — Intel Core Ultra 9 288V (hybrid P+E, no LP-E cores), Intel
  Arc iGPU (xe driver, PCI `0000:00:02.0`), no NVIDIA/AMD GPU, no AMD CPU,
  non-root, kernel `7.2.5-omarchy`. The opt-in live smokes
  (`discovery_real_system_smoke`, `intel_real_system_smoke`,
  `drm_real_system_smoke`; all `#[ignore]`d, not part of CI) all pass here.
- **cf-desktop** — Intel Core i9-12900K (8P+8E/24T), Intel iGPU (i915,
  `0000:00:02.0`), NVIDIA GeForce RTX 4090 (`0000:01:00.0`), a local
  llama.cpp server (`llama serve`, b10909, `--parallel 2`), non-root,
  kernel `7.2.5-omarchy`. The NVIDIA + local-llama.cpp (S24) environment,
  captured 2026-09-18.

## Status values

- **Supported** — implemented and live-verified where applicable.
- **Partial** — implemented but with explicit gaps (see the last column).
- **Unsupported** — not implemented.
- **Untested** — implemented and fixture-tested, but never run on the target
  hardware.

A provider abstraction existing does not by itself count as support.

## Matrix

| Capability | Status | Evidence | Missing / why unverified |
| --- | --- | --- | --- |
| NVIDIA discrete GPU | Partial | `src/providers/nvidia.rs` (NVML via `nvml-wrapper`): utilization, memory total/used/free, encoder/decoder, power, temperature, clocks, fan, PCIe link info; `sanitize()` clamps or drops non-finite values; init failure becomes an `error` string, never a fake zero; enumeration cached at construction; device resolved by UUID or PCI bus id, never a bare ordinal; fixture tests in `providers::nvidia`. **Live-verified on an RTX 4090 (cf-desktop)** during active inference: GPU 97% / VRAM 23.4/24.0 GiB / 369/450 W / 64 °C / fan 58% / PCIe TX 251.5 MB/s — semantically consistent with a same-moment `nvidia-smi` (91% / 23947/24564 MiB / 63 °C / 387.98 W) | Only a GeForce RTX 4090 is live: data-center GPUs and multi-NVIDIA index selection remain untested; MIG is implemented but untested (see the NVIDIA MIG row) |
| AMD discrete GPU | Partial (Untested) | `src/providers/amd.rs` (amdgpu sysfs + per-BDF hwmon): `gpu_busy_percent`, `mem_info_vram_total`/`_used`, `product_name`; hwmon temperature, sclk/mclk, power, fan RPM; fixtures `samples_amd_dgpu_full_sensor_set`, `malformed_sysfs_values_degrade_to_none_not_fakes` (run in CI) | No AMD hardware on the live box |
| Intel discrete GPU (Arc, xe/i915) | Partial | `src/providers/intel.rs`: xe per-GT sysfs graphics clock + throttle reasons; i915 root-GT `rps_cur_freq_mhz` + `throttle_reason_*` booleans (card dir, then `gt/gt0` fallback); hwmon exists only for dGfx and provides `temp1_input` + `power1_max` (PL1); by design `None`: GPU busy, memory bandwidth, VRAM total/used, instantaneous power draw, fan max; fixtures in `providers::intel` (i915 shapes built from kernel source) | Live-verified only for the xe branch on the dev box's iGPU (`intel_real_system_smoke`, `clock=Some(800.0)`); the dGfx hwmon branch and i915 are not live (no Arc dGPU on the box) |
| Intel integrated GPU (iGPU/APU) | Supported | xe APU at `0000:00:02.0` on the dev box: all three `#[ignore]`d smokes pass (discovery, intel, drm); clock + throttle reason from GT sysfs; temperature and power limit degrade to `None` because APU iGPUs register no hwmon (documented by design, `docs/S7_STAGE_SUMMARY.md`) | — |
| AMD integrated GPU / APU | Partial (Untested) | `src/providers/amd.rs`: VRAM is the kernel-reported system-memory carveout (`mem_info_vram_*`), reported as shared, never presented as discrete VRAM; where hwmon is absent, thermal/clock/power/fan degrade to `None`; fixtures cover the carveout shape | No AMD APU hardware |
| NVIDIA MIG | Partial (Untested) | Stage S17: `src/providers/nvidia.rs` builds the MIG topology once at construction per physical device (`mig_mode()` enabled → `mig_device_count()` → probe each `mig_device_by_index`); a child's identity is its driver MIG UUID (`Device::uuid()` on the child handle) with a synthetic `{parent-uuid}#mig-{slot}` fallback; `MIG-…` UUIDs parse to `GpuSelector::Uuid` and resolve through `device_by_index(parent).mig_device_by_index(slot)` (name = parent name + ` MIG {slot}`); process→child mapping via `running_compute_processes()` `gpu_instance_id`/`compute_instance_id` (`src/gpu_map.rs` `attribute_process_keys` — one placement yields exactly one identity, unmatched placements degrade to the parent); the LLM chip matches MIG parent↔child (`src/ui.rs` `gpu_in_mapping`). Child metrics NVML does not expose per MIG handle (temp/power/fan/clocks/PCIe) render `—`, never fake zeros | **Assumption (unverified):** `Device::uuid()` on a MIG child handle returns the `MIG-GPU-<parent>-<gi>-<ci>` UUID; if a driver returns the parent's UUID or nothing, the synthetic fallback keeps identity stable but process→child attribution degrades to the parent. **Limitation:** topology is captured at construction only — an admin re-partition requires an OrsikTop restart. No MIG hardware (A100/H100-class) on either live box; fixture-tested only (`providers::nvidia` `mig_child_key_*`, `gpu_map` `attribute_*`/`mig_child_evidence_*`/`combine_mig_*`, `domain` `mig_uuid_*`, `ui` `gpu_in_mapping_matches_mig_parent_and_child`) |
| Multi-GPU systems | Partial | Discovery enumerates every display-class PCI GPU, sorted by BDF (fixture `discovery::discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf`); **live on a 2-GPU machine (cf-desktop: i915 iGPU + NVIDIA 4090)** — both enumerated and BDF-sorted in `diag`, `Auto` resolves to the NVIDIA; `GpuSelector::Index(n)` is the n-th NVIDIA on mixed machines and the n-th overall on NVIDIA-less machines (`src/gpu.rs` `resolve_discovered`, `docs/S1_S4_STAGE_SUMMARY.md`); multi-GPU process evidence yields `GpuMapping::Multi` (fixture `multi_gpu_when_process_opens_two_render_nodes`) | Index selection across multiple NVIDIA GPUs and the `Multi` mapping state remain fixture-only |
| Heterogeneous GPU systems | Partial | Mixed-vendor discovery + per-vendor provider dispatch is fixture-tested (the iGPU+dGPU discovery fixture above); **live on cf-desktop (i915 iGPU + NVIDIA 4090)**: both discovered, `Auto` selects the NVIDIA (`src/gpu.rs` `resolve_discovered`), the selected GPU is sampled via its vendor provider | The iGPU's own provider was never sampled there (`Auto` takes the NVIDIA), so dual-vendor concurrent telemetry is unverified |
| Intel hybrid CPU (P + E cores) | Supported | `src/cpu.rs` `detect_topology`: hybrid-PMU cpumasks (`/sys/devices/cpu_core|cpu_atom|cpu_lowpower/cpus`), disjoint masks, each CPU in exactly one class → P/E/LP-E; live (`docs/S10_STAGE_SUMMARY.md`): P=4 E=4 LP=0, hybrid=true, matching sysfs on the 288V. LP-E (third class): implemented + fixture-tested (`fixture_detects_low_power_group_disjoint_from_atom`); UI renders the P/E minibar for two classes, LP-E via heatmap suffix + `+nL` title | LP-E not live: no LP-E cores on this box |
| Conventional Intel CPU | Partial | Same topology pipeline; non-hybrid shapes covered by `cpu::fixture_tests` (`cpu_capacity` fallback, SMT sibling heuristic); vendor/model from `/proc/cpuinfo`; cpufreq/hwmon paths are vendor-neutral | The live box is a hybrid Intel; an all-P Intel has not been live-tested |
| AMD CPU | Partial (Untested) | Vendor-neutral `/proc/cpuinfo` vendor/model; cpufreq + hwmon temperature walkers include AMD sensor names (`k10temp`, `zenpower`, `x86_pkg` — `src/cpu_sensors.rs`); no hybrid-PMU files → `cpu_capacity` fallback → SMT heuristic; fixtures in `cpu::fixture_tests` | No AMD CPU on the live box |
| cpufreq | Supported | `src/cpu_sensors.rs`: per-policy frequencies, weighted average across policies (not per-core); live (`docs/S11_STAGE_SUMMARY.md`): `freq=Some(1505.9)` MHz | — |
| hwmon temperatures / fans / power (CPU) | Partial | Temperature live on this box (coretemp, 46.0 °C; preferred package/Tctl over per-core; −20..150 °C range filter, out-of-range dropped, not clamped); power via RAPL (next row) — live result here is `None` (non-root); CPU fans: **not implemented** (no fan reading exists in `src/cpu_sensors.rs`). GPU fans: AMD hwmon fan RPM + NVIDIA NVML fan are implemented (both untested live); Intel has no fan telemetry (`None` by design) | CPU fan telemetry missing; power `None` under non-root (RAPL row) |
| RAPL | Partial | Fully implemented: `/sys/class/powercap` `intel-rapl`, two `energy_uj` reads across a bounded ~50 ms window (kernel `udelay` bound where present); fixtures `power_none_without_rapl`, `power_none_when_energy_counter_unreadable` | `power=None` on the dev box because `energy_uj` is root-only (`0400 root:root`) and OrsikTop runs non-root (`docs/S11_STAGE_SUMMARY.md` §4); reports real watts under root or on a world-readable kernel |
| DRM/sysfs discovery | Supported | `src/discovery.rs`: `/sys/class/drm` → PCI device dir, PCI class base 0x03 required, results sorted by BDF (enumeration order is never identity); vendor from driver binding (`nvidia`/`amdgpu`/`i915`/`xe`) else PCI vendor ID (0x10de/0x1002/0x8086); live: `discovery_real_system_smoke` on the xe card | — |
| DRM fdinfo per-process telemetry | Supported | `src/drm.rs`: `/proc/<pid>/fd` → filter symlinks to `/dev/dri/*`, read `/proc/<pid>/fdinfo/<fd>`, parse canonical `drm-*` keys; per-BDF resident/total bytes, per-engine `busy_ns` or `cycles_busy/cycles_total` → derived utilization; baseline state pruned each cycle; 13 fixture tests + `drm_real_system_smoke` (observed the display server's DRM fds) | — |
| Stable PCI BDF / UUID identity | Supported | `src/domain.rs` `DeviceId`: PCI BDF primary, vendor UUID secondary; `card0` names and NVML ordinals are never identity; enumeration is BDF-sorted; BDF live on both boxes (xe iGPU `0000:00:02.0` on the dev box; i915 iGPU + NVIDIA 4090 on cf-desktop) — the 4090 carries one canonical 4-digit BDF (`0000:01:00.0`) through discovery, provider, and LLM mapping | UUID resolution goes through NVML enumeration only (NVIDIA); the UUID half never rendered live because the BDF is primary |
| llama.cpp server discovery | Partial | `src/discovery_llm.rs`: `/proc` cmdline scan for `llama-server` / `llama serve`, endpoint from `--host`/`--port` args, deterministic order (lowest port, then endpoint string) — never by PID — de-duplicated, all candidates kept; configured `server=` honored via `ServerSource::Configured`; no live reachability probe (S15). **Live on cf-desktop (S24)**: a local `llama serve` process (b10909) was auto-discovered with no `--server`; TUI header `LLM ENDPOINT 127.0.0.1:8081 ·auto`, status connected; `diag` reports the candidate with PID attribution (pid 1224246) | Multi-candidate ordering (lowest port) remains fixture-only: one local server was observed |
| llama.cpp → GPU mapping | Partial | `src/gpu_map.rs`: render-fd evidence (the process's open `/dev/dri` nodes) + NVML compute-app evidence, de-duplicated by stable key; explicit Single/Multi/Unknown states (`src/domain.rs`, `Default == None`); surfaced in the TUI by S23 (commit `88a7efc`: "LLM" chip on the GPU panel + `GPU <BDF/UUID>` line in the LLM panel); 12 `gpu_map::tests` + domain tests. **Live on cf-desktop (S24)**: the local server's NVML compute-app evidence maps to `GpuMapping::Single`, key `0000:01:00.0`, evidence `NvmlCompute`; the TUI rendered the `LLM` marker on the GPU panel and the `GPU 0000:01:00.0` identity line. Live validation caught and fixed a BDF-domain mismatch (NVML's 8-digit `00000000:01:00.0` never unified with sysfs's 4-digit form — identity line rendered 8-digit, name/vendor unresolved): ingress normalization `normalize_pci_bdf` at every BDF entry point, 4 regression tests | The render-fd (non-NVML) evidence branch and the `Multi` state remain fixture-only; an explicit `--server` has no PID attribution, so its mapping is `Unknown` by design |
| Multi-slot / parallel llama.cpp | Partial | Slot-based sampling of a single server via `/slots` (slot-based live TPS, S14); **live-verified against a remote `--parallel 2` llama.cpp server (b10909, Qwen3.8-27B UD-Q4_K_M) over Tailscale**: the context pair is read from one display slot (most-used busy, else most-used) — idle frames showed the most-used slot's retained pair (98,315/115,200, cross-checked against `/slots`) and busy frames showed the busy slot's own pair (1,277/115,200 while another slot retained 79,549); **also live-verified against a local `--parallel 2` server on cf-desktop (S24)**: SLOTS 1/2, per-slot CTX 98,307/115,200 tok, MTP3, spec counters 310,316 draft / 187,897 accepted (60.6%); aggregate live TPS over active slots; spec/MTP counters incl. the 3 s acceptance hold; 14 regression tests in `llama.rs`; multiple local servers remain candidates switchable via `server=` (S15), one active `LlamaMonitor` (`src/app.rs` `spawn_llm_worker`), no simultaneous polling | — |
| Unavailable/unsupported telemetry fallback | Supported | Project invariant: missing metric → `None` → rendered "—", never a fake 0. NVML init failure → `error` string; malformed sysfs → `None` (`malformed_sysfs_values_degrade_to_none_not_fakes`); non-root RAPL → `None` (observed live on this box); APU without hwmon → temp/power `None` (observed live on this box) | — |

## Gaps → future work

Items that should become engineering tasks:

- NVIDIA coverage beyond the RTX 4090: data-center GPUs, multi-NVIDIA index selection, and the UUID half of identity (NVML-resolved, never rendered live because the BDF is primary).
- Live verification on AMD hardware (amdgpu dGPU + APU providers, AMD CPU sensor names) — needs AMD hardware.
- Live verification of i915 + Intel Arc dGPU hwmon branch — needs that hardware; xe is already live.
- NVIDIA MIG live verification — implemented in S17 (fixture-tested), needs A100/H100-class MIG hardware; the `Device::uuid()`-on-MIG-child assumption and the restart-on-repartition limitation should be confirmed there.
- CPU fan telemetry — not implemented.
- RAPL under root (or world-readable `energy_uj`) — implemented, needs a root deployment to be live-verified.
- Multiple local llama.cpp candidates: ordering (lowest port) and switching via `server=` remain fixture-only (one local server observed live); the render-fd mapping branch and the `Multi` state are fixture-only.
