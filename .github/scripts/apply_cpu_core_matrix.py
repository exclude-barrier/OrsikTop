from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()

text = text.replace(
    'const ORK_GREEN: Color = Color::Rgb(105, 210, 70);\n',
    'const ORK_GREEN: Color = Color::Rgb(105, 210, 70);\nconst BRIGHT_GREEN: Color = Color::Rgb(150, 235, 95);\n',
    1,
)
text = text.replace(
    '    let llm_height = if llm.connected { 10 } else { 5 };',
    '    let llm_height = if llm.connected { 12 } else { 5 };',
    1,
)
text = text.replace(
    '    if inner.width < 34 || inner.height < 8 {',
    '    if inner.width < 35 || inner.height < 10 {',
    1,
)

start = text.index('    let max_per_row = inner.width.saturating_sub(11).max(1) as usize;')
end = text.index('fn core_usage_glyph(usage: f64) -> char {', start)
replacement = '''    let lines = vec![
        meter_line(
            "CPU",
            system.cpu_usage,
            bar_width,
            ORK_GREEN,
            format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
            vec![],
        ),
        Line::from(vec![
            label_span("      "),
            value_span(&format!("{frequency} GHz"), CYAN),
            label_span("   "),
            value_span(&temperature, temperature_tint),
            label_span("   IOW "),
            value_span(
                &io_wait,
                if system.io_wait_pct.unwrap_or(0.0) >= 10.0 {
                    YELLOW
                } else {
                    MUTED
                },
            ),
        ]),
        core_matrix_line(&system.per_cpu_usage, 0),
        core_matrix_line(&system.per_cpu_usage, 1),
        core_matrix_line(&system.per_cpu_usage, 2),
        core_matrix_line(&system.per_cpu_usage, 3),
        Line::from(vec![
            label_span(" LOAD "),
            value_span(
                &format!(
                    "{:.2} / {:.2} / {:.2}",
                    system.load_one, system.load_five, system.load_fifteen
                ),
                WHITE,
            ),
        ]),
        meter_line(
            "RAM",
            ram_pct,
            bar_width,
            CYAN,
            format!("{:>4.1}%", ram_pct),
            vec![],
        ),
        Line::from(vec![
            label_span("      "),
            value_span(
                &format!(
                    "{:.1} / {:.1} GiB",
                    bytes_to_gib(system.memory_used_bytes),
                    bytes_to_gib(system.memory_total_bytes)
                ),
                CYAN,
            ),
        ]),
        Line::from(vec![
            label_span(" SWAP "),
            value_span(
                &format!(
                    "{:.1} / {:.1} GiB",
                    bytes_to_gib(system.swap_used_bytes),
                    bytes_to_gib(system.swap_total_bytes)
                ),
                if system.swap_used_bytes > 0 {
                    YELLOW
                } else {
                    MUTED
                },
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

fn core_matrix_line(usages: &[f64], row: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut rendered = false;

    for column in 0..6 {
        let index = row + column * 4;
        let Some(usage) = usages.get(index).copied() else {
            continue;
        };

        if rendered {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{index:02} "),
            Style::default().fg(MUTED),
        ));
        spans.push(Span::styled(
            core_usage_glyph(usage).to_string(),
            Style::default()
                .fg(core_usage_color(usage))
                .add_modifier(Modifier::BOLD),
        ));
        rendered = true;
    }

    if !rendered {
        spans.push(value_span("CORES —", MUTED));
    }
    Line::from(spans)
}

fn core_usage_color(usage: f64) -> Color {
    match clamp_percent(usage) {
        value if value >= 90.0 => ORANGE,
        value if value >= 75.0 => YELLOW,
        value if value >= 50.0 => BRIGHT_GREEN,
        value if value >= 25.0 => ORK_GREEN,
        _ => DIM_GREEN,
    }
}

'''
text = text[:start] + replacement + text[end:]

old_test = '''    #[test]\n    fn core_usage_glyph_scales_with_utilization() {\n        assert_eq!(core_usage_glyph(0.0), '▁');\n        assert_eq!(core_usage_glyph(12.5), '▂');\n        assert_eq!(core_usage_glyph(50.0), '▅');\n        assert_eq!(core_usage_glyph(87.5), '█');\n        assert_eq!(core_usage_glyph(100.0), '█');\n    }\n'''
new_test = old_test + '''\n    #[test]\n    fn core_usage_color_uses_non_red_load_scale() {\n        assert_eq!(core_usage_color(0.0), DIM_GREEN);\n        assert_eq!(core_usage_color(25.0), ORK_GREEN);\n        assert_eq!(core_usage_color(50.0), BRIGHT_GREEN);\n        assert_eq!(core_usage_color(75.0), YELLOW);\n        assert_eq!(core_usage_color(90.0), ORANGE);\n        assert_eq!(core_usage_color(100.0), ORANGE);\n    }\n'''
if old_test not in text:
    raise SystemExit('core glyph test marker not found')
text = text.replace(old_test, new_test, 1)

path.write_text(text)
