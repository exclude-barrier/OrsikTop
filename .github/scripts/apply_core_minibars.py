from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()

old = '''    let llm_height = if llm.connected { 12 } else { 5 };\n    let history_required = 3 + 7 + llm_height + 5 + 3;\n'''
new = '''    let llm_height = if llm.connected { 12 } else { 5 };\n    let system_height = system_panel_height(system);\n    let middle_height = llm_height.max(system_height);\n    let history_required = 3 + 7 + middle_height + 5 + 3;\n'''
if old not in text:
    raise SystemExit('draw height marker not found')
text = text.replace(old, new, 1)
text = text.replace('Constraint::Length(llm_height),', 'Constraint::Length(middle_height),', 2)
text = text.replace('if inner.width < 35 || inner.height < 10 {', 'if inner.width < 35 || inner.height < 15 {', 1)

old = '''        lines.extend(physical_core_rows(\n            &system.per_cpu_usage,\n            &system.cpu_topology.physical_core_groups,\n            inner.width,\n            busiest,\n            4,\n        ));\n'''
new = '''        lines.extend(physical_core_minibar_rows(\n            &system.per_cpu_usage,\n            &system.cpu_topology.physical_core_groups,\n            inner.width,\n        ));\n'''
if old not in text:
    raise SystemExit('hybrid renderer marker not found')
text = text.replace(old, new, 1)

marker = 'fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {'
if marker not in text:
    raise SystemExit('draw_system marker not found')
helper = '''fn system_panel_height(system: &SystemStats) -> u16 {\n    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {\n        let p = system\n            .cpu_topology\n            .physical_core_groups\n            .iter()\n            .filter(|core| core.kind == CpuCoreKind::Performance)\n            .count();\n        let e = system\n            .cpu_topology\n            .physical_core_groups\n            .iter()\n            .filter(|core| core.kind == CpuCoreKind::Efficiency)\n            .count();\n        let rows = p.max(e).max(1);\n        return (rows as u16 + 9).max(12);\n    }\n    12\n}\n\n'''
text = text.replace(marker, helper + marker, 1)

start = text.index('fn physical_core_rows(')
end = text.index('fn core_heatmap_rows(', start)
new_helpers = r'''fn physical_core_minibar_rows(
    usages: &[f64],
    cores: &[CpuPhysicalCore],
    width: u16,
) -> Vec<Line<'static>> {
    let performance = cores
        .iter()
        .filter(|core| core.kind == CpuCoreKind::Performance)
        .collect::<Vec<_>>();
    let efficiency = cores
        .iter()
        .filter(|core| core.kind == CpuCoreKind::Efficiency)
        .collect::<Vec<_>>();

    if performance.is_empty() || efficiency.is_empty() || width < 30 {
        return Vec::new();
    }

    let rows = performance.len().max(efficiency.len());
    let mut lines = Vec::with_capacity(rows + 1);
    lines.push(Line::from(vec![
        Span::styled(
            " P-CORES",
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::raw("            "),
        Span::styled(
            "E-CORES",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
    ]));

    for row in 0..rows {
        let mut spans = Vec::new();

        if let Some(core) = performance.get(row) {
            let usage = physical_core_average(core, usages);
            let color = physical_core_color(usage, ORK_GREEN);
            spans.push(Span::styled(
                format!(" P{row}  "),
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ));
            spans.extend(mini_core_bar_spans(usage, 4, color));
            spans.push(Span::styled(
                format!(" {:>3.0}%", clamp_percent(usage)),
                Style::default().fg(color),
            ));
        } else {
            spans.push(Span::raw("              "));
        }

        let current_width = spans.iter().map(Span::width).sum::<usize>();
        let e_column = 20usize;
        if current_width < e_column {
            spans.push(Span::raw(" ".repeat(e_column - current_width)));
        } else {
            spans.push(Span::raw("  "));
        }

        if let Some(core) = efficiency.get(row) {
            let usage = physical_core_average(core, usages);
            let color = physical_core_color(usage, CYAN);
            spans.push(Span::styled(
                format!("E{row}  "),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ));
            spans.extend(mini_core_bar_spans(usage, 3, color));
            spans.push(Span::styled(
                format!(" {:>3.0}%", clamp_percent(usage)),
                Style::default().fg(color),
            ));
        }

        lines.push(Line::from(spans));
    }

    lines
}

fn physical_core_average(core: &CpuPhysicalCore, usages: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for &cpu in &core.logical_cpus {
        if let Some(usage) = usages.get(cpu).copied() {
            sum += usage;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        clamp_percent(sum / count as f64)
    }
}

fn physical_core_color(usage: f64, base: Color) -> Color {
    if clamp_percent(usage) >= 90.0 {
        ORANGE
    } else {
        base
    }
}

fn mini_core_bar_spans(percent: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let percent = clamp_percent(percent);
    let scaled = percent / 100.0 * width as f64;
    let (full, half) = if percent < 5.0 {
        (0usize, false)
    } else if scaled < 0.5 {
        (0usize, true)
    } else {
        (scaled.ceil().min(width as f64) as usize, false)
    };

    let mut spans = Vec::with_capacity(width);
    for cell in 0..width {
        if cell < full {
            spans.push(Span::styled("⣿", Style::default().fg(color)));
        } else if cell == full && half {
            spans.push(Span::styled("⣇", Style::default().fg(color)));
        } else {
            spans.push(Span::styled("⣀", Style::default().fg(BAR_EMPTY)));
        }
    }
    spans
}

'''
text = text[:start] + new_helpers + text[end:]

test_marker = '''    #[test]\n    fn core_usage_glyph_uses_heatmap_levels() {'''
if test_marker not in text:
    raise SystemExit('test marker not found')
new_tests = '''    #[test]\n    fn physical_core_average_combines_smt_threads() {\n        let core = CpuPhysicalCore {\n            kind: CpuCoreKind::Performance,\n            logical_cpus: vec![0, 1],\n        };\n        assert_eq!(physical_core_average(&core, &[80.0, 20.0]), 50.0);\n    }\n\n    #[test]\n    fn physical_core_color_only_warns_near_saturation() {\n        assert_eq!(physical_core_color(89.9, ORK_GREEN), ORK_GREEN);\n        assert_eq!(physical_core_color(90.0, ORK_GREEN), ORANGE);\n        assert_eq!(physical_core_color(95.0, CYAN), ORANGE);\n    }\n\n'''
text = text.replace(test_marker, new_tests + test_marker, 1)

path.write_text(text)
