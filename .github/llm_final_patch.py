from pathlib import Path

p = Path('src/ui.rs')
s = p.read_text()

s = s.replace('''    let cache = llm
        .prompt_cached_total
        .map(grouped_f64)
        .unwrap_or_else(|| "—".to_string());

    let metric_width = ((inner.width as usize).saturating_sub(10) / 2).clamp(16, 30);
    let mut state = vec![
        label_span(" STATE    "),
''', '''    let cache_available = llm.prompt_cached_total.is_some();
    let cache = llm
        .prompt_cached_total
        .map(grouped_f64)
        .unwrap_or_else(|| "—".to_string());
    let cache_color = if cache_available { CYAN } else { MUTED };

    let pp_active = llm.prompt_tps > 0.05;
    let tg_active = llm.generation_tps > 0.05;
    let pp_header_color = if pp_active { CYAN } else { MUTED };
    let tg_header_color = if tg_active { ORK_GREEN } else { MUTED };
    let pp_live_color = if pp_active { CYAN } else { MUTED };
    let tg_live_color = if tg_active { ORK_GREEN } else { MUTED };
    let request_color = if llm.busy_slots > 0 { WHITE } else { MUTED };

    let metric_width = ((inner.width as usize).saturating_sub(12) / 2).clamp(16, 30);
    let mut state = vec![
        label_span(" STATE      "),
''')

s = s.replace('''        label_span("ACC "),
        value_span(&acc, acc_color),
    ];
    if inner.width < 66 {
        state = vec![
            label_span(" STATE    "),
            value_span(phase, phase_color),
            Span::raw("  "),
            label_span("SLOT "),
            value_span(&slots, CYAN),
            Span::raw("  "),
            value_span(&mtp, mtp_color),
        ];
    }

    let mut total_line = vec![
        label_span(" TOTAL    "),
        llm_metric_cell(&total_pp, metric_width, WHITE, false),
        llm_metric_cell(&total_tg, metric_width, WHITE, false),
    ];
    if inner.width >= 78 {
        total_line.push(label_span("CACHE "));
        total_line.push(value_span(&cache, CYAN));
    }
''', '''        label_span("ACC "),
        value_span(&acc, acc_color),
    ];
    if inner.width >= 82 {
        state.push(llm_sep());
        state.push(label_span("CACHE "));
        state.push(value_span(&cache, cache_color));
    }
    if inner.width < 66 {
        state = vec![
            label_span(" STATE      "),
            value_span(phase, phase_color),
            Span::raw("  "),
            label_span("SLOT "),
            value_span(&slots, CYAN),
            Span::raw("  "),
            value_span(&mtp, mtp_color),
        ];
    }

    let total_line = vec![
        label_span(" TOTAL      "),
        llm_metric_cell(&total_pp, metric_width, WHITE, false),
        llm_metric_cell(&total_tg, metric_width, WHITE, false),
    ];
''')

s = s.replace('''            label_span(" MODEL    "),
''', '''            label_span(" MODEL      "),
''', 1)

s = s.replace('''            label_span("          "),
            llm_metric_cell("PREFILL / PP", metric_width, CYAN, true),
            llm_metric_cell("DECODE / TG", metric_width, ORK_GREEN, true),
''', '''            label_span("            "),
            llm_metric_cell("PREFILL / PP", metric_width, pp_header_color, pp_active),
            llm_metric_cell("DECODE / TG", metric_width, tg_header_color, tg_active),
''')

s = s.replace('''            label_span(" LIVE     "),
            llm_metric_cell(&live_pp, metric_width, CYAN, llm.prompt_tps > 0.05),
            llm_metric_cell(&live_tg, metric_width, ORK_GREEN, llm.generation_tps > 0.05),
''', '''            label_span(" LIVE       "),
            llm_metric_cell(&live_pp, metric_width, pp_live_color, pp_active),
            llm_metric_cell(&live_tg, metric_width, tg_live_color, tg_active),
''')

s = s.replace('''            label_span(" SERVER   "),
''', '''            label_span(" SERVER AVG "),
''')

s = s.replace('''            label_span(" REQUEST  "),
            llm_metric_cell(&request_pp, metric_width, WHITE, false),
            llm_metric_cell(&request_tg, metric_width, WHITE, false),
''', '''            label_span(" REQUEST    "),
            llm_metric_cell(&request_pp, metric_width, request_color, false),
            llm_metric_cell(&request_tg, metric_width, request_color, false),
''')

# Strengthen regression coverage around the small formatting helpers used by the panel.
anchor = '''    fn token_counts_are_grouped_for_readability() {
        assert_eq!(grouped_u64(999), "999");
        assert_eq!(grouped_u64(1_000), "1,000");
        assert_eq!(grouped_u64(196_608), "196,608");
        assert_eq!(grouped_f64(447_924.0), "447,924");
    }
'''
replacement = anchor + '''\n    #[test]\n    fn llm_phase_marks_idle_without_activity() {\n        let stats = LlmStats::default();\n        let (phase, color) = llm_phase(&stats);\n        assert_eq!(phase, "IDLE");\n        assert_eq!(color, MUTED);\n    }\n'''
if anchor not in s:
    raise SystemExit('token formatting test anchor not found')
s = s.replace(anchor, replacement)

p.write_text(s)
