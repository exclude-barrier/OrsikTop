from pathlib import Path

p = Path('src/ui.rs')
s = p.read_text()

s = s.replace('let llm_height = if llm.connected { 9 } else { 5 };', 'let llm_height = if llm.connected { 10 } else { 5 };')

old_vram = '''        "VRAM" => &[
            (0.0, DARK_CYAN),
            (80.0, CYAN),
            (95.0, YELLOW),
            (98.0, ORANGE),
            (100.0, RED),
        ],'''
new_vram = '''        "VRAM" => &[
            (0.0, DARK_CYAN),
            (90.0, CYAN),
            (95.0, YELLOW),
            (98.0, ORANGE),
            (100.0, RED),
        ],'''
if old_vram not in s:
    raise SystemExit('VRAM gradient block not found')
s = s.replace(old_vram, new_vram)

start = s.index('fn draw_llm(frame: &mut Frame, area: Rect, llm: &LlmStats) {')
end = s.index('\nfn llm_phase(llm: &LlmStats)', start)
new_draw = r'''fn draw_llm(frame: &mut Frame, area: Rect, llm: &LlmStats) {
    let block = Block::default()
        .title(" LLM INFERENCE ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    if !llm.connected {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    label_span(" STATUS   "),
                    value_span("METRICS OFF", YELLOW),
                ]),
                Line::from(vec![
                    label_span(" LLAMA    "),
                    Span::styled("restart server with --metrics", Style::default().fg(MUTED)),
                ]),
            ]),
            inner,
        );
        return;
    }

    let context_used = if llm.slots_available {
        llm.context_used
    } else {
        llm.context_high_watermark
    };
    let context_pct = if llm.context_size > 0 {
        context_used as f64 / llm.context_size as f64 * 100.0
    } else {
        0.0
    };
    let context_bar = inner.width.saturating_sub(46).max(8) as usize;
    let slots = if llm.slots_available {
        format!("{}/{}", llm.busy_slots, llm.slot_count)
    } else if llm.props_slot_count > 0 {
        format!("—/{}", llm.props_slot_count)
    } else {
        "—".to_string()
    };
    let (mtp, mtp_color) = if llm.spec_is_mtp {
        (
            llm.spec_n_max
                .filter(|value| *value > 0)
                .map(|value| format!("MTP{value}"))
                .unwrap_or_else(|| "MTP".to_string()),
            CYAN,
        )
    } else if llm.spec_enabled {
        ("SPEC".to_string(), CYAN)
    } else {
        ("MTP OFF".to_string(), MUTED)
    };
    let (acc, acc_color) = match llm.spec_acceptance_pct {
        Some(value) => (format!("{value:.0}%"), ORK_GREEN),
        None => ("—".to_string(), MUTED),
    };
    let (phase, phase_color) = llm_phase(llm);

    let live_pp = if llm.prompt_tps > 0.05 {
        format!("{:.1} tok/s", llm.prompt_tps)
    } else {
        "— tok/s".to_string()
    };
    let live_tg = if llm.generation_tps > 0.05 {
        format!("{:.1} tok/s", llm.generation_tps)
    } else {
        "— tok/s".to_string()
    };
    let request_pp = if llm.busy_slots > 0 {
        format!("{} tok", grouped_u64(llm.request_prompt_tokens))
    } else {
        "— tok".to_string()
    };
    let request_tg = if llm.busy_slots > 0 {
        format!("{} tok", grouped_u64(llm.request_generated_tokens))
    } else {
        "— tok".to_string()
    };
    let total_pp = format!("{} tok", grouped_f64(llm.prompt_total));
    let total_tg = format!("{} tok", grouped_f64(llm.generated_total));
    let cache = llm
        .prompt_cached_total
        .map(grouped_f64)
        .unwrap_or_else(|| "—".to_string());

    let metric_width = ((inner.width as usize).saturating_sub(10) / 2).clamp(16, 30);
    let mut state = vec![
        label_span(" STATE    "),
        value_span(phase, phase_color),
        llm_sep(),
        label_span("SLOT "),
        value_span(&slots, CYAN),
        llm_sep(),
        label_span("QUEUE "),
        value_span(&format!("{:.0}", llm.deferred_requests), WHITE),
        llm_sep(),
        value_span(&mtp, mtp_color),
        llm_sep(),
        label_span("ACC "),
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

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL    "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(state),
        Line::from(vec![
            label_span("          "),
            llm_metric_cell("PREFILL / PP", metric_width, CYAN, true),
            llm_metric_cell("DECODE / TG", metric_width, ORK_GREEN, true),
        ]),
        Line::from(vec![
            label_span(" LIVE     "),
            llm_metric_cell(&live_pp, metric_width, CYAN, llm.prompt_tps > 0.05),
            llm_metric_cell(
                &live_tg,
                metric_width,
                ORK_GREEN,
                llm.generation_tps > 0.05,
            ),
        ]),
        Line::from(vec![
            label_span(" SERVER   "),
            llm_metric_cell(
                &format!("{:.1} tok/s", llm.prompt_avg_tps),
                metric_width,
                MUTED,
                false,
            ),
            llm_metric_cell(
                &format!("{:.1} tok/s", llm.generation_avg_tps),
                metric_width,
                MUTED,
                false,
            ),
        ]),
        Line::from(vec![
            label_span(" REQUEST  "),
            llm_metric_cell(&request_pp, metric_width, WHITE, false),
            llm_metric_cell(&request_tg, metric_width, WHITE, false),
        ]),
        Line::from(total_line),
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            vec![Span::styled(
                if llm.context_size > 0 {
                    format!(" {} / {} tok", grouped_u64(context_used), grouped_u64(llm.context_size))
                } else {
                    " waiting for context".to_string()
                },
                Style::default().fg(MUTED),
            )],
        ),
    ];

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn llm_metric_cell(text: &str, width: usize, color: Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(format!("{text:<width$}"), style)
}

fn llm_sep() -> Span<'static> {
    Span::styled("  │  ", Style::default().fg(INNER_GREEN))
}

fn grouped_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn grouped_f64(value: f64) -> String {
    if value.is_finite() && value >= 0.0 {
        grouped_u64(value.round() as u64)
    } else {
        "—".to_string()
    }
}
'''
s = s[:start] + new_draw + s[end:]

anchor = '''    #[test]
    fn bar_gradients_use_expected_endpoints() {
        assert_eq!(bar_gradient_color("GPU", 100.0, WHITE), ORK_GREEN);
        assert_eq!(bar_gradient_color("VRAM", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("PWR", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("CTX", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("OTHER", 50.0, CYAN), CYAN);
    }
'''
replacement = '''    #[test]
    fn bar_gradients_use_expected_endpoints() {
        assert_eq!(bar_gradient_color("GPU", 100.0, WHITE), ORK_GREEN);
        assert_eq!(bar_gradient_color("VRAM", 90.0, WHITE), CYAN);
        assert_eq!(bar_gradient_color("VRAM", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("PWR", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("CTX", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("OTHER", 50.0, CYAN), CYAN);
    }

    #[test]
    fn token_counts_are_grouped_for_readability() {
        assert_eq!(grouped_u64(999), "999");
        assert_eq!(grouped_u64(1_000), "1,000");
        assert_eq!(grouped_u64(196_608), "196,608");
        assert_eq!(grouped_f64(447_924.0), "447,924");
    }
'''
if anchor not in s:
    raise SystemExit('gradient test anchor not found')
s = s.replace(anchor, replacement)

p.write_text(s)
