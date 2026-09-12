from pathlib import Path

# main.rs
main = Path('src/main.rs')
text = main.read_text()
text = text.replace('mod app;\nmod gpu;\n', 'mod app;\nmod cpu;\nmod gpu;\n', 1)
main.write_text(text)

# app.rs
app = Path('src/app.rs')
text = app.read_text()
text = text.replace(
'''use crate::{\n    gpu::{GpuMonitor, GpuStats},\n    llama::{LlamaMonitor, LlmStats},\n    ui::{\n        self, CpuCoreKind, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,\n    },\n};''',
'''use crate::{\n    cpu::{detect_cpu_topology, CpuTopology},\n    gpu::{GpuMonitor, GpuStats},\n    llama::{LlamaMonitor, LlmStats},\n    ui::{self, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS},\n};''',
1)
text = text.replace(
'''        let mut cpu_core_kinds: Option<Vec<CpuCoreKind>> = None;''',
'''        let mut cpu_topology: Option<CpuTopology> = None;''',
1)
text = text.replace(
'''                let core_kinds = cpu_core_kinds\n                    .get_or_insert_with(|| detect_cpu_core_kinds(per_cpu_usage.len()))\n                    .clone();\n                system_stats = SystemStats {\n                    cpu_usage: system.global_cpu_usage() as f64,\n                    per_cpu_usage,\n                    cpu_core_kinds: core_kinds,''',
'''                let topology = cpu_topology\n                    .get_or_insert_with(|| detect_cpu_topology(per_cpu_usage.len()))\n                    .clone();\n                system_stats = SystemStats {\n                    cpu_usage: system.global_cpu_usage() as f64,\n                    per_cpu_usage,\n                    cpu_topology: topology,''',
1)
start = text.index('fn detect_cpu_core_kinds(')
end = text.index('fn read_cpu_frequency_mhz()', start)
text = text[:start] + text[end:]
old_test = '''    #[test]\n    fn parses_linux_cpu_lists() {\n        assert_eq!(\n            parse_cpu_list("0-3,8,10-11\\n"),\n            Some(vec![0, 1, 2, 3, 8, 10, 11])\n        );\n        assert_eq!(parse_cpu_list("7"), Some(vec![7]));\n        assert_eq!(parse_cpu_list("3-1"), None);\n    }\n\n'''
text = text.replace(old_test, '', 1)
app.write_text(text)

# ui.rs
ui = Path('src/ui.rs')
text = ui.read_text()
text = text.replace(
'''use crate::{gpu::GpuStats, llama::LlmStats};''',
'''use crate::{\n    cpu::{CpuCoreKind, CpuTopology, CpuVendor},\n    gpu::GpuStats,\n    llama::LlmStats,\n};''',
1)
old_enum = '''#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]\npub enum CpuCoreKind {\n    Performance,\n    Efficiency,\n    #[default]\n    Unknown,\n}\n\n'''
text = text.replace(old_enum, '', 1)
text = text.replace(
'''    pub per_cpu_usage: Vec<f64>,\n    pub cpu_core_kinds: Vec<CpuCoreKind>,''',
'''    pub per_cpu_usage: Vec<f64>,\n    pub cpu_topology: CpuTopology,''',
1)

start = text.index('fn draw_system(')
end = text.index('fn core_usage_color(', start)
new_block = r'''fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(system_panel_title(&system.cpu_topology, area.width))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 16 || inner.height == 0 {
        return;
    }

    let ram_pct = percent(
        system.memory_used_bytes as f64,
        system.memory_total_bytes as f64,
    );
    let bar_width = inner.width.saturating_sub(16).max(8) as usize;

    if inner.width < 35 || inner.height < 10 {
        let mut lines = vec![
            meter_line(
                "CPU",
                system.cpu_usage,
                bar_width,
                ORK_GREEN,
                format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
                vec![],
            ),
            meter_line(
                "RAM",
                ram_pct,
                bar_width,
                CYAN,
                format!("{:>4.1}%", ram_pct),
                vec![],
            ),
            Line::from(vec![
                label_span(" USED     "),
                value_span(
                    &format!("{:.1} GiB", bytes_to_gib(system.memory_used_bytes)),
                    CYAN,
                ),
            ]),
            Line::from(vec![
                label_span(" TOTAL    "),
                value_span(
                    &format!("{:.1} GiB", bytes_to_gib(system.memory_total_bytes)),
                    WHITE,
                ),
            ]),
        ];
        lines.truncate(inner.height as usize);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let frequency = system
        .cpu_frequency_mhz
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| format!("{:.2} GHz", value / 1000.0))
        .unwrap_or_else(|| "—".to_string());
    let temperature = system
        .cpu_temperature_c
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.0}°C"))
        .unwrap_or_else(|| "—".to_string());
    let temperature_tint = system
        .cpu_temperature_c
        .map(temperature_color)
        .unwrap_or(MUTED);
    let io_wait = system
        .io_wait_pct
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "—".to_string());

    let busiest = system
        .per_cpu_usage
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(index, _)| index);

    let mut lines = vec![
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
            value_span(&frequency, CYAN),
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
    ];

    lines.extend(core_heatmap_rows(
        &system.per_cpu_usage,
        &system.cpu_topology.core_kinds,
        inner.width,
        busiest,
        system.cpu_topology.is_hybrid(),
        4,
    ));

    lines.extend([
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
    ]);

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn system_panel_title(topology: &CpuTopology, width: u16) -> String {
    let model = compact_cpu_model(topology);
    let topology_text = if topology.is_hybrid() {
        match (topology.performance_cores, topology.efficiency_cores) {
            (Some(p), Some(e)) => format!("{p}P+{e}E/{}T", topology.logical_cpus),
            _ => format!(
                "P{}T+E{}T",
                topology.performance_threads(),
                topology.efficiency_threads()
            ),
        }
    } else if let Some(cores) = topology.physical_cores {
        format!("{cores}C/{}T", topology.logical_cpus)
    } else if topology.logical_cpus > 0 {
        format!("{}T", topology.logical_cpus)
    } else {
        String::new()
    };

    let mut title = if topology_text.is_empty() {
        format!(" SYSTEM · {model} ")
    } else {
        format!(" SYSTEM · {model} · {topology_text} ")
    };

    let max_len = width.saturating_sub(2) as usize;
    if title.chars().count() > max_len && max_len > 4 {
        title = truncate_title(&title, max_len);
    }
    title
}

fn compact_cpu_model(topology: &CpuTopology) -> String {
    let model = topology.model.trim();
    if model.is_empty() {
        return topology.vendor.label().to_string();
    }

    if topology.vendor == CpuVendor::Intel {
        let parts = model.split_whitespace().collect::<Vec<_>>();
        if let Some(part) = parts
            .iter()
            .find(|part| ["i3-", "i5-", "i7-", "i9-"].iter().any(|prefix| part.starts_with(prefix)))
        {
            return (*part).to_string();
        }
        if let Some(pos) = model.find("Core Ultra") {
            return model[pos..]
                .split_whitespace()
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
        }
    }

    if topology.vendor == CpuVendor::Amd {
        if let Some(pos) = model.find("Ryzen") {
            return model[pos..]
                .split_whitespace()
                .take_while(|part| !part.contains("-Core"))
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
        }
        if let Some(pos) = model.find("EPYC") {
            return model[pos..]
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
        }
    }

    model.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
}

fn truncate_title(title: &str, max_len: usize) -> String {
    if title.chars().count() <= max_len {
        return title.to_string();
    }
    let mut result = title.chars().take(max_len.saturating_sub(1)).collect::<String>();
    result.push('…');
    result
}

fn core_heatmap_rows(
    usages: &[f64],
    kinds: &[CpuCoreKind],
    width: u16,
    busiest: Option<usize>,
    show_kind: bool,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if usages.is_empty() || max_rows == 0 {
        return vec![Line::from(vec![label_span(" CORES "), value_span("—", MUTED)])];
    }

    let digits = usages.len().saturating_sub(1).to_string().len().max(2);
    let cell_width = digits + usize::from(show_kind) + 2;
    let max_cols = (width.saturating_sub(2) as usize / cell_width).max(1);
    let preferred_cols = 6.min(max_cols);
    let cols_for_row_limit = usages.len().div_ceil(max_rows);
    let cols = preferred_cols.max(cols_for_row_limit);

    if cols <= max_cols {
        let row_count = usages.len().div_ceil(cols).min(max_rows);
        return (0..row_count)
            .map(|row| {
                core_heatmap_numbered_line(
                    usages,
                    kinds,
                    row * cols,
                    ((row + 1) * cols).min(usages.len()),
                    busiest,
                    show_kind,
                    digits,
                )
            })
            .collect();
    }

    compact_core_heatmap_rows(usages, busiest, max_rows, digits)
}

fn core_heatmap_numbered_line(
    usages: &[f64],
    kinds: &[CpuCoreKind],
    start: usize,
    end: usize,
    busiest: Option<usize>,
    show_kind: bool,
    digits: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];

    for (index, usage) in usages.iter().copied().enumerate().take(end).skip(start) {
        if index > start {
            spans.push(Span::raw(" "));
        }

        let kind = kinds.get(index).copied().unwrap_or_default();
        let suffix = if show_kind {
            match kind {
                CpuCoreKind::Performance => "P",
                CpuCoreKind::Efficiency => "E",
                CpuCoreKind::Unknown => "?",
            }
        } else {
            ""
        };
        let mut label_style = Style::default().fg(MUTED);
        let mut heat_style = Style::default().fg(core_usage_color(usage));
        if busiest == Some(index) {
            label_style = label_style.add_modifier(Modifier::BOLD);
            heat_style = heat_style.add_modifier(Modifier::BOLD);
        }

        spans.push(Span::styled(
            format!("{index:0digits$}{suffix}"),
            label_style,
        ));
        spans.push(Span::styled(core_usage_glyph(usage).to_string(), heat_style));
    }

    Line::from(spans)
}

fn compact_core_heatmap_rows(
    usages: &[f64],
    busiest: Option<usize>,
    max_rows: usize,
    digits: usize,
) -> Vec<Line<'static>> {
    let rows = usages.len().min(max_rows).max(1);
    let per_row = usages.len().div_ceil(rows);
    let mut lines = Vec::with_capacity(rows);

    for row in 0..rows {
        let start = row * per_row;
        let end = ((row + 1) * per_row).min(usages.len());
        if start >= end {
            break;
        }

        let mut spans = vec![Span::styled(
            format!(" {start:0digits$}-{:0digits$} ", end - 1),
            Style::default().fg(MUTED),
        )];
        for (index, usage) in usages.iter().copied().enumerate().take(end).skip(start) {
            let mut style = Style::default().fg(core_usage_color(usage));
            if busiest == Some(index) {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(core_usage_glyph(usage).to_string(), style));
        }
        lines.push(Line::from(spans));
    }

    lines
}

'''
text = text[:start] + new_block + text[end:]
ui.write_text(text)
