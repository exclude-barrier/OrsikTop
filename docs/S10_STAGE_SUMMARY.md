# Stage Summary — S10 (Modern CPU topology: LP-E distinction)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/cpu.rs` | `CpuCoreKind` gains a third real class, `LowPower` (Intel low-power E-core / LP-E), alongside `Performance`/`Efficiency`/`Unknown`. `CpuTopology` gains `pub low_power_cores: Option<usize>`. `detect_kernel_core_groups` now reads `/sys/devices/cpu_lowpower/cpus` and applies it **last** as `LowPower`. The three masks are the Intel x86 **perf hybrid-PMU** groups (`cpu_core`=big, `cpu_atom`=small, `cpu_lowpower`=tiny; `arch/x86/events/intel/core.c`, `intel_hybrid_pmu_type_map`): each logical CPU is assigned to exactly one PMU, so the masks are **mutually disjoint** and each core is classified exactly once (see §2, §5). `detect_topology`'s "resolved" gate generalizes from `has_both_core_kinds` (P∧E) to `has_multiple_core_kinds` (≥2 distinct classes), so a P+LP-E layout without regular E cores is already resolved and the capacity/SMT fallbacks do not overwrite it; `count_kind_groups` uses the same gate. |
| `src/ui.rs` | The exhaustive `CpuCoreKind` match in `core_heatmap_numbered_line` gains `LowPower => "L"` (per-core heatmap suffix). `system_panel_title` appends `+{l}L` to the hybrid topology text when `low_power_cores` is `Some(l > 0)` (skipped for 0, so the 2-class title is unchanged). The two-class P/E minibar (`physical_core_minibar_rows`) and `system_panel_height` are deliberately unchanged — see §4. |
| `CHANGELOG.md` | `[Unreleased]` gained the S10 Added entry. |
| `docs/S10_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **`LowPower` is a distinct class, never merged into `Efficiency`.** Before S10, `cpu_lowpower` members were mapped to `Efficiency` — i.e. LP-E cores were indistinguishable from regular E-cores. The kernel's `cpu_lowpower/cpus` mask is the authoritative LP-E signal. On parts with a "tiny" PMU (Arrow Lake-H class) LP-E cores form a **third** `cpu_capacity` band (LP-E < E < P, from `intel_pstate`'s per-core HWP normalization), but the sysfs group name — not the capacity value — is the structural discriminator and the path this code uses.
- **The three groups are Intel perf hybrid-PMU devices, and their masks are mutually disjoint.** `cpu_core` / `cpu_atom` / `cpu_lowpower` under `/sys/devices/` are not topology objects; they are the `supported_cpus` masks of the per-core-type perf PMUs registered by the Intel x86 driver (`arch/x86/events/intel/core.c`, `intel_hybrid_pmu_type_map`: big→`cpu_core`, small→`cpu_atom`, tiny→`cpu_lowpower`). A logical CPU is assigned to exactly one PMU at perf init (`init_hybrid_pmu` → `cpumask_set_cpu`) and removed in the CPU-dying hook, so no CPU appears in two groups — in particular LP-E cores are in `cpu_lowpower` **only**, never in `cpu_atom` (selected out of the ATOM type by native model ID, `INTEL_ATOM_CMT_NATIVE_ID` → hybrid_tiny). The detection order `cpu_core` → P, `cpu_atom` → E, `cpu_lowpower` → L is therefore just three disjoint `apply_kind` passes; "applied last" is harmless, not corrective.
- **`is_hybrid()` stays the P∧E UI-routing gate.** The per-core minibar renders exactly two columns (P/E); a P+LP-only or E+LP-only layout has no minibar and falls to the per-core heatmap, which now labels LP cores with `L`. Generalizing `is_hybrid()` to ≥2 classes would have routed 2-class-less layouts into the two-column minibar with a blank column, so it was intentionally left strict. The *detection* gate (`has_multiple_core_kinds`) is the one that widened.
- **`low_power_cores` is `Some(0)` on a resolved hybrid without LP-E** (e.g. this 288V box) and `None` when the topology is not resolved as multi-class — same explicit-`Option` convention as `performance_cores`/`efficiency_cores`. The title's `+{l}L` segment only renders for `Some(l > 0)`.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **172 passed, 0 failed, 3 ignored**
(S10 adds 3 `FixtureSys` tests; the pre-existing 169 stay green).
`cargo test -- --ignored` → **3 passed** (`discovery_real`, `intel_real`, `drm_real`).

- `fixture_detects_low_power_group_disjoint_from_atom` — disjoint hybrid-PMU masks `cpu_core=0-1`, `cpu_atom=2-3`, `cpu_lowpower=4-5`: each CPU lands in exactly one class, `P=Some(2)`, `E=Some(2)`, `LP=Some(2)`, `is_hybrid()`.
- `fixture_detects_performance_and_low_power_without_regular_e` — P+LP only, no `cpu_atom` file: the kernel-file result (2P + 2LP) is kept; the capacity fallback (which would have misclassified the LP band as E) does **not** overwrite it; `E=Some(0)` (explicit zero, not `None`).
- `fixture_p_e_without_low_power_keeps_zero_low_power` — the 288V shape (`cpu_core=0-3`, `cpu_atom=4-7`, no `cpu_lowpower`): `P=Some(4)`, `E=Some(4)`, `LP=Some(0)` — no regression of the current box.

Real-machine smoke on **this** box (Intel Core Ultra 9 288V, 8 logical CPUs, no
LP-E): a temporary `#[ignore]`d live test ran `detect_topology` on `RealSys` and
printed `P=Some(4) E=Some(4) LP=Some(0) hybrid=true`,
`kinds=[P,P,P,P,E,E,E,E]`, `physical=Some(8)` — matching the sysfs
(`cpu_core/cpus=0-3`, `cpu_atom/cpus=4-7`, no `cpu_lowpower` dir, capacities
P=1006/1024, E=642). The temporary test was removed afterwards.

## 4. Known limitations

- **LP-E cores are visible in the per-core heatmap (`L` suffix) and the panel
  title (`+{l}L`), but not in the two-column P/E minibar.** The minibar renders
  one column per class and `system_panel_height` is sized from the P/E row
  counts; adding a third `LP-CORES` column is deferred as YAGNI — the
  `cpu_lowpower` group exists only on Intel parts with a third "tiny" perf PMU
  (Arrow Lake-H class) and the heatmap already distinguishes them.
- **LP-E detection is Intel-only by construction.** `cpu_lowpower/cpus` is
  exported by the Intel x86 perf driver on hybrid parts; AMD exposes no
  equivalent `/sys/devices/<type>/cpus` groups (the kernel's
  `TOPO_CPU_TYPE_LOW_POWER` enum value for AMD only landed in 7.3-rc1, commit
  `924be9d6`), so AMD boxes remain all-`Unknown` (existing behavior, not a
  regression).
- **`cpu_capacity` is used as the P/E fallback only.** On parts without a tiny
  PMU (Meteor Lake, Lunar Lake, Panther Lake, Arrow Lake-U) there is no
  `cpu_lowpower` group at all — ATOM-type cores all sit in `cpu_atom`, so no
  distinct LP-E class exists there. On parts with a tiny PMU, `cpu_capacity`
  shows a third LP-E < E < P band but the code uses the group name, not the
  capacity value, as the discriminator.

## 5. Newly discovered issues

- **The pre-existing `cpu_lowpower` → `Efficiency` mapping was the bug fixed by
  this stage.** It was harmless on boxes without LP-E (the file is absent) but
  would have mislabeled every LP-E core as a regular E-core — exactly the S0
  audit gap ("LP-E distinction missing; `CpuCoreKind` has only P/E/Unknown").
- **The three group masks are mutually disjoint — a premise correction from the
  kernel research.** `cpu_core`/`cpu_atom`/`cpu_lowpower` are the Intel x86
  **perf hybrid-PMU** devices (`arch/x86/events/intel/core.c`,
  `intel_hybrid_pmu_type_map`; big→`cpu_core`, small→`cpu_atom`, tiny→
  `cpu_lowpower`), not topology objects — which is why exhaustive topology
  greps missed them. A logical CPU is assigned to exactly one PMU
  (`init_hybrid_pmu` → `cpumask_set_cpu`) and removed in the CPU-dying hook, so
  the three `cpus` masks never overlap: LP-E cores appear in `cpu_lowpower`
  **only**, never in `cpu_atom` (selected out of the ATOM type by native model
  ID `INTEL_ATOM_CMT_NATIVE_ID` → hybrid_tiny). The earlier interim note that
  "LP-E ⊂ cpu_atom" was wrong and is retracted here. Consequence: the fix is
  simply three disjoint `apply_kind` passes; the "applied last" ordering is
  harmless, not corrective, and no set difference is computed.
- **`cpu_lowpower` exists only on parts with a "tiny" PMU (Arrow Lake-H
  class).** Alder/Meteor/Lunar/Panther Lake and Arrow Lake-U register only
  big+small PMUs, so no `cpu_lowpower` dir exists (this 288V = Lunar Lake box
  shows exactly that: `cpu_core`/`cpu_atom`, no `cpu_lowpower`). `cpu_capacity`
  is the reliable LP-E-vs-E value discriminator (a third, lower band on
  ARL-H-class parts) and is set by `intel_pstate` from per-core HWP
  capabilities; the group name is the reliable structural one.
- **The "resolved" gate and the "countable" gate had diverged silently.**
  `count_kind_groups` was gated on `has_both_core_kinds`, so an absent class in
  a *resolved* hybrid returned `None` instead of `Some(0)` (caught by
  `fixture_detects_performance_and_low_power_without_regular_e`). Both now use
  `has_multiple_core_kinds`; the P/E-only classifiers keep the strict P∧E gate
  because they can only emit two classes.
- **`is_hybrid()` is overloaded: it is also the UI minibar-routing gate**, so
  widening it for LP-E would change rendering for P+LP-only boxes. It was kept
  strict (P∧E) and documented as the routing gate; the detection path uses the
  separate widened gate.
