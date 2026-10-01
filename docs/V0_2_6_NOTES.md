# v0.2.6 Development Notes

Durable notes for the current development cycle. Evidence levels follow
`docs/HARDWARE_SUPPORT.md`: **implemented** = code plus fixture/unit tests
running in CI; **live-verified** = additionally executed on real hardware
(name the box); **live validation pending** = no live run recorded yet.

## Release status

- v0.2.5 is released (2026-09-18) and immutable.
- v0.2.6 is the current version (2026-10-01): `Cargo.toml` = 0.2.6 and
  `CHANGELOG.md` `[0.2.6]`. The GitHub release/tag is created by the release
  workflow, not by this commit.
- The v0.2.6 line started from HEAD `6fbda08` (2026-09-20: llama multi-slot
  (2 commits), RAM fake-zero fix, DRM render-node process attribution, shared
  hwmon lookup, Intel/AMD device-name maps) and adds five correctness/privacy
  fixes: diagnostics URL redaction, session-tagged worker results, lossless
  endpoint saving, replacing spec-cache updates, identity-verified slot
  deltas. See `CHANGELOG.md` `[0.2.6]`. Automated tests: 304 regular pass,
  0 fail, 3 ignored hardware smoke tests; live hardware validation unchanged.

## llama.cpp multi-slot CTX — IMPLEMENTED + TESTED, live validation pending

Behavior (see `src/llama.rs`, `src/ui.rs`):

- Multi-slot llama.cpp servers (`--parallel > 1`) show which slot supplies
  the displayed CTX value via a compact `S<n>` tag on the CTX row.
- The tag names the slot by its actual `/slots` `id` — the real API id.
  Array position is never treated as slot identity.
- Selected slot id + used context + `n_ctx` remain one coherent selection
  from the same slot (no mixing across slots).
- BUSY-slot priority and most-used-BUSY selection are unchanged from
  v0.2.5 behavior.
- All-IDLE fallback selects the most-used retained slot.
- `LlmStats.context_slot_id` carries the selected slot's API id
  (`None` when the slot has no `id` field).
- `LlmStats.slot_overview: Vec<LlmSlotInfo>` provides per-slot visibility
  (`id`, busy state, used/total context).
- The per-slot SLOTS overview row reuses the existing `/slots` poll —
  there is no second HTTP polling loop.
- The main CTX row is never the sum of all slot contexts; it stays the
  authoritative selected-slot display.
- A trailing `*` in the overview marks the slot supplying the main CTX row.
- Single-slot servers do not get the redundant multi-slot UI (no tag, no
  overview row).
- On narrow layouts the overview row is dropped before any main CTX data
  is sacrificed.

Why: slot arrays can be reported in arbitrary order, so position-based
identity breaks as the slot set changes; the API `id` is stable. Reusing
the single `/slots` poll keeps monitoring overhead flat (project rule:
low monitoring overhead / YAGNI).

## RAM telemetry — IMPLEMENTED + TESTED, live validation pending

- RAM total == 0 or missing renders unavailable (`—`) instead of a
  synthetic `0.0%` / `0.0 GiB`, in both the system panel meter and the RAM
  history sparkline — matching existing VRAM/POWER semantics (missing
  telemetry is never a fabricated zero).
- Invalid RAM totals are not appended to RAM history (history skips
  samples without a valid total).

## Shared GPU hwmon lookup — IMPLEMENTED + TESTED in fixtures, live validation pending

Committed in `0b77ec2` (refactor(gpu): centralize hwmon device selection):

- `find_hwmon_for_bdf()` in `src/providers/mod.rs` is the shared hwmon
  matcher used by the AMD (`amdgpu`) and Intel (`i915`/`xe`) providers.
- A hwmon is a candidate when the BDF appears as a path component of its
  `device` symlink target; among candidates a hwmon whose `name` matches a
  known driver name wins, then one with a `temp1_input` sensor wins, and
  ties keep directory order (deterministic).
- `None` degrades gracefully — providers tolerate APUs and driver
  differences by leaving hwmon-only metrics `None`.
- Behavior change vs the old per-provider first-match: a driver-named
  hwmon block now wins over a same-BDF non-driver block (e.g. a
  power-domain `acpi_power` hwmon sitting first in `/sys/class/hwmon`),
  and a block with a `temp1_input` sensor wins over one without.

## Validation record (2026-09-20)

- `cargo test`: 269 passed, 0 failed, 3 ignored on the current working
  tree (covers llama slot selection/overview fixtures, RAM unavailable
  rendering, hwmon matching fixtures).
- LIVE-VERIFIED: nothing newly claimed in this cycle.
- LIVE VALIDATION PENDING: llama multi-slot CTX/overview on a live
  multi-slot llama.cpp server; RAM unavailable path on hardware reporting a
  zero/missing total; shared hwmon lookup on real AMD/Intel systems.

## GPU process attribution over all render nodes — IMPLEMENTED + TESTED

Committed in `3345d9e` (fix(gpu): improve DRM render-node process
attribution):

- `DiscoveredGpu.render_node` is now `render_nodes: Vec<String>` — every
  render node of the card is captured (Intel iGPU/xe expose a
  display+render node plus a compute-only node).
- `process_render_gpus` attributes a process to the GPU when its open fd
  targets ANY of the card's render nodes — a llama.cpp client on the
  compute-only node (e.g. `renderD129`) now attributes to the iGPU.
- The by-path BDF parser accepts 8-digit PCI domains and canonicalizes
  them through `normalize_pci_bdf` (4-digit default domain; ≥ `0x10000`
  stay 8-digit), completing the PCI-BDF-identity rule at this ingress.
- Live note: the current xe box exposes a single render node
  (`renderD128`); multi-node attribution is fixture-tested, not
  live-verified on this hardware.

## Intel/AMD device-name maps — IMPLEMENTED + TESTED

Committed in `6fbda08` (feat(gpu): improve Intel and AMD hardware
identification):

- AMD: when amdgpu exposes no `product_name` (common on APUs), a
  well-known device-ID map (Granite Ridge, Strix Halo, Rembrandt,
  Navi 33, RX 6600/6800/6900/7900/7700-7800 series) supplies a real name;
  unknown IDs keep the `cardN` fallback; a real `product_name` always
  wins.
- Intel: the device-ID map grows from 1 to 11 entries (Core Ultra 200V,
  Meteor Lake-P, Tiger Lake, Alder Lake, Arc A770/A750, Battlemage
  B580/B570).
- The i915 root-GT vs `gt/gt0` fallback regression test added here pins
  pre-existing behavior (clock and throttle reason from one directory);
  no behavior change.

## Release-readiness audit (2026-09-20, HEAD `6fbda08`)

Full local gate green: fmt/check/clippy OK, 269 passed / 0 failed /
3 ignored, release build OK, cargo audit clean (313 deps), CLI smoke OK
(`orsiktop 0.2.5`). CI + CodeQL (rust + actions) success on exact HEAD;
Release workflow published nothing (all build jobs skipped); latest
release still v0.2.5. One open CodeQL alert (`rust/cleartext-logging`,
`src/discovery.rs` `#[ignore]`d real-system test) is pre-existing
(2026-09-15) and test-only — not a release blocker.

Live validation on the current box (Intel Core Ultra 9 288V / xe,
Qwen3.8-27B at 100.99.68.81:8081):

- Intel xe LIVE-VERIFIED: discovery (`card0 0000:00:02.0 8086:64a0 xe
  renderD128`), display name (`Intel Arc (Core Ultra 200V iGPU)`),
  live clock (`tile0/gt0/freq0/cur_freq` = 800 MHz, TUI `CORE 800 MHz`),
  missing temperature/power/fan/VRAM/utilization all render `—`
  (no hwmon on the PCI device — expected, driver limitation).
- llama server is SINGLE-SLOT (`slots 0 busy / 1 total`): the `S<n>` CTX
  tag and SLOTS overview correctly do not render. Multi-slot live
  validation still PENDING (no `--parallel > 1` server reachable).
- AMD: fixture-tested only (no AMD hardware available).
- NVIDIA MIG: implemented + fixture-tested since v0.2.5; not
  live-verified (no MIG hardware available).

Audit findings (pre-existing, NOT introduced by the v0.2.6 delta):

1. **PWR meter fake zero** — `src/ui.rs` `draw_gpu`: the PWR meter row
   always renders `meter_line` with `power_pct.unwrap_or(0.0)` and no
   `None` branch, while GPU/VRAM/I-O rows render `unavailable_meter_line`
   (`—`) when their metric is `None`. On a box without power telemetry
   (this xe iGPU) the row shows an empty bar + `  0%` instead of `—` —
   indistinguishable from a real 0 W reading. Contradicts the documented
   "never a fabricated zero" rule (v0.2.2/v0.2.5 entries) and the v0.2.6
   RAM entry's "matching the existing VRAM/POWER semantics" claim.
   Pre-existing since before v0.2.5 (verified in `v0.2.5:src/ui.rs`).
   Small fix: give the PWR row the same `None => unavailable_meter_line`
   branch.
2. **CHANGELOG [Unreleased] gaps** — three user-visible changes missing:
   (a) GPU process attribution across all render nodes + 8-digit by-path
   BDF canonicalization (`3345d9e`); (b) Intel/AMD device-name maps
   (`6fbda08`); (c) hwmon driver-name scoring behavior change (`0b77ec2`).
   The RAM entry's "matching the existing VRAM/POWER semantics" wording
   is inaccurate while finding 1 stands (the POWER meter is not
   `—`-semantics).

Verdict at audit time: READY AFTER SMALL FIXES — add the three
CHANGELOG entries (honest validation levels: AMD/multi-slot/hwmon
scoring fixture-tested, Intel xe live-verified) and fix the PWR meter
`None` branch (or reword the RAM entry). No other blockers found:
no TODO/FIXME/unimplemented!/production panic, no debug prints, no
dependency or Cargo.toml/lock changes since v0.2.5, no local-machine
assumptions in the delta, version metadata all still 0.2.5 (correct
pre-bump state).

## Audit findings — RESOLVED (2026-09-21)

Both audit findings are fixed and the gates re-run green.

1. **PWR meter fake zero — FIXED.** `src/ui.rs` `draw_gpu` now builds
   `power_row` through `match power_pct` with a
   `None => unavailable_meter_line("PWR", …)` branch, mirroring the
   GPU/VRAM rows; the dead `power_pct_value` binding was removed.
   Covered by two new tests: `gpu_panel_renders_pwr_unavailable_without_power_telemetry`
   (no power telemetry → no `0%`, three `—` on PWR/VRAM/DRAW) and
   `gpu_panel_renders_pwr_meter_when_power_is_reported` (real telemetry
   still renders `33%`). The first was confirmed to FAIL against the
   reintroduced `power_pct.or(Some(0.0))` construction — it is a real
   regression test, not a tautology. This is an EXPANDED fix relative to
   the audit note, which had offered "or reword the RAM entry" as an
   alternative: the code was fixed instead, so the RAM entry's
   "matching the existing VRAM/POWER semantics" claim is now true.
2. **CHANGELOG [Unreleased] gaps — FIXED, and the gap count corrected.**
   The audit note said three missing entries and implied MIG was among
   them; verification shows MIG (`2b72a05`) is contained in the `v0.2.5`
   tag and was already documented in the v0.2.5 section, so it was never a
   gap — searching only the `[Unreleased]` block was the wrong scope for a
   v0.2.5-contained commit. The three genuine gaps (all v0.2.6 work,
   `git tag --contains` confirms none is in v0.2.5) are now entries:
   Intel/AMD device-name maps and multi-render-node process attribution
   under `### Added`; the hwmon driver-aware scoring behavior change under
   `### Fixed`. Evidence levels match `docs/HARDWARE_SUPPORT.md`: Intel xe
   live-verified, AMD and multi-node attribution and hwmon scoring
   fixture-tested.

Post-fix validation (2026-09-21, HEAD `6fbda08` + working tree):
`cargo fmt --all -- --check` OK, `cargo check --all-targets --locked`
OK, `cargo clippy --all-targets --locked -- -D warnings` OK,
`cargo test --all-targets --locked`: 271 passed, 0 failed, 3 ignored
(269 before, +2 new PWR tests). Still no production panics; still no
dependency or version-metadata change (0.2.5, pre-bump state is correct).

Remaining pre-release item: live validation of the multi-slot llama
UI and the AMD/hwmon paths still stands as PENDING, unchanged by this
fix — the PWR fix is itself covered only by the fixture-based render
tests, but it was authored against the live xe box's observed
"PWR 0%" behavior, which is what the tests reproduce.

