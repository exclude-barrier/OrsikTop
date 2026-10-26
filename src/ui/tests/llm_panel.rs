//! LLM panel and server-selector rendering tests.
use super::super::*;
use super::{render_llm_panel, summary};

#[test]
fn server_selector_renders_unavailable_slots_without_a_fake_zero() {
    // Regression for the M4 report: a connected server whose `/slots`
    // endpoint is down (metrics-only) appeared as "S 0/0". Unknown slots
    // must render `—/—`, never a fabricated zero.
    let area = Rect::new(0, 0, 80, 20);
    let metrics_only = vec![summary("metrics-only", true, false, 0, 0)];
    let text = render_server_selector_text(&metrics_only, "metrics-only", area);
    assert!(
        text.contains("S —/—"),
        "unavailable slots must render —/—:\n{text}"
    );
    assert!(
        !text.contains("S 0/0"),
        "unavailable slots must not fabricate 0/0:\n{text}"
    );

    // When /props still knows the total, the known capacity survives as
    // `—/N` rather than being dropped.
    let mut known_total = summary("metrics-only", true, false, 0, 0);
    known_total.props_slot_count = 3;
    let text = render_server_selector_text(&[known_total], "metrics-only", area);
    assert!(
        text.contains("S —/3"),
        "a known /props total must render as —/3:\n{text}"
    );
    assert!(!text.contains("S 0/0"), "{text}");

    // A valid empty `/slots` array is a genuine zero and stays numeric.
    let empty = vec![summary("empty", true, true, 0, 0)];
    let text = render_server_selector_text(&empty, "empty", area);
    assert!(
        text.contains("S 0/0"),
        "a real empty /slots array is 0/0:\n{text}"
    );
}

#[test]
fn server_selector_shows_real_counts_and_no_cross_server_contamination() {
    let area = Rect::new(0, 0, 80, 20);
    let servers = vec![
        summary("healthy", true, true, 2, 4),
        summary("metrics-only", true, false, 0, 0),
        summary("dead", false, false, 0, 0),
    ];
    let text = render_server_selector_text(&servers, "healthy", area);
    assert!(text.contains("S 2/4  "), "healthy count lost:\n{text}");
    // Two unavailable rows, no fabricated 0/0 anywhere.
    assert!(!text.contains("S 0/0"), "fabricated zero leaked:\n{text}");
    assert_eq!(
        text.matches("S —/—").count(),
        2,
        "both unknown rows:\n{text}"
    );

    // A small but usable terminal still renders the row (the popup is
    // dropped only below its hard floor of a 48-column popup).
    let small = Rect::new(0, 0, 56, 10);
    let text = render_server_selector_text(&servers, "healthy", small);
    assert!(
        text.contains("S 2/4"),
        "small terminal lost the row:\n{text}"
    );

    // Below the floor it degrades quietly instead of panicking.
    let tiny = Rect::new(0, 0, 40, 4);
    let text = render_server_selector_text(&servers, "healthy", tiny);
    assert!(
        !text.contains("S 2/4"),
        "too-small popup should not draw:\n{text}"
    );
}

#[test]
fn server_selector_scrolls_to_keep_the_highlight_visible() {
    // 12 servers do not fit the popup inner height at 80x12, so the list
    // must scroll. Every server stays reachable by moving the highlight,
    // and the title carries an N/M position indicator throughout.
    let servers: Vec<ServerSummary> = (0..12)
        .map(|i| summary(&format!("s{i:02}"), true, true, 1, 4))
        .collect();
    let area = Rect::new(0, 0, 80, 12);
    for index in 0..servers.len() {
        let text = render_selector(&servers, "s00", index, &[], area);
        let label = format!("s{index:02}");
        assert!(
            text.contains(&label),
            "highlight {index} must stay visible:\n{text}"
        );
        let position = format!("{}/{}", index + 1, servers.len());
        assert!(
            text.contains(&position),
            "position indicator {position} missing:\n{text}"
        );
    }
}

#[test]
fn server_selector_keeps_the_not_monitored_block_visible_when_scrolled() {
    // The overflow block must not be pushed off-screen by the scrolled
    // server list: it is pinned below the server viewport, so configured
    // but unmonitored endpoints are never silently cut off.
    let servers: Vec<ServerSummary> = (0..16)
        .map(|i| summary(&format!("s{i:02}"), true, true, 1, 4))
        .collect();
    let overflow = vec![
        crate::app::ServerSpec {
            key: "o1".to_string(),
            endpoint: "http://10.0.0.9:8080".to_string(),
            identity: None,
        },
        crate::app::ServerSpec {
            key: "o2".to_string(),
            endpoint: "http://10.0.0.10:8080".to_string(),
            identity: None,
        },
    ];
    let area = Rect::new(0, 0, 80, 16);
    let text = render_selector(&servers, "s00", 15, &overflow, area);
    assert!(
        text.contains("s15"),
        "last server must stay visible:\n{text}"
    );
    assert!(
        text.contains("NOT MONITORED"),
        "overflow block must stay visible:\n{text}"
    );
    assert!(
        text.contains("16/16"),
        "position indicator missing:\n{text}"
    );
}

#[test]
fn server_selector_keeps_the_highlighted_detail_line_visible_at_odd_heights() {
    // Regression (F5): each server now renders two lines, which made the
    // effective viewport odd at many terminal heights. The scroll offset
    // was rounded down to an even line, dropping the highlighted entry's
    // *detail* line (slots, CTX, tok/s, age) out of the window — at the
    // last entry it was not reachable at all. Each server carries a unique
    // CTX marker, so `contains` proves the detail line itself is on screen,
    // not merely its header. `render_selector` returns only the drawn
    // buffer, so an off-screen detail is genuinely absent.
    let servers: Vec<ServerSummary> = (0..5)
        .map(|i| {
            let mut server = summary(&format!("s{i:02}"), true, true, 1, 4);
            server.context_used = Some(1000 + i);
            server.context_size = 9999;
            server
        })
        .collect();
    // 80x15 gives an odd popup inner height (no overflow block).
    let area = Rect::new(0, 0, 80, 15);
    for index in 0..servers.len() {
        let text = render_selector(&servers, "s00", index, &[], area);
        let marker = format!("CTX {}/9999", 1000 + index);
        assert!(
            text.contains(&marker),
            "highlight {index} detail line ({marker}) must be visible at 80x15:\n{text}"
        );
    }

    // Same invariant with a pinned overflow block shrinking the viewport to
    // an odd height, matching the existing scrolled-overflow configuration
    // (80x16, 16 servers, 2 overflow endpoints).
    let servers: Vec<ServerSummary> = (0..16)
        .map(|i| {
            let mut server = summary(&format!("s{i:02}"), true, true, 1, 4);
            server.context_used = Some(1000 + i);
            server.context_size = 9999;
            server
        })
        .collect();
    let overflow = vec![
        crate::app::ServerSpec {
            key: "o1".to_string(),
            endpoint: "http://10.0.0.9:8080".to_string(),
            identity: None,
        },
        crate::app::ServerSpec {
            key: "o2".to_string(),
            endpoint: "http://10.0.0.10:8080".to_string(),
            identity: None,
        },
    ];
    let area = Rect::new(0, 0, 80, 16);
    for index in 0..servers.len() {
        let text = render_selector(&servers, "s00", index, &overflow, area);
        let marker = format!("CTX {}/9999", 1000 + index);
        assert!(
                text.contains(&marker),
                "highlight {index} detail line ({marker}) must be visible at 80x16 with overflow:\n{text}"
            );
        assert!(
            text.contains("NOT MONITORED"),
            "the pinned overflow block must stay visible at highlight {index}:\n{text}"
        );
    }
}

#[test]
fn server_selector_shows_activity_phase_per_server() {
    let area = Rect::new(0, 0, 80, 20);
    let mut generating = summary("gen", true, true, 1, 1);
    generating.phase = Some(ActivityPhase::Generating);
    let mut prefilling = summary("prefill", true, true, 1, 1);
    prefilling.phase = Some(ActivityPhase::Prefill);
    let mut idle = summary("quiet", true, true, 0, 1);
    idle.phase = Some(ActivityPhase::Idle);
    // No telemetry at all: the phase must stay unknown, never `IDLE`.
    let unknown = summary("unknown", false, false, 0, 0);
    // A dropped connection keeps an explicit held-state label.
    let mut reconnecting = summary("recon", false, false, 0, 0);
    reconnecting.reconnecting = true;
    reconnecting.phase = Some(ActivityPhase::Reconnecting);
    let servers = vec![generating, prefilling, idle, unknown, reconnecting];
    let rows = render_selector_rows(&servers, "gen", area);

    // Check the phase column itself, not just "somewhere on the row".
    for (label, expected) in [
        ("gen", "GENERATING"),
        ("prefill", "PREFILL"),
        ("quiet", "IDLE"),
        ("recon", "RECONNECTING"),
    ] {
        let (header, _) = selector_entry(&rows, label);
        let cell = selector_cell_after_label(header, label, PHASE_OFFSET, PHASE_WIDTH);
        assert_eq!(
            cell.trim(),
            expected,
            "phase column of {label} must read {expected}: {rows:?}"
        );
    }

    let (unknown_header, _) = selector_entry(&rows, "unknown");
    let unknown_cell =
        selector_cell_after_label(unknown_header, "unknown", PHASE_OFFSET, PHASE_WIDTH);
    assert_eq!(
        unknown_cell.trim(),
        "—",
        "a missing phase must render — in the phase column: {rows:?}"
    );
    assert_ne!(
        unknown_cell.trim(),
        "IDLE",
        "a missing phase must not be guessed as IDLE: {rows:?}"
    );
}

#[test]
fn server_selector_shows_measurement_age_and_interval() {
    let area = Rect::new(0, 0, 80, 20);
    let mut fresh = summary("fresh", true, true, 1, 1);
    fresh.sample_interval_ms = Some(100.0);
    fresh.age_ms = Some(95);
    // Connected but the interval EMA is not known yet: show the age, not a
    // fabricated interval or `—`.
    let mut warming = summary("warming", true, true, 0, 1);
    warming.age_ms = Some(1_500);
    let mut stale = summary("stale", false, false, 0, 0);
    stale.age_ms = Some(2_400);
    // Never measured: no age, must render `—` rather than `0`. Slot/CTX are
    // known so the sample is the only `—` on the row.
    let mut never = summary("never", false, true, 0, 2);
    never.context_used = Some(1);
    never.context_size = 2;
    never.generation_tps = Some(1.0);
    let rows = render_selector_rows(&[fresh, warming, stale, never], "fresh", area);

    let (_, fresh_detail) = selector_entry(&rows, "fresh");
    assert!(
        fresh_detail.contains("~100 ms avg"),
        "a connected server shows its sample interval: {rows:?}"
    );
    let (_, warming_detail) = selector_entry(&rows, "warming");
    assert!(
        warming_detail.contains("1.5 s ago"),
        "a connected server without an interval yet shows its age: {rows:?}"
    );
    let (_, stale_detail) = selector_entry(&rows, "stale");
    assert!(
        stale_detail.contains("2.4 s ago"),
        "a stale server shows its measurement age: {rows:?}"
    );
    let (_, never_detail) = selector_entry(&rows, "never");
    assert!(
        never_detail.contains("—"),
        "a never-measured server shows —: {rows:?}"
    );
}

#[test]
fn server_selector_marks_held_values_stale_and_shows_unknown_phase() {
    // CFA-88: a reconnecting server still lists its held CTX/tok/s, so its
    // freshness cell must say the values are stale; a connected server
    // without activity evidence reads UNKNOWN, never IDLE.
    let area = Rect::new(0, 0, 80, 20);
    let mut held = summary("held", false, true, 1, 1);
    held.reconnecting = true;
    held.phase = Some(ActivityPhase::Reconnecting);
    held.age_ms = Some(1_200);
    held.generation_tps = Some(42.0);
    let mut bare = summary("bare", true, false, 0, 0);
    bare.phase = Some(ActivityPhase::Unknown);
    let rows = render_selector_rows(&[held, bare], "held", area);

    let (held_header, held_detail) = selector_entry(&rows, "held");
    assert!(held_header.contains("reconnecting"), "{rows:?}");
    assert!(
        held_detail.contains("stale 1.2 s ago"),
        "held values must be marked stale: {rows:?}"
    );
    let (bare_header, _) = selector_entry(&rows, "bare");
    let cell = selector_cell_after_label(bare_header, "bare", PHASE_OFFSET, PHASE_WIDTH);
    assert_eq!(cell.trim(), "UNKNOWN", "{rows:?}");
}

#[test]
fn server_selector_keeps_phase_age_ctx_tps_and_backend_visible_at_80_columns() {
    // Required fields must all be readable on an 80-column terminal without
    // any value being sliced in half (a truncated CTX would look like a
    // different, real value).
    let area = Rect::new(0, 0, 80, 20);
    let mut server = summary("model", true, true, 1, 4);
    server.phase = Some(ActivityPhase::Generating);
    server.backend = crate::domain::ServerBackend::LlamaCpp;
    server.context_used = Some(32768);
    server.context_size = 131072;
    server.generation_tps = Some(20.5);
    server.sample_interval_ms = Some(100.0);
    server.age_ms = Some(95);
    let rows = render_selector_rows(&[server], "model", area);
    let (header, detail) = selector_entry(&rows, "model");

    assert!(
        detail.contains("CTX 32768/131072"),
        "the full CTX value must survive at 80 columns: {rows:?}"
    );
    assert!(
        detail.contains("20.5 tok/s"),
        "tok/s must be visible at 80 columns: {rows:?}"
    );
    assert!(
        detail.contains("~100 ms avg"),
        "the measurement age/interval must be visible: {rows:?}"
    );
    assert!(
        detail.contains("S 1/4"),
        "slot counts must stay visible: {rows:?}"
    );
    let phase = selector_cell_after_label(header, "model", PHASE_OFFSET, PHASE_WIDTH);
    assert_eq!(
        phase.trim(),
        "GENERATING",
        "phase missing at 80 columns: {rows:?}"
    );
    assert!(
        header.contains("llama.cpp"),
        "the backend label must be visible at 80 columns: {rows:?}"
    );
}

#[test]
fn server_selector_overflow_continuation_counts_missing_entries_exactly() {
    // 16 servers + 10 overflow endpoints at 80x16: inner height 10, the
    // pinned block is capped at half (5 rows = header + 3 entries + the
    // continuation marker), so exactly 7 endpoint rows are hidden.
    let servers: Vec<ServerSummary> = (0..16)
        .map(|i| summary(&format!("s{i:02}"), true, true, 1, 4))
        .collect();
    let overflow: Vec<crate::app::ServerSpec> = (0..10)
        .map(|i| crate::app::ServerSpec {
            key: format!("o{i}"),
            endpoint: format!("http://10.0.0.{}:8080", 10 + i),
            identity: None,
        })
        .collect();
    let area = Rect::new(0, 0, 80, 16);
    let text = render_selector(&servers, "s00", 0, &overflow, area);
    assert!(
        text.contains("NOT MONITORED"),
        "overflow header must stay visible:\n{text}"
    );
    assert!(
        text.contains("7 more not monitored"),
        "continuation must count the hidden endpoints exactly:\n{text}"
    );
}

#[test]
fn llm_phase_without_activity_evidence_is_unknown_not_idle() {
    // Connected, /metrics answered, but no /slots, no request gauge and no
    // server-reported phase: nothing says the server is idle (CFA-88).
    let stats = LlmStats {
        connected: true,
        metrics_available: true,
        ..LlmStats::default()
    };
    let (phase, color) = llm_phase(&stats);
    assert_eq!(phase, "UNKNOWN");
    assert_eq!(color, MUTED);
}

#[test]
fn llm_phase_marks_idle_only_with_evidence() {
    let connected = LlmStats {
        connected: true,
        metrics_available: true,
        ..LlmStats::default()
    };
    // A valid /slots array with no busy slot.
    let slots_idle = LlmStats {
        slots_available: true,
        slot_count: 2,
        ..connected.clone()
    };
    assert_eq!(llm_phase(&slots_idle), ("IDLE", MUTED));
    // An active-requests gauge reporting a real 0.
    let gauge_idle = LlmStats {
        active_requests: Some(0.0),
        ..connected.clone()
    };
    assert_eq!(llm_phase(&gauge_idle).0, "IDLE");
    // Only a deferred gauge of 0 is not evidence of no active request.
    let deferred_only = LlmStats {
        deferred_requests: Some(0.0),
        ..connected
    };
    assert_eq!(llm_phase(&deferred_only).0, "UNKNOWN");
}

#[test]
fn llm_phase_distinguishes_offline_and_reconnecting() {
    assert_eq!(llm_phase(&LlmStats::default()), ("OFFLINE", RED));
    let held = LlmStats {
        connected: true,
        reconnecting: true,
        generation_tps: Some(40.0),
        ..LlmStats::default()
    };
    assert_eq!(llm_phase(&held).0, "RECONNECTING");
}

#[test]
fn llm_phase_uses_a_reported_server_phase() {
    use crate::strata::StrataPhase;

    // Prefill is labeled PREFILL even though Strata reports no live prefill
    // rate; generating is GENERATING without needing a live rate.
    let reading = LlmStats {
        connected: true,
        strata_phase: Some(StrataPhase::Reading),
        ..LlmStats::default()
    };
    assert_eq!(llm_phase(&reading).0, "PREFILL");

    let generating = LlmStats {
        connected: true,
        strata_phase: Some(StrataPhase::Generating),
        ..LlmStats::default()
    };
    assert_eq!(llm_phase(&generating).0, "GENERATING");

    // A reported idle phase is evidence for IDLE, and a programmatic rate
    // still wins when there is no phase.
    let idle = LlmStats {
        connected: true,
        strata_phase: Some(StrataPhase::Idle),
        ..LlmStats::default()
    };
    assert_eq!(llm_phase(&idle).0, "IDLE");
    let rate_only = LlmStats {
        connected: true,
        generation_tps: Some(50.0),
        ..LlmStats::default()
    };
    assert_eq!(llm_phase(&rate_only).0, "GENERATING");
}

#[test]
fn spec_row_hints_when_enabled_but_counters_unavailable() {
    let llm = LlmStats {
        spec_enabled: true,
        ..LlmStats::default()
    };
    let row = llm_spec_row(&llm, 24, None);
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("no counters"), "row: {text:?}");
    assert!(text.contains("SPEC"));
}

#[test]
fn spec_row_shows_values_when_any_counter_reported() {
    let llm = LlmStats {
        spec_enabled: true,
        spec_drafts_total: Some(3.0),
        spec_draft_tokens: Some(12.0),
        spec_accepted_tokens: Some(9.0),
        ..LlmStats::default()
    };
    let row = llm_spec_row(&llm, 24, Some(75.0));
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("12 draft"), "row: {text:?}");
    assert!(text.contains("9 accepted · 75.0%"), "row: {text:?}");
    assert!(!text.contains("b10700"));
}

#[test]
fn spec_row_off_when_disabled() {
    let llm = LlmStats::default();
    let row = llm_spec_row(&llm, 24, None);
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("OFF"), "row: {text:?}");
    assert!(!text.contains("b10700"));
}

#[test]
fn llm_panel_ctx_row_names_the_selected_slot_on_multi_slot_servers() {
    let llm = LlmStats {
        connected: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(78800),
        metrics_available: true,
        slots_available: true,
        slot_count: 2,
        busy_slots: 1,
        context_slot_id: Some(1),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        text.contains("S1"),
        "multi-slot panel must tag the selected slot, got:\n{text}"
    );
    assert!(
        text.contains("78,800 / 115,200 tok"),
        "context pair must stay readable next to the tag, got:\n{text}"
    );
}

#[test]
fn llm_panel_single_slot_omits_the_redundant_slot_tag() {
    let llm = LlmStats {
        connected: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(28851),
        metrics_available: true,
        slots_available: true,
        slot_count: 1,
        context_slot_id: Some(0),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        !text.contains("S0"),
        "single-slot panel must not show a redundant S0, got:\n{text}"
    );
    assert!(text.contains("28,851 / 115,200 tok"));
}

#[test]
fn llm_panel_without_slot_id_never_prints_a_fake_slot() {
    let llm = LlmStats {
        connected: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(5000),
        metrics_available: true,
        slots_available: true,
        slot_count: 2,
        busy_slots: 1,
        context_slot_id: None,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        !text.contains("S0") && !text.contains("S1"),
        "unknown slot identity must not be guessed, got:\n{text}"
    );
    assert!(text.contains("5,000 / 115,200 tok"));
}

#[test]
fn narrow_llm_panel_drops_the_slot_tag_before_the_context_values() {
    let llm = LlmStats {
        connected: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(28851),
        metrics_available: true,
        slots_available: true,
        slot_count: 2,
        busy_slots: 1,
        context_slot_id: Some(0),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 40, 12);
    assert!(
        !text.contains("S0"),
        "below the bar-pin width the tag must be dropped, got:\n{text}"
    );
}

#[test]
fn llm_panel_shows_slots_but_marks_metrics_unavailable() {
    // Reachable server without /metrics: the slot/context state stays
    // visible, the metric rows are unavailable (absent), never fake zeros.
    let llm = LlmStats {
        connected: true,
        metrics_available: false,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(58745),
        slots_available: true,
        slot_count: 2,
        busy_slots: 1,
        context_slot_id: Some(0),
        slot_overview: overview_slots(),
        error: "/metrics disabled; start llama.cpp with --metrics".to_string(),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(text.contains("UNAVAILABLE"), "got:\n{text}");
    assert!(
        text.contains("58,745 / 115,200 tok"),
        "the CTX pair from /slots must stay visible, got:\n{text}"
    );
    assert!(
        text.contains("S0 58.7k*"),
        "the per-slot overview must stay visible, got:\n{text}"
    );
    assert!(
        !text.contains("tok/s"),
        "no metric rate may be fabricated without /metrics, got:\n{text}"
    );
    // The metric-derived rows are omitted entirely, so no total/avg/
    // request cell can show a fabricated value.
    assert!(
        !text.contains("TOTAL") && !text.contains("AVG") && !text.contains("REQUEST"),
        "no metric row may be fabricated without /metrics, got:\n{text}"
    );
}

#[test]
fn unknown_context_occupancy_renders_the_ctx_meter_unavailable() {
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: None,
        slots_available: true,
        slot_count: 1,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        !text.contains("0.0%"),
        "unknown occupancy must not render 0.0%:\n{text}"
    );
    assert!(text.contains("— / 115,200 tok"), "got:\n{text}");
}

#[test]
fn known_zero_context_occupancy_still_renders_zero_percent() {
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: 4096,
        context_used: Some(0),
        slots_available: true,
        slot_count: 1,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(text.contains("0.0%"), "a real zero is 0.0%:\n{text}");
    assert!(text.contains("0 / 4,096 tok"), "got:\n{text}");
}

#[test]
fn context_without_capacity_renders_the_meter_unavailable() {
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: 0,
        context_used: Some(5),
        slots_available: false,
        context_high_watermark: None,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        !text.contains("0.0%"),
        "no capacity means the percentage is unknown:\n{text}"
    );
}

#[test]
fn watermark_is_not_shown_as_current_context_occupancy() {
    // /slots is down but /metrics answered: the CTX row must render the
    // occupancy as unknown (`— / capacity`) instead of presenting the
    // historical high-water mark as the current context.
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        slots_available: false,
        model: "test-model".to_string(),
        context_size: 4096,
        context_used: None,
        context_high_watermark: Some(2048),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        text.contains("— / 4,096 tok"),
        "unknown occupancy must stay unknown, got:\n{text}"
    );
    assert!(
        !text.contains("2,048"),
        "the historical watermark must not appear as current CTX, got:\n{text}"
    );
    assert!(!text.contains("0.0%"), "unknown is not 0.0%:\n{text}");
}

#[test]
fn absent_metrics_render_as_unavailable_not_zero() {
    // Reachable server, but every /metrics-derived metric is absent: the
    // panel must show `—`, never a fabricated 0 / 0.0 / 0 tok/s.
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: 4096,
        context_used: Some(100),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 100, 14);
    assert!(!text.contains("0 tok"), "no fabricated total: {text}");
    assert!(!text.contains("0.0 tok/s"), "no fabricated rate: {text}");
    assert!(!text.contains("0.0 s"), "no fabricated time: {text}");
    assert!(
        !text.contains("0/0"),
        "no fabricated request counts: {text}"
    );
    assert!(text.contains('—'), "unavailable values must render —");
}

#[test]
fn incomplete_request_totals_render_unavailable_not_zero() {
    // A busy slot whose prompt total is incomplete (None) renders —, while
    // an independent real zero decode total still renders 0 tok.
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        busy_slots: 1,
        request_prompt_tokens: None,
        request_generated_tokens: Some(0),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 100, 14);
    assert!(text.contains("REQUEST"), "request row must render:\n{text}");
    assert!(
        text.contains("— tok"),
        "incomplete total must be —:\n{text}"
    );
    assert!(
        text.contains("0 tok"),
        "a real zero request count must render 0:\n{text}"
    );
}

#[test]
fn cache_row_never_shows_a_clamped_share_when_cached_exceeds_total() {
    // metrics_current.prom: cached 26,000 > prompt total 12,000. The old
    // clamped `100.0% of PP` was an invented ratio; the absolute counter
    // must stay visible and the ratio cell must be `—`.
    let text = render_llm_panel(&cache_llm(Some(26000.0), Some(12000.0)), 100, 14);
    let row = cache_row(&text, 100);
    assert!(row.contains("26,000 tok"), "absolute counter:\n{text}");
    assert!(!row.contains('%'), "no ratio may be shown:\n{row}");
    assert!(row.contains('—'), "ratio cell must be unavailable:\n{row}");
    assert!(!text.contains("100.0%"), "no clamped 100%:\n{text}");
    assert!(!text.contains("of PP"), "no share of PP:\n{text}");
}

#[test]
fn cache_row_keeps_a_real_zero_counter_without_a_ratio() {
    for total in [Some(12000.0), Some(0.0), None] {
        let text = render_llm_panel(&cache_llm(Some(0.0), total), 100, 14);
        let row = cache_row(&text, 100);
        assert!(row.contains("0 tok"), "real zero counter:\n{row}");
        assert!(!row.contains('%'), "no 0.0% share ({total:?}):\n{row}");
    }
}

#[test]
fn cache_row_with_zero_or_missing_total_shows_only_the_counter() {
    for total in [Some(0.0), None, Some(f64::NAN)] {
        let text = render_llm_panel(&cache_llm(Some(500.0), total), 100, 14);
        let row = cache_row(&text, 100);
        assert!(row.contains("500 tok"), "counter stays visible:\n{row}");
        assert!(!row.contains('%'), "no ratio ({total:?}):\n{row}");
    }
}

#[test]
fn cache_row_with_missing_or_invalid_counter_is_unavailable() {
    for cached in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-5.0)] {
        assert_eq!(valid_cache_counter(cached), None, "{cached:?}");
        let text = render_llm_panel(&cache_llm(cached, Some(12000.0)), 100, 14);
        let row = cache_row(&text, 100);
        assert!(
            row.contains('—'),
            "unavailable counter ({cached:?}):\n{row}"
        );
        assert!(
            !row.contains("tok"),
            "no fabricated count ({cached:?}):\n{row}"
        );
        assert!(!row.contains('%'), "no ratio ({cached:?}):\n{row}");
    }
    assert_eq!(valid_cache_counter(Some(0.0)), Some(0.0));
    assert_eq!(valid_cache_counter(Some(26000.0)), Some(26000.0));
}

#[test]
fn hero_line_shows_live_decode_prefill_and_context() {
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        slots_available: true,
        slot_count: 1,
        busy_slots: 1,
        generation_tps: Some(119.9),
        prompt_tps: Some(5.0),
        context_used: Some(10),
        context_size: 100,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 100, 14);
    assert!(text.contains("STATE"), "{text}");
    assert!(text.contains("GENERATING"), "{text}");
    assert!(text.contains("DECODE 119.9 tok/s"), "{text}");
    assert!(text.contains("PREFILL 5.0 tok/s"), "{text}");
    assert!(text.contains("CTX 10.0%"), "{text}");
}

#[test]
fn history_panel_includes_llm_decode_and_prefill_rates() {
    let mut state = UiState::default();
    let now = Instant::now();
    state.llm_history.decode.push_back(TimedSample {
        at: now,
        value: 12.0,
    });
    state.llm_history.prefill.push_back(TimedSample {
        at: now,
        value: 3.0,
    });
    let area = Rect::new(0, 0, 90, 12);
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_history(frame, area, &state))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(text.contains("DECODE"), "{text}");
    assert!(text.contains("12.0 tok/s"), "{text}");
    assert!(text.contains("PREFILL"), "{text}");
    assert!(text.contains("3.0 tok/s"), "{text}");
}

#[test]
fn held_reconnecting_sample_is_marked_stale_in_the_panel() {
    // During the offline grace the last good sample is still drawn; the
    // panel must say it is not fresh and must not highlight held rates as
    // live activity (CFA-88).
    let live = LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        slots_available: true,
        slot_count: 1,
        busy_slots: 1,
        generation_tps: Some(42.0),
        ..Default::default()
    };
    let fresh_text = render_llm_panel(&live, 100, 14);
    assert!(!fresh_text.contains("STALE"), "{fresh_text}");
    assert!(fresh_text.contains("GENERATING"), "{fresh_text}");

    let held = LlmStats {
        reconnecting: true,
        ..live
    };
    for (width, height) in [(54, 16), (72, 22), (100, 14)] {
        let text = render_llm_panel(&held, width, height);
        assert!(
            text.contains("STALE (held sample)"),
            "held sample must be marked stale at {width}x{height}:\n{text}"
        );
        assert!(text.contains("RECONNECTING"), "{text}");
        assert!(!text.contains("GENERATING"), "{text}");
    }
}

#[test]
fn metrics_unavailable_panel_with_slots_keeps_context_unknown() {
    let llm = LlmStats {
        connected: true,
        metrics_available: false,
        model: "test-model".to_string(),
        context_size: 8192,
        context_used: None,
        slots_available: true,
        slot_count: 1,
        error: "/metrics disabled; start llama.cpp with --metrics".to_string(),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(text.contains("METRICS"), "got:\n{text}");
    assert!(!text.contains("0.0%"), "got:\n{text}");
}

#[test]
fn metrics_unavailable_panel_never_claims_the_server_is_offline() {
    // A transient /metrics transport failure while /slots answers must not
    // render the contradictory pair LINK ONLINE + "LLAMA SERVER OFFLINE".
    let llm = LlmStats {
        connected: true,
        metrics_available: false,
        model: "test-model".to_string(),
        error: "cannot reach llama.cpp: connection refused".to_string(),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(text.contains("METRICS UNREACHABLE"), "got:\n{text}");
    assert!(
        !text.contains("SERVER OFFLINE"),
        "a reachable server must not be reported offline, got:\n{text}"
    );
}

#[test]
fn slot_overview_entries_format_ids_usage_and_selected_marker() {
    let entries = slot_overview_entries(&overview_slots(), Some(0));
    assert_eq!(entries[0].0, "S0 58.7k*");
    assert_eq!(entries[0].1, WHITE, "busy slot is highlighted");
    assert_eq!(entries[1].0, "S1 34.2k");
    assert_eq!(entries[1].1, MUTED, "idle slot is dimmed");

    // The marker follows context_slot_id, never a fixed position.
    let entries = slot_overview_entries(&overview_slots(), Some(1));
    assert_eq!(entries[0].0, "S0 58.7k");
    assert_eq!(entries[1].0, "S1 34.2k*");

    // An unknown selection marks nothing.
    let entries = slot_overview_entries(&overview_slots(), None);
    assert!(entries.iter().all(|(text, _)| !text.ends_with('*')));
}

#[test]
fn slot_overview_entries_show_per_slot_capacity_when_capacities_differ() {
    let mut slots = vec![
        LlmSlotInfo {
            id: 0,
            busy: true,
            context_used: Some(58745),
            context_size: Some(115200),
        },
        LlmSlotInfo {
            id: 3,
            busy: false,
            context_used: Some(34200),
            context_size: Some(70144),
        },
    ];
    let entries = slot_overview_entries(&slots, Some(0));
    assert_eq!(entries[0].0, "S0 58.7/115.2k*");
    assert_eq!(entries[1].0, "S3 34.2/70.1k");

    // A missing capacity is shown as unknown, not hidden or faked.
    slots[1].context_size = None;
    let entries = slot_overview_entries(&slots, Some(0));
    assert_eq!(entries[1].0, "S3 34.2/—");
}

#[test]
fn slot_overview_entries_never_print_a_fake_zero_for_missing_usage() {
    let slots = vec![
        LlmSlotInfo {
            id: 0,
            busy: false,
            context_used: None,
            context_size: Some(115200),
        },
        LlmSlotInfo {
            id: 7,
            busy: false,
            context_used: Some(999),
            context_size: Some(115200),
        },
    ];
    let entries = slot_overview_entries(&slots, Some(0));
    assert_eq!(entries[0].0, "S0 —*");
    assert_eq!(entries[1].0, "S7 999");
}

#[test]
fn slot_overview_line_truncates_many_slots_with_visible_remainder() {
    let slots = (0..5)
        .map(|id| LlmSlotInfo {
            id,
            busy: id % 2 == 0,
            context_used: Some(10000 + id * 1000),
            context_size: Some(115200),
        })
        .collect::<Vec<_>>();
    // Entry widths: S0 10.0k* (9) + S1..S4 (8 each); 12 (label) + 3
    // (gap) + 4 (suffix). Width 55 must drop entries from the end and
    // name the remainder: 3 entries + `+2` = 47 cells fit, 4 entries
    // + `+1` = 58 do not.
    let line = llm_slot_overview_line(&slots, Some(0), 55).expect("row must fit");
    let text: String = line.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("S0 10.0k*"), "got: {text}");
    assert!(
        text.contains("+2"),
        "hidden entries must be named, got: {text}"
    );
    assert!(!text.contains("S4 "), "got: {text}");
    // Deterministic: the same input always yields the same row.
    let again = llm_slot_overview_line(&slots, Some(0), 55).unwrap();
    let again_text: String = again.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(text, again_text);
}

#[test]
fn slot_overview_line_fits_exactly_at_the_width_boundary() {
    // 4 entries (8 cells each) + `  +1` (4) = 12 + 32 + 9 + 4 = exactly
    // 57 cells: the remainder marker must survive intact at the boundary
    // and the row must fall back one entry below it.
    let slots = (0..5)
        .map(|id| LlmSlotInfo {
            id,
            busy: false,
            context_used: Some(10000 + id * 1000),
            context_size: Some(115200),
        })
        .collect::<Vec<_>>();
    let line = llm_slot_overview_line(&slots, None, 57).expect("exact fit must pass");
    let text: String = line.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("S3 13.0k"), "got: {text}");
    assert!(text.ends_with("  +1"), "got: {text}");
    assert!(!text.contains("S4 "), "got: {text}");
    // One cell less: the suffixed 4-entry row (57) no longer fits, so
    // the row falls back to 3 entries + `  +2` = exactly 46 cells.
    let line = llm_slot_overview_line(&slots, None, 56).unwrap();
    let text: String = line.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("S2 12.0k"), "got: {text}");
    assert!(text.ends_with("  +2"), "got: {text}");
    assert!(!text.contains("S3 "), "got: {text}");
    // And 3 entries + `  +2` = 46 cells still fits at width 46.
    let line = llm_slot_overview_line(&slots, None, 46).unwrap();
    let text: String = line.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.ends_with("  +2"), "got: {text}");
    assert!(!text.contains("S3 "), "got: {text}");
}

#[test]
fn slot_overview_line_omitted_when_nothing_fits() {
    let line = llm_slot_overview_line(&overview_slots(), Some(0), 20);
    assert!(
        line.is_none(),
        "a 20-cell pane cannot hold label + one entry"
    );
    assert!(llm_slot_overview_line(&[], Some(0), 120).is_none());
}

#[test]
fn llm_panel_multi_slot_shows_per_slot_overview_below_the_ctx_row() {
    let llm = overview_llm(overview_slots(), Some(0));
    let text = render_llm_panel(&llm, 90, 12);
    assert!(text.contains("S0 58.7k*"), "got:\n{text}");
    assert!(text.contains("S1 34.2k"), "got:\n{text}");
    // The main CTX row stays authoritative and unchanged.
    assert!(
        text.contains("58,745 / 115,200 tok"),
        "main CTX pair must stay readable, got:\n{text}"
    );
}

#[test]
fn llm_panel_single_slot_never_shows_the_overview() {
    let mut llm = overview_llm(overview_slots(), Some(0));
    llm.slot_count = 1;
    llm.slot_overview.truncate(1);
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        !text.contains("S0"),
        "single-slot panels must not gain a redundant overview, got:\n{text}"
    );
}

#[test]
fn narrow_llm_panel_omits_the_slot_overview_safely() {
    let llm = overview_llm(overview_slots(), Some(0));
    // Width 20 fits the CTX row but not even one overview entry.
    let text = render_llm_panel(&llm, 20, 12);
    assert!(
        !text.contains("58.7k"),
        "the overview must be dropped before corrupting the panel, got:\n{text}"
    );
}

fn render_selector(
    servers: &[ServerSummary],
    selected: &str,
    highlight: usize,
    overflow: &[crate::app::ServerSpec],
    area: Rect,
) -> String {
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_server_selector(frame, area, servers, selected, highlight, overflow))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn render_server_selector_text(servers: &[ServerSummary], selected: &str, area: Rect) -> String {
    render_selector(servers, selected, 0, &[], area)
}

/// Render the selector and return it row by row, so a test can assert what
/// appears on one server's line (per-server, not merely "somewhere").
fn render_selector_rows(servers: &[ServerSummary], selected: &str, area: Rect) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_server_selector(frame, area, servers, selected, 0, &[]))
        .unwrap();
    let width = area.width as usize;
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
        .collect()
}

/// The two rendered lines (`header`, `detail`) of the selector entry whose
/// header carries `label` at the marker/label column.
fn selector_entry<'a>(rows: &'a [String], label: &str) -> (&'a str, &'a str) {
    let pos = rows
        .iter()
        .position(|row| {
            row.find(label).is_some_and(|byte| {
                let start = row[..byte].chars().count();
                let chars: Vec<char> = row.chars().collect();
                start >= 3
                    && matches!(chars[start - 3], '>' | ' ')
                    && matches!(chars[start - 2], '*' | ' ')
                    && chars[start - 1] == ' '
            })
        })
        .unwrap_or_else(|| panic!("no selector entry for {label}: {rows:?}"));
    (
        rows[pos].as_str(),
        rows.get(pos + 1).map(String::as_str).unwrap_or(""),
    )
}

/// The `width` characters of `header` starting `offset` chars after `label`.
/// Used to assert a specific column rather than "somewhere on the row".
fn selector_cell_after_label(header: &str, label: &str, offset: usize, width: usize) -> String {
    let byte = header
        .find(label)
        .unwrap_or_else(|| panic!("label {label} not in {header:?}"));
    let start = header[..byte].chars().count() + offset;
    header.chars().skip(start).take(width).collect()
}

/// Char offset from the label to the activity-phase column: the label cell
/// (16 + 1) plus the state cell (12 + 1).
const PHASE_OFFSET: usize = 17 + 13;

const PHASE_WIDTH: usize = 12;

/// The rendered `CACHE` detail row of an LLM panel, or an empty string.
fn cache_row(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|row| row.iter().collect::<String>())
        .find(|row| row.contains(" CACHE      "))
        .unwrap_or_default()
}

fn cache_llm(cached: Option<f64>, total: Option<f64>) -> LlmStats {
    LlmStats {
        connected: true,
        metrics_available: true,
        model: "test-model".to_string(),
        context_size: 4096,
        context_used: Some(100),
        prompt_cached_total: cached,
        prompt_total: total,
        ..Default::default()
    }
}

fn overview_slots() -> Vec<LlmSlotInfo> {
    vec![
        LlmSlotInfo {
            id: 0,
            busy: true,
            context_used: Some(58745),
            context_size: Some(115200),
        },
        LlmSlotInfo {
            id: 1,
            busy: false,
            context_used: Some(34200),
            context_size: Some(115200),
        },
    ]
}

fn overview_llm(overview: Vec<LlmSlotInfo>, selected: Option<u64>) -> LlmStats {
    LlmStats {
        connected: true,
        model: "test-model".to_string(),
        context_size: 115200,
        context_used: Some(58745),
        metrics_available: true,
        slots_available: true,
        slot_count: overview.len() as u64,
        busy_slots: overview.iter().filter(|slot| slot.busy).count() as u64,
        context_slot_id: selected,
        slot_overview: overview,
        ..Default::default()
    }
}
