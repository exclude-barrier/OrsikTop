//! TestBackend regression tests for the CTX row width behaviour of the LLM
//! panel (`draw_llm` and `draw_llm_metrics_unavailable`).
//!
//! The CTX row appends the "used / capacity tok" pair after a bar whose width
//! is pinned via `inner.width.saturating_sub(46).max(8)`. At narrow panels
//! (inner width < 54 — e.g. the full-layout floor `FULL_MIN_WIDTH = 72` with
//! the 65 % LLM pane gives panel 46 → inner width 44) or with long context
//! values (e.g. capacity 1.048.576), the row exceeds the panel's inner width
//! and the pair is cut off: "tok"/digits become only partially visible.
//!
//! These tests pin the required behaviour. Written against the unfixed state
//! (CFA-56, before the CFA-38 production fix): the clipped cases (tests 1 and
//! 3) and the degradation cases (tests 4 and 5) must fail there and pass once
//! `src/ui/llm.rs` implements the full rule; the exact-fit boundary (test 2)
//! is green in both states and guards that the fix does not degrade values
//! that already fit. Deterministic: pure `TestBackend`, no real server, no
//! timing.
//!
//! The degradation oracle is deliberately strict about the line *tail*
//! (`ends_with`), not merely about the digits being present: a row that keeps
//! the numbers but clips the unit (" t"/" to") or that drops the pair's
//! separator ("1,048,576/…") must not pass.

use super::{draw_llm, GpuMapping, LlmStats, UiState};
use ratatui::{backend::TestBackend, layout::Rect, Terminal};

/// Render the LLM panel into a row-major text snapshot (one String per
/// terminal row, borders included) so the CTX row can be asserted against
/// the real buffer.
fn render_llm_panel_rows(llm: &LlmStats, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_llm(frame, area, llm, &UiState::default(), &GpuMapping::None))
        .unwrap();
    let content = terminal.backend().buffer().content();
    (0..height)
        .map(|row| {
            content[(row as usize * width as usize)..((row + 1) as usize * width as usize)]
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect()
        })
        .collect()
}

/// Connected server with `/metrics` and `/slots`, single slot (no slot tag),
/// the occupancy/capacity pair under test.
fn ctx_stats(used: u64, capacity: u64) -> LlmStats {
    LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: capacity,
        context_used: Some(used),
        slots_available: true,
        slot_count: 1,
        busy_slots: 1,
        ..Default::default()
    }
}

/// Same occupancy pair, but the server is reachable without `/metrics`:
/// `draw_llm` must route to `draw_llm_metrics_unavailable`, which builds the
/// CTX row with the same width rule.
fn ctx_stats_metrics_unavailable(used: u64, capacity: u64) -> LlmStats {
    LlmStats {
        metrics_available: false,
        ..ctx_stats(used, capacity)
    }
}

/// The rendered CTX row inside the panel borders (first/last cell stripped,
/// trailing padding trimmed) — i.e. exactly what the Paragraph had room for.
/// A row that was clipped by the panel shows its truncated tail here.
/// Fails loudly if no CTX row was drawn.
fn ctx_inner_row(rows: &[String]) -> String {
    rows.iter()
        .filter(|row| row.contains("CTX"))
        .map(|row| {
            row.chars()
                .skip(1)
                .take(row.chars().count().saturating_sub(2))
                .collect::<String>()
        })
        .map(|inner| inner.trim_end().to_string())
        .next()
        .unwrap_or_else(|| panic!("no CTX row drawn in the panel:\n{}", rows.join("\n")))
}

#[test]
fn narrow_panel_ctx_row_keeps_the_full_pair_with_a_large_capacity() {
    // Full-layout floor 72 → 65 % LLM pane → panel 46 → inner width 44.
    // Capacity 1.048.576 with 78.800 used: the pair must stay complete.
    // Pre-fix the row is clipped at the border: "…1,048,576 t" — "ok" gone.
    let llm = ctx_stats(78_800, 1_048_576);
    let rows = render_llm_panel_rows(&llm, 46, 12);
    let text = rows.join("\n");
    assert!(
        text.contains("78,800 / 1,048,576 tok"),
        "the CTX pair must be fully visible at inner width 44, got:\n{text}"
    );
    let ctx = ctx_inner_row(&rows);
    assert!(
        ctx.ends_with("78,800 / 1,048,576 tok"),
        "the CTX row must end with the complete pair, not a clipped tail, got: {ctx:?}"
    );
}

#[test]
fn narrow_panel_ctx_row_keeps_a_capacity_that_fits_exactly() {
    // Boundary: capacity 115.200 is the longest value whose full pair still
    // fits the current row budget at inner width 44 — it must stay fully
    // visible, and must stay visible after the fix (the fix may only degrade
    // longer values, never ones that already fit).
    let llm = ctx_stats(78_800, 115_200);
    let rows = render_llm_panel_rows(&llm, 46, 12);
    let text = rows.join("\n");
    assert!(
        text.contains("78,800 / 115,200 tok"),
        "the exactly-fitting CTX pair must stay fully visible at inner width 44, got:\n{text}"
    );
    let ctx = ctx_inner_row(&rows);
    assert!(
        ctx.ends_with("78,800 / 115,200 tok"),
        "the CTX row must end with the complete pair, got: {ctx:?}"
    );
}

#[test]
fn metrics_unavailable_panel_keeps_the_same_ctx_width_rule() {
    // Reachable server without `/metrics`: the `/slots`-derived CTX pair must
    // obey the same width rule as the metrics panel.
    let llm = ctx_stats_metrics_unavailable(78_800, 1_048_576);
    let rows = render_llm_panel_rows(&llm, 46, 10);
    let text = rows.join("\n");
    assert!(
        text.contains("78,800 / 1,048,576 tok"),
        "the metrics-unavailable CTX pair must be fully visible at inner width 44, got:\n{text}"
    );
    let ctx = ctx_inner_row(&rows);
    assert!(
        ctx.ends_with("78,800 / 1,048,576 tok"),
        "the CTX row must end with the complete pair, not a clipped tail, got: {ctx:?}"
    );
}

#[test]
fn overflowing_ctx_row_drops_the_unit_before_any_digit() {
    // Degradation contract: when the pair *with* unit does not fit, only the
    // unit " tok" is given up first — the complete "used / capacity" pair,
    // separator included, stays intact and no digit is ever truncated.
    // Panel 43 → inner width 41. Pre-fix the bar stays pinned and the row is
    // clipped mid-number ("…78,800 / 1,048,57"), cutting real digits; a form
    // that also drops the separator ("1,048,576/…") would violate the rule as
    // well. The exact tail is asserted, not just the presence of the digits:
    // a row that keeps the numbers but clips the unit would otherwise pass.
    let llm = ctx_stats(78_800, 1_048_576);
    let rows = render_llm_panel_rows(&llm, 43, 12);
    let ctx = ctx_inner_row(&rows);
    assert!(
        ctx.ends_with("78,800 / 1,048,576"),
        "the CTX row must end with the complete 'used / capacity' pair once the \
         unit is dropped, got: {ctx:?}"
    );
    assert!(
        !ctx.contains("tok") && !ctx.ends_with(" t") && !ctx.ends_with(" to"),
        "the unit must be dropped in full, with no ' t'/' to' remainder, got: {ctx:?}"
    );
}

#[test]
fn overflowing_metrics_unavailable_ctx_row_keeps_the_same_degradation() {
    // The /slots-derived CTX row of a reachable server without /metrics
    // (`draw_llm_metrics_unavailable`) follows the identical degradation rule:
    // the unit goes first, the pair and its separator stay whole (Panel 43 →
    // inner width 41).
    let llm = ctx_stats_metrics_unavailable(78_800, 1_048_576);
    let rows = render_llm_panel_rows(&llm, 43, 10);
    let ctx = ctx_inner_row(&rows);
    assert!(
        ctx.ends_with("78,800 / 1,048,576"),
        "the metrics-unavailable CTX row must end with the complete pair once \
         the unit is dropped, got: {ctx:?}"
    );
    assert!(
        !ctx.contains("tok") && !ctx.ends_with(" t") && !ctx.ends_with(" to"),
        "the unit must be dropped in full, with no ' t'/' to' remainder, got: {ctx:?}"
    );
}
