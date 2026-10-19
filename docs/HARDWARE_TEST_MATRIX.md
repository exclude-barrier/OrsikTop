# Hardware test matrix and operator test plan

Scope: the actionable test matrix for the hardware the Phase-2 roadmap
prioritises, the evidence level of each row, the reproducible instructions an
operator can run on the target hardware, and an explicit split between
**confirmed support** (live-verified) and **open hardware tests** (implemented
and fixture-tested, never observed live).

This file does not replace the other two hardware documents; it is the
test-actionable view over them:

- `docs/HARDWARE_SUPPORT.md` — per-capability hardware detail and evidence
  levels.
- `docs/LIVE_VS_FIXTURE.md` — per-functional-area live/fixture ledger.

The source code and its tests remain the source of truth. This matrix records
**no new live result**: every live claim it repeats is already recorded in
`docs/HARDWARE_SUPPORT.md` / `docs/LIVE_VS_FIXTURE.md`, and where hardware is
missing the gap is named, never filled with a fabricated value.

Matrix baseline: OrsikTop **v0.4.1**, git `2e1f0d2` (2026-10-17). Test
*names* are the stable key; Rust line numbers move with the tree and are
indicative only. The live-evidence rows below remain v0.4.0-era observations;
this rebaseline adds no new live claim.

## Evidence levels

Three levels, identical to `docs/LIVE_VS_FIXTURE.md`:

- **implemented** — the code path exists on the current HEAD.
- **fixture-tested** — exercised by unit tests / checked-in fixtures (or
  in-process fake servers) that run in CI; never observed on the target
  hardware.
- **live-verified** — additionally executed on real hardware; the box is named.

Live environments (both non-root, kernel `7.2.5-omarchy`):

- **Dev box** — Intel Core Ultra 9 288V (hybrid P+E), Intel Arc iGPU (xe driver,
  `0000:00:02.0`). No NVIDIA/AMD hardware.
- **test-desktop** — Intel Core i9-12900K (8P+8E/24T), Intel iGPU (i915,
  `0000:00:02.0`), NVIDIA GeForce RTX 4090 (`0000:01:00.0`), a local
  `llama serve` (b10909, `--parallel 2`). Captured 2026-09-18 (S24).
- **2026-10-18 `cf-desktop` diag:** the same i915/NVIDIA devices were
  discovered, but NVML was unavailable (`RM detects a driver/library version
  mismatch`, userspace `615.78`). The provider reported `—`/unavailable, not
  fake zeros; `diag` exited `0`.
- **2026-10-19 re-run:** driver `615.78.08` works; Auto reports RTX 4090
  utilization `0 %`, `37 °C`, `38.4 W`; `--gpu 0000:00:02.0` reports i915
  `clocks` only, with the other GPU fields `—`.

## Priority matrix

The five roadmap priorities in roadmap order; the hardware name is bold.
"Open" means the row has no live evidence on the target hardware.

| # | Hardware | Strongest evidence | Confirmed / open | Implemented | Fixture coverage (runs in CI) | Live-verified on | Missing hardware |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | **AMD Radeon** (amdgpu dGPU) | fixture-tested | **Open** | `src/providers/amd.rs`: `gpu_busy_percent`, `mem_info_vram_total`/`_used`, `product_name`; hwmon temperature, sclk/mclk, power, fan RPM | `samples_amd_dgpu_full_sensor_set` (`src/providers/amd.rs:292`), `degrades_gracefully_when_hwmon_is_absent` (`:322`), `product_name_overrides_card_name_and_missing_vram_is_none` (`:347`), `hwmon_is_matched_by_bdf_component` (`:439`), `malformed_sysfs_values_degrade_to_none_not_fakes` (`:462`) | — (no AMD GPU on either box) | any amdgpu dGPU |
| 2 | **Intel i915 / Arc dGPU** | i915 iGPU live, Arc dGPU fixture | **Partial** (i915 iGPU clock live; Arc dGPU open) | `src/providers/intel.rs`: i915 root-GT `rps_cur_freq_mhz` + `throttle_reason_*` (card dir, then `gt/gt0` fallback); dGfx hwmon `temp1_input` + `power1_max` (PL1) | `samples_i915_from_card_directory` (`src/providers/intel.rs:471`), `i915_reads_clock_and_reasons_from_gt_gt0_fallback` (`:502`), `missing_throttle_source_is_unavailable_not_none` (`:484`), `samples_xe_dgpu_with_hwmon` (`:373`), `pl1_limit_of_zero_is_treated_as_unavailable` (`:564`) | i915 iGPU clock live on `cf-desktop`; xe iGPU live on the dev box; Arc dGfx **not** live | an Intel Arc dGPU (i915 iGPU is now covered) |
| 3 | **AMD Ryzen/EPYC CPU sensors** | fixture-tested | **Open** | `src/cpu.rs` vendor-neutral identity; `src/cpu_sensors.rs` hwmon walker accepts the AMD driver names `k10temp` and `zenpower` (out-of-tree) plus the generic `x86_pkg` package-temp name (`is_cpu_thermal_driver`, `src/cpu_sensors.rs:315`) and the `Tctl` package label (`is_package_label`, `:325`); no hybrid-PMU files → `cpu_capacity` fallback | `temperature_reads_an_amd_k10temp_tctl_sensor` (`src/cpu_sensors.rs:470`), `temperature_reads_a_zenpower_package_sensor` (`src/cpu_sensors.rs:494`), `parses_amd_identity` (`src/cpu.rs:516`) | — (both boxes are Intel) | an AMD Ryzen or EPYC host |
| 4 | **NVIDIA Multi-GPU** | partially live | **Partially confirmed** | `src/discovery.rs` enumerates every display-class PCI GPU, sorted by BDF; `src/gpu.rs` `resolve_discovered` / `GpuSelector::Index(n)`; `src/gpu_map.rs` `Multi` mapping state | `discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf` (`src/discovery.rs:299`), `index_selects_the_nth_nvidia_on_igpu_dgpu_machines` (`src/gpu.rs:285`), `multi_gpu_when_process_opens_two_render_nodes` (`src/gpu_map.rs:460`) | test-desktop **iGPU + one NVIDIA** (both enumerated, BDF-sorted, `Auto`→NVIDIA) | a second NVIDIA GPU (2× NVIDIA index selection) |
| 5 | **NVIDIA MIG** | fixture-tested | **Open** | `src/providers/nvidia.rs` MIG topology at construction; `src/domain.rs` `MIG-…` identity; `src/gpu_map.rs` `gpu_instance_id`/`compute_instance_id` attribution; `src/ui.rs` parent↔child chip | `mig_child_key_prefers_driver_uuid` (`src/providers/nvidia.rs:653`) ff.; `mig_uuid_detection` (`src/domain.rs:672`); `attribute_mig_child_when_instance_evidence_matches` (`src/gpu_map.rs:617`); `gpu_in_mapping_matches_mig_parent_and_child` (`src/ui.rs:3753`) | — (no MIG-capable A100/H100) | an A100/H100-class MIG host |

## Evidence required per category (the open rows)

The priority matrix says **what** is implemented and fixture-tested; this table
states **what a live row must carry** — the HW-01 field list (exact model,
kernel/driver version, OrsikTop version, test method, real measurement, field
availability, reproducibility) — and which of those fields is still missing. A
missing field is left `—`; it is never filled by inference. The whole table is
`Open` for the same reason: neither test box carries the device (see *Live
environments*), so no live value can be recorded.

| Required field | AMD Radeon dGPU | AMD APU | Intel i915 | Intel Arc dGPU | AMD Ryzen/EPYC | 2× NVIDIA | NVIDIA MIG |
| --- | --- | --- | --- | --- | --- | --- | --- |
| exact model | — | — | — | — | — | — | — |
| kernel / driver version | — | — | — | — | — | — | — |
| OrsikTop version | v0.4.1 | v0.4.1 | v0.4.1 | v0.4.1 | v0.4.1 | v0.4.1 | v0.4.1 |
| test method | §4.1 | §4.1 | §4.2 | §4.2 | §4.3 | §4.4 | §4.5 |
| real measurement | — | — | — | — | — | — | — |
| field availability (provider; implemented + fixture-tested) | busy %, VRAM total/used, `product_name`, temp, sclk/mclk, power, fan | VRAM carveout via `mem_info_vram_*` | graphics clock, throttle reasons | `temp1_input`, `power1_max` (PL1) | `Tctl`/package temp (k10temp, zenpower) | per-GPU provider sample + `Index(n)` selection | MIG child identity + instance attribution |
| reproducibility | §4.1 | §4.1 | §4.2 | §4.2 | §4.3 | §4.4 | §4.5 |

`—` = not captured. Each `—` cell is filled only by a real run on the named
hardware, following the §4 procedure for that row and recorded as §5 requires.

Two notes that keep the open rows honest:

- **Intel iGPU on test-desktop.** Its iGPU *is* enumerated live (row 4
  above, `discovery_real_system_smoke`), but the Intel *provider* sample is not:
  the operator helper's fixed allow-list exposes the CI gate list only and does
  **not** include the `#[ignore]`d `intel_real_system_smoke`, so no live provider
  sample (clock, throttle reasons) was produced and the i915 provider branch
  stays **Open**. §2 and §4.2 record how the operator runs that smoke on the
  target host.
- **A fixture is not a live value.** A fixture test proves the parser shape on
  checked-in data; it never turns a row into *live-verified*. The `(fixture)`
  names in the matrix and in the table above stay evidence of parsing only.

## Reproducible test instructions

Every command below is a real OrsikTop entry point. The **CI/fixture** suite
runs anywhere Rust builds; the **live smokes** are `#[ignore]`d and must be
run explicitly on the target hardware. Per the project rules, CPU-intensive
builds/tests run through the operator-managed test-desktop helper
(`pc-project cargo ...`), never as a local laptop workaround. That helper's
allow-list covers the **CI/fixture** gates only; the `#[ignore]`d live smokes
are *not* part of it, so the operator runs them directly on the target host.

### 1. CI / fixture suite (no hardware required)

```sh
cargo test                 # whole suite; all provider parsers run against FixtureSys
cargo test providers::amd  # Priority 1 parser shapes only
cargo test providers::intel  # Priority 2 parser shapes only
cargo test cpu_sensors     # Priority 3 CPU-sensor shapes only
```

Expected: the suite is green; the provider tests assert parsing, not hardware
behaviour. (No fixed test count is quoted here — it drifts with the tree.)
A failure here is a fixture/parser regression, not a hardware finding.

### 2. Opt-in live smokes (`#[ignore]`d, real `/sys` and `/proc`)

```sh
cargo test -- --ignored --nocapture real_system_smoke
```

`--ignored` runs only the `#[ignore]`d tests, `--nocapture` keeps their
`println!`/`eprintln!` output (the harness otherwise swallows the output of a
passing test), and the `real_system_smoke` filter narrows the run to the three
hardware smokes. **Do not** run a bare `cargo test -- --ignored`: it also runs
two heavy tests —

- `perf_baseline_soak_and_process_collection_benchmark` (`src/app.rs:3659`)
- `soak_many_set_servers_rounds_bound_resources_single_threaded` (`src/app.rs:4715`)

— which their own doc comments restrict to an approved single-threaded runner
and which are **not** part of the hardware check (a failure of the leak probe
in a parallel gate is meaningless, not a hardware finding).

The three smokes differ in what they assert; only the printed output gives the
full picture:

| Smoke | Location | What it does | What it asserts |
| --- | --- | --- | --- |
| `discovery_real_system_smoke` | `src/discovery.rs:452` | prints every DRM-discovered GPU | at least one GPU is found (`:469-472`), every GPU carries a BDF (`:474-477`) and the vendor read succeeded (`:479-483`) — it **can** go red |
| `intel_real_system_smoke` | `src/providers/intel.rs:581` | prints one Intel provider sample; returns `skipping` when the host has no Intel GPU | `available`, the device key, and `utilization`/`memory_total_mib` are `None` — **not** clock, throttle reason, temperature or PL1 |
| `drm_real_system_smoke` | `src/drm.rs:911` | counts processes holding a DRM fd and prints each | nothing — output only; its doc comment says it always passes |

So a green run is not by itself vendor telemetry: reading the printed output is
what turns it into evidence. For AMD (Priority 1) the asserted discovery evidence
is `discovery_real_system_smoke`, the provider evidence is `diag` — there is no
AMD provider smoke. See §4.

### 3. Manual end-to-end check

```sh
cargo run -- diag                       # no-secrets diagnostics summary
cargo run -- diag --servers             # multi-server overview (up to 16 endpoints)
cargo run -- --gpu 0000:41:00.0 diag    # pin a GPU by BDF, for a loggable check
cargo run -- --gpu MIG-<uuid> diag      # pin a MIG child (Priority 5)
```

`diag` prints the DRM-discovered GPUs, then one provider sample for the selected
GPU (`GPU provider (selector = …)`), then the server→GPU mapping line (`gpu map`
in the single report, `gpu` under `--servers`); unavailable metrics render `—`
(the project invariant), never `0`.
Top-level flags (`--gpu`, `--gpu-index`) come **before** the `diag` subcommand
(see the code comment `orsiktop --server … diag` at `src/main.rs:129`);
`--servers` is a `diag` flag and comes after it.

`--gpu`/`--gpu-index` without a subcommand launches the TUI; add `diag` for a
protocollable, non-interactive sample.

### 4. Per-priority procedure (run on the target hardware)

For each priority, on a host that has the hardware:

1. **AMD Radeon (Priority 1)** — on an amdgpu dGPU host.
   - `cargo test -- --ignored --nocapture discovery_real_system_smoke` — the
     asserted discovery evidence: it fails when no GPU is found, a GPU carries no
     BDF, or the vendor read fails (`src/discovery.rs:469-483`).
   - `cargo run -- diag` — the provider evidence. There is **no** AMD provider
     smoke; `drm_real_system_smoke` only counts DRM-fd holders (no assertion) and
     is not an AMD result.
   - `diag` prints per card a `[i] <card> <bdf> vendor=…` line plus a
     `pci … driver=… render=…` line, then `GPU provider (selector = …)` with
     `device`, `utilization`, `temperature`, `power`, and the capability lines
     `available`/`unavailable`. VRAM totals,
     clock values and fan RPM are **not** printed as numbers — only the
     capabilities `memory`/`clocks`/`fan` appear; the selected card's
     `product_name` is not a separate field (the provider sets `device`).
   - The APU "shared carveout vs. discrete" distinction is **not** observable
     from `diag` (or the TUI): the tool reports whatever the kernel exposes
     through `mem_info_vram_*` (`src/providers/amd.rs:16-18`). Record the value,
     not an interpretation of it.
2. **Intel i915 / Arc dGPU (Priority 2)** — on an i915 host or an Intel Arc
   dGPU host.
   - `cargo test -- --ignored --nocapture intel_real_system_smoke` and
     `cargo run -- diag`.
   - The smoke asserts only `available`, the device key and that
     utilization/VRAM are `None`; it returns `skipping` on a host with no Intel
     GPU and does **not** assert clock, throttle reason, temperature or PL1 —
     read those from the printed sample.
   - Expect on i915: graphics clock (`rps_cur_freq_mhz`) and throttle reasons,
     with the `gt/gt0` fallback where the root GT has no files. On `dGfx`:
     `temp1_input` and `power1_max` (PL1) from hwmon. GPU busy, VRAM and
     instantaneous power are `None` by design and render `—`.
3. **AMD Ryzen/EPYC CPU sensors (Priority 3)** — on an AMD CPU host.
   - `cargo run -- diag` (CPU section) and a read-only driver check
     `grep . /sys/class/hwmon/hwmon*/name`.
   - The `CPU sensors` section prints only `frequency`, `temperature` and
     `power` (`src/diagnostics.rs:236-253`); it does **not** print the hwmon
     driver name nor whether the package/`Tctl` value was preferred, so `diag`
     alone confirms neither. The driver name comes from the hwmon `name` read
     above (`k10temp` or `zenpower`; the generic `x86_pkg` name is accepted by the
     walker but is not AMD-specific evidence). Non-hybrid topology (no P/E split)
     uses the capacity fallback.
4. **NVIDIA Multi-GPU (Priority 4)** — on a host with **two** NVIDIA GPUs.
   - `cargo run -- diag` (lists both cards and their BDF under `GPU discovery`),
     then `cargo run -- --gpu-index 1 diag`.
   - The provider section shows the selector and the chosen `device` name but
     **not** the selected card's BDF/UUID (`src/diagnostics.rs:182-189`), so two
     identical cards cannot be told apart from the output; compare with the BDF
     list, or force `--gpu <bdf>` per card. `Index(n)` selecting the n-th NVIDIA
     and a two-render-node process yielding `GpuMapping::Multi` remain the
     fixture-tested behaviour (open here).
5. **NVIDIA MIG (Priority 5)** — on an A100/H100 MIG host.
   - `cargo run -- --gpu MIG-<uuid> diag` and `cargo run -- diag --servers`.
   - `diag` does **not** print a MIG-child list: `discover_mig_children` is
     called only to build the server→GPU mapping (`src/diagnostics.rs:285-286`,
     `:486-494`). What is observable is (a) that `--gpu MIG-<uuid>` resolves the
     child as a selector (`GpuSelector::Uuid` → `NvidiaGpuProvider::new`,
     `src/gpu.rs:42-43`) and yields a provider sample, and (b) the mapping line —
     `gpu map` in the single report (`src/diagnostics.rs:307`), `gpu` under
     `diag --servers` (`:561-564`) — attributing a server to the MIG child.
   - A child's identity is the driver `MIG-…` UUID when present, else the
     synthetic `{parent}#mig-{slot}`; per-MIG metrics NVML does not expose
     render `—`. Also report the exact `MIG-…` string the driver emits and
     whether `Device::uuid()` on a child handle returns it (see Open list below).

### 5. Recording evidence

A live result counts only when it is captured as: the **host** (box name),
the **command** and its exit code, the **raw output**, and the **snapshot hash**
when run through `pc-project`. A claim that cannot be tied to such a record
stays `implemented`/`fixture-tested`.

## Confirmed support vs. open hardware tests

### Confirmed support (live-verified)

- **NVIDIA discrete GPU** — RTX 4090 on test-desktop under active inference
  (util, VRAM, power, temperature, fan, PCIe link), cross-checked against a
  same-moment `nvidia-smi`.
- **Intel integrated GPU (xe, iGPU/APU)** — xe iGPU on the dev box (discovery,
  live clock, throttle reason; temp/power `None` by design).
- **NVIDIA Multi-GPU discovery** — test-desktop i915 iGPU + one NVIDIA 4090: both
  enumerated and BDF-sorted; `Auto` resolves to the NVIDIA. *(Index selection
  across 2+ NVIDIA remains open — see below.)*
- **DRM/sysfs discovery and per-process fdinfo** — dev box.
- **Hybrid and conventional Intel CPU**, cpufreq, hwmon temperature — dev box.
- **llama.cpp / Strata backend telemetry** — remote + local `--parallel 2`
  servers and a Strata `0.1.40` build.

### Open hardware tests (implemented + fixture-tested, never live)

| # | Open test | Required hardware | Reproducible instruction |
| --- | --- | --- | --- |
| 1 | AMD Radeon dGPU telemetry | an amdgpu dGPU host | §4.1 |
| 1b | AMD APU `mem_info_vram_*` carveout source | an AMD APU host | §4.1 |
| 2 | Intel i915 branch + Intel Arc dGfx hwmon | an i915 host or an Arc dGPU | §4.2 |
| 3 | AMD Ryzen/EPYC CPU thermal sensors | an AMD Ryzen/EPYC host | §4.3 |
| 4 | NVIDIA index selection across 2+ NVIDIA; `Multi` mapping | a 2× NVIDIA host | §4.4 |
| 5 | NVIDIA MIG topology, identity and process attribution | an A100/H100 MIG host | §4.5 |
| 6 | RAPL power under root / world-readable `energy_uj` | a root deployment | `cargo run -- diag` as root |
| 7 | CPU fan telemetry | — | not implemented |

**Unknown (neither live nor fixture), must stay explicitly unknown:**

- The exact `MIG-…` UUID form a driver emits, and whether `Device::uuid()` on a
  MIG child handle returns a `MIG-…` value at all. Only MIG hardware can settle
  this; no test can.
- CPU fan telemetry — not implemented, so there is nothing to observe.

## Operator test requirements

Requested from the operator (no live result is claimed until one is returned):

| Priority | Hardware / driver | Command | Expected | Return |
| --- | --- | --- | --- | --- |
| 1 | amdgpu dGPU (any modern Radeon) | `cargo test -- --ignored --nocapture discovery_real_system_smoke`; `cargo run -- diag` | card discovered with BDF + driver; provider `device`; `utilization`/`temperature`/`power` values or `—`; `available`/`unavailable` names `memory`/`clocks`/`fan` | host, command, raw output, snapshot hash |
| 1 | AMD APU | `cargo run -- diag` | `memory` capability present; value is the kernel `mem_info_vram_*` figure — carveout vs. discrete is not distinguishable from the output | same |
| 2 | i915 host (pre-Xe) | `cargo test -- --ignored --nocapture intel_real_system_smoke`; `cargo run -- diag` | smoke prints the sample; clock + throttle reasons, `gt/gt0` fallback where needed (neither asserted) | same |
| 2 | Intel Arc dGPU | `cargo test -- --ignored --nocapture intel_real_system_smoke`; `cargo run -- diag` | temp + PL1 from hwmon in the printed sample; busy/VRAM `—` | same |
| 3 | AMD Ryzen or EPYC | `cargo run -- diag`; `grep . /sys/class/hwmon/hwmon*/name` | `temperature` value; driver name from hwmon (`k10temp`/`zenpower`); package preference not observable via `diag` | same |
| 4 | 2× NVIDIA | `cargo run -- diag`; `cargo run -- --gpu-index 1 diag` | both cards listed with BDF; provider shows selector + `device` name, not the picked BDF/UUID | same |
| 5 | A100/H100 MIG | `cargo run -- --gpu MIG-<uuid> diag`; `cargo run -- diag --servers` | child resolves as selector + provider sample; `gpu map`/`gpu` line attributes the server to the child; exact `MIG-…` string noted | same + the emitted UUID string |

## Method / audit note

This matrix was assembled read-only from the current `docs/HARDWARE_SUPPORT.md`
and `docs/LIVE_VS_FIXTURE.md`, the provider source and its tests. No live probe,
no server contact and no benchmark was performed. Every test name and file:line
was checked against the source at the current HEAD. Where a live claim could not
be traced to a recorded run it is not asserted; where hardware is absent the row
is marked **Open**, not guessed.

Test names re-checked 2026-10-17 against `2e1f0d2`: all 38 cited identifiers are
present in `src/`. Line numbers remain indicative.

Historical re-verification 2026-10-14 against git `e9600d6`: all cited test/function names and
their `file:line` were re-resolved in the working tree; one stale reference was
corrected (`gpu_in_mapping_matches_mig_parent_and_child`, `src/ui.rs` 3728 →
3753). The evidence-required table above was added for HW-01; it asserts no new
live result and leaves every hardware-dependent cell `—`.

## Cross-references

- `docs/HARDWARE_SUPPORT.md` — per-capability detail and the "Gaps → future
  work" list this matrix operationalises.
- `docs/LIVE_VS_FIXTURE.md` — per-functional-area live/fixture ledger.
- `README.md` — CLI (`orsiktop diag`, `--gpu`, `--gpu-index`).
