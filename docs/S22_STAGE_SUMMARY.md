# Stage Summary — S22 (Monolith decomposition: ui.rs, app.rs, llama.rs)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/ui.rs` | 5.717 → 253 lines: keeps the `UiState` struct, its `Default` impl, layout-agnostic constants, the color palette and all re-exports. |
| `src/ui/draw.rs` | New (542): the dashboard layout shell — full/compact/too-small draw paths, header, LLM/system, bottom and footer panels, layout floor constants, `RefreshControls`. |
| `src/ui/state_shell.rs`, `state_llm.rs`, `state_settings.rs`, `state_process.rs` | New (36/158/466/460): the 67-method `impl UiState` split into four domains. |
| `src/ui/tests/` | New: `mod.rs` (shared fixtures) + theme files `state.rs`, `llm_panel.rs`, `process_table.rs`, `layout_shell.rs`. The four pre-existing regression test files were not touched. |
| `src/app.rs` | 4.511 → 426 lines: keeps `run` (a ~150-line loop), snapshot application, refresh constants and re-exports. |
| `src/app/channels.rs`, `src/app/workers.rs`, `src/app/loop_events.rs` | New (57/367/329): channel helpers, the two background workers, and the per-iteration event handling. |
| `src/app/tests/` | New theme files: `mod.rs` (shared infra), `workers.rs`, `server_reconcile.rs`, `process_cache.rs`, `soak_scenarios.rs`; the pre-existing `soak.rs` was not touched. |
| `src/llama.rs` | 3.612 → 918 lines: only the test module moved out (Phase 1); the monitor itself stays. |
| `src/llama/tests/` | New: `mod.rs`, `metrics.rs` (1.395), `monitor.rs` (1.132), `speculative.rs` (167). |
| `docs/DEVELOPMENT.md` | Source map rows for the new modules and the `pub(super)`-plus-re-import pattern. |
| `docs/S22_STAGE_SUMMARY.md` | This summary. |

Branch: `refactor/decompose-monoliths` (unpushed), 14 commits on top of
`main` at `476bf5d`. Every step is one commit and independently
revertable.

## 2. What S22 required, and what was found

The plan (`~/.opencode/plan/orsiktop-monolith-zerlegung.md`) mandates:
decompose the three monoliths — `src/ui.rs` (5.717 lines), `src/app.rs`
(4.511), `src/llama.rs` (3.612) — of which ~73 % of the lines are tests in
a trailing `mod tests`. Rules per step: behavior-preserving moves only
(reviewed via `git diff -M` line-multiset comparison), one commit per
extraction, gate of `cargo fmt --check` + `cargo clippy --all-targets --locked
-- -D warnings` + `cargo test --all-targets --locked` with the test count
pinned at **589** (581 run + 8 ignored), following the existing
submodule pattern (`llama/prometheus.rs`, `app/tests/soak.rs`), and a
source-map update in `docs/DEVELOPMENT.md` after each phase.

## 3. The change, phase by phase

**Phase 1 — test modules out** (`1c76dc3`, `d28edcc`, `472ff6c`).
Each trailing `mod tests` became `src/<crate>/tests/mod.rs` plus theme
files grouped by unit under test (ui: state / llm panel / process table /
layout shell; app: workers / server reconcile / process cache / soak
scenarios; llama: metrics / monitor / speculative). Shared test
infrastructure (mock HTTP backends, soak helpers, identity constructors,
dashboard render fixtures) lives in each `tests/mod.rs`; theme files
import it via `super`. Result: ui 5.717 → 1.859, app 4.511 → 1.014,
llama 3.612 → 918.

**Phase 2 — `impl UiState` split** (`b6f7390`, `760dee4`, `e79f920`,
`9191269`). The struct and its `Default` impl stayed in `ui.rs`; the
1.070-line impl block left as separate `impl UiState` blocks in four
modules by domain: `state_shell.rs` (drop counters, help), `state_llm.rs`
(histories, samples, server selector), `state_settings.rs` (settings
dialog, server-list editor), `state_process.rs` (navigation, selection,
search, clicks) with their helper types. `ui.rs` 1.859 → 763. Compiler-
forced deviations, verified by multiset comparison (0 changed code
lines): moved types and their fields became `pub(super)` with a parent
re-import (effective visibility unchanged); three methods were raised to
`pub(crate)` because a sibling module or the `draw` path calls them
(`process_search_query`, `settings_endpoint`,
`clear_process_interaction`); `pub` methods in private child modules
proved callable from parent/sibling modules (verified empirically, kept).

**Phase 3 — `app.rs` unspooled** (`87a6fb7`, `fef81d3`, `934a3f8`).
`channels.rs` took `drain_try_recv` / `try_send_snapshot` /
`join_bounded`; `workers.rs` took `spawn_fast_worker`,
`spawn_llm_worker`, `PollFlag`, `next_cycle_target`,
`sleep_until_next_cycle`; `loop_events.rs` took the per-iteration work of
`run`: `pump_snapshots`, `handle_key_event`, `handle_mouse_event`.
`app.rs` 1.014 → 426, and `run` is now a thin loop (pump → server resync
→ paint → event dispatch). Compiler-forced deviations: `break`/`continue`
became return values (`bool` = quit; `Result<bool, io::Error>` for the
mouse path, where `Err` is a `crossterm::terminal::size()` failure); the
mouse handler runs inside the `Event::Mouse` match guard with `?` (legal
Rust, verified by a scratch compile, and required by
`clippy::collapsible_match`); formerly owned locals are now written
through their references (`*settings = …`); the two wide handlers carry
`#[allow(clippy::too_many_arguments)]` (existing codebase convention, cf.
`sync_server_set`); `clippy::needless_borrow` stripped redundant
`&`/`&mut` on reference parameters (no `mut` parameters needed — reborrow
and closure capture suffice); names only the tests use stay behind
`#[cfg(test)]` re-imports in `app.rs`.

**Phase 4 — `src/ui/draw.rs`** (`0dd3a48`). The ~530-line layout shell
left `ui.rs` (763 → 253): `draw` plus the compact/too-small paths, the
header, LLM/system, bottom and footer panels, the layout floor constants
(`COMPACT_MIN_*`, `FULL_MIN_*`, `REFRESH_CONTROL_WIDTH`) and
`RefreshControls`. `ui.rs` re-exports so every existing call path keeps
working: `pub use draw::draw` (the app loop calls `ui::draw`),
`pub use draw::{rect_contains, refresh_controls}` (mouse hit-testing in
`app/loop_events.rs` and the sibling modules), `pub(crate) use
draw::uses_full_layout`, and a `#[cfg(test)]` import of `draw_footer` /
`footer_status` for the layout tests. `draw.rs` imports the sibling panel
functions through the parent namespace (`use super::{…}`) so `ui.rs`
keeps owning those import bindings. The optional `src/llama/monitor.rs`
step (`impl LlamaMonitor`, 543 lines / 8 methods) was deliberately left
out of scope.

## 4. Known limitations

Stage-time record; both points have since moved on. The decomposition landed as
PR #98 (`18c5c4e`), and the release was made (v0.4.4).

- The branch is **unpushed**; `main` is untouched at `476bf5d`. A release
  (Unreleased → v0.4.3) is due but out of scope for this stage.
- `llama.rs` still contains `impl LlamaMonitor` (543 lines / 8 methods);
  the plan marks it low-urgency and it can be split later with the same
  pattern.
- The test modules' shared-infra `use super::{…}` lists in the parent
  modules grow as modules are added — mechanical, and each commit keeps
  the diff small (accepted risk from the plan).
- The `#[allow(clippy::too_many_arguments)]` attributes are a documented
  codebase convention for inherently wide signatures, not a lint
  relaxation.

## 5. Newly discovered issues

- No behavioral issues: every step passed the full gate and the
  line-multiset review (the only changed lines were visibility prefixes,
  the `break`/`continue` → return conversions, the reference-write
  adaptations and import splits).
- Lint-level surprises, all resolved and now documented:
  `unused_imports` treats a parent's import binding as used only when a
  non-test child module references it (glob or explicit) — test-only
  re-imports therefore need `#[cfg(test)]`, and `draw.rs` references the
  sibling functions through `super::` rather than the sibling modules
  directly so `ui.rs` keeps owning the bindings.
- `?` is legal inside match guards (verified by scratch compile before
  relying on it).
- Rebinding a `&mut T` parameter's referent and capturing it in closures
  does not require a `mut` parameter binding.

## 6. Test results

`cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D
warnings` and `cargo test --all-targets --locked` were green **after
every one of the 11 refactor commits**: **589 tests (581 passed + 8
ignored)** throughout — no test was added, removed or modified.

Final sizes: `src/ui.rs` 253, `src/app.rs` 426, `src/llama.rs` 918 lines
(was 5.717 / 4.511 / 3.612), with the logic now living in
`src/ui/draw.rs` + `src/ui/state_*.rs`, `src/app/{channels,workers,loop_events}.rs`
and the `tests/` trees.

## 7. Follow-up — `llama/monitor.rs` (post v0.4.3)

The one step deliberately left out of scope above landed afterwards as
PR #101 (`refactor(llama): extract LlamaMonitor into monitor.rs`): the
540-line `LlamaMonitor` impl moved from `llama.rs` (918 → 377 lines)
into `src/llama/monitor.rs`, same discipline — line-multiset-verified
pure move, one commit, **589 tests** green. Forced visibility changes:
5 impl methods and 3 helper types (`JsonOutcome`, `SlotCounter`,
`LocalSpeculativeConfig`) to `pub(crate)` (the tests call the methods
directly; E0446 for the types in method signatures) and `std::thread`
kept as a `#[cfg(test)]` re-import in `llama.rs` for the test glob.
