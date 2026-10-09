//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

/// The `1 / 5 / 15` load-average row. All three come from one `/proc/loadavg`
/// read, so they are either all known or all unknown; an unknown read renders
/// `—` rather than three fake zeros.
pub(super) fn load_average_text(system: &SystemStats) -> String {
    match (system.load_one, system.load_five, system.load_fifteen) {
        (Some(one), Some(five), Some(fifteen)) => {
            format!("{one:.2} / {five:.2} / {fifteen:.2}")
        }
        _ => "—".to_string(),
    }
}

pub(super) fn system_panel_height(system: &SystemStats) -> u16 {
    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {
        let groups = &system.cpu_topology.physical_core_groups;
        let count = |kind: CpuCoreKind| groups.iter().filter(|core| core.kind == kind).count();
        let rows = count(CpuCoreKind::Performance)
            .max(count(CpuCoreKind::Efficiency))
            .max(1);
        return (rows as u16 + 10).max(13);
    }
    13
}

/// Whether the SYSTEM panel renders the full view (fixed telemetry rows + the
/// per-core section: the P/E minibar on hybrid boxes, the per-core heatmap
/// otherwise) rather than the 4-line compact fallback. The width floor is what
/// the minibar columns need; the height floor is the minimum full-view content
/// (7 fixed rows + the minibar header + one core row). It must stay at or
/// below the smallest full-view allocation so short hybrid boxes — e.g. a
/// 4P+4E Core Ultra with `inner.height` 12 — still get the P/E minibar instead
/// of silently collapsing to the compact view.
pub(super) fn system_full_view(width: u16, height: u16) -> bool {
    width >= 35 && height >= 9
}

pub(super) fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(system_panel_title(&system.cpu_topology, area.width))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 16 || inner.height == 0 {
        return;
    }

    let bar_width = inner.width.saturating_sub(16).max(8) as usize;

    // Same validity semantics as VRAM/POWER: an unavailable or zero total
    // means the RAM meter renders as unavailable ("—"), never a fabricated
    // 0.0 % / 0.0 GiB.
    let ram_pct = if system.memory_total_bytes > 0 {
        Some(percent(
            system.memory_used_bytes as f64,
            system.memory_total_bytes as f64,
        ))
    } else {
        None
    };
    let ram_meter = match ram_pct {
        Some(pct) => meter_line(
            "RAM",
            pct,
            bar_width,
            CYAN,
            format!("{:>4.1}%", pct),
            vec![],
        ),
        None => unavailable_meter_line("RAM", bar_width, vec![]),
    };
    let ram_used_text = match ram_pct {
        Some(_) => format!("{:.1} GiB", bytes_to_gib(system.memory_used_bytes)),
        None => "—".to_string(),
    };
    let ram_total_text = match ram_pct {
        Some(_) => format!("{:.1} GiB", bytes_to_gib(system.memory_total_bytes)),
        None => "—".to_string(),
    };
    let ram_pair_text = match ram_pct {
        Some(_) => format!(
            "{:.1} / {:.1} GiB",
            bytes_to_gib(system.memory_used_bytes),
            bytes_to_gib(system.memory_total_bytes)
        ),
        None => "—".to_string(),
    };

    if !system_full_view(inner.width, inner.height) {
        let mut lines = vec![
            meter_line(
                "CPU",
                system.cpu_usage,
                bar_width,
                ORK_GREEN,
                format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
                vec![],
            ),
            ram_meter.clone(),
            Line::from(vec![
                label_span(" USED     "),
                value_span(&ram_used_text, CYAN),
            ]),
            Line::from(vec![
                label_span(" TOTAL    "),
                value_span(&ram_total_text, WHITE),
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
    let power = system
        .cpu_power_w
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| format!("{value:.1} W"))
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
                &load_average_text(system),
                if system.load_one.is_some() {
                    WHITE
                } else {
                    MUTED
                },
            ),
        ]),
        Line::from(vec![
            label_span(" POWER "),
            value_span(
                &power,
                if system.cpu_power_w.is_some() {
                    CYAN
                } else {
                    MUTED
                },
            ),
        ]),
    ];

    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {
        lines.extend(physical_core_minibar_rows(
            &system.per_cpu_usage,
            &system.cpu_topology.online,
            &system.cpu_topology.physical_core_groups,
            inner.width,
        ));
    } else {
        lines.extend(core_heatmap_rows(
            &system.per_cpu_usage,
            &system.cpu_topology.core_kinds,
            &system.cpu_topology.online,
            inner.width,
            busiest,
            system.cpu_topology.is_hybrid(),
            4,
        ));
    }

    lines.extend([
        ram_meter,
        Line::from(vec![label_span("      "), value_span(&ram_pair_text, CYAN)]),
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

pub(super) fn system_panel_title(topology: &CpuTopology, width: u16) -> String {
    let model = compact_cpu_model(topology);
    let topology_text = if topology.is_hybrid() {
        let mut text = match (topology.performance_cores, topology.efficiency_cores) {
            (Some(p), Some(e)) => format!("{p}P+{e}E/{}T", topology.logical_cpus),
            _ => format!(
                "P{}T+E{}T",
                topology.performance_threads(),
                topology.efficiency_threads()
            ),
        };
        if let Some(low) = topology.low_power_cores.filter(|low| *low > 0) {
            text.push_str(&format!("+{low}L"));
        }
        text
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

pub(super) fn physical_core_minibar_rows(
    usages: &[f64],
    online: &[bool],
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
            let usage = physical_core_average(core, usages, online);
            let color = physical_core_color(usage, ORK_GREEN);
            spans.push(Span::styled(
                format!(" P{row}  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
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
            let usage = physical_core_average(core, usages, online);
            let color = physical_core_color(usage, CYAN);
            spans.push(Span::styled(
                format!("E{row}  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
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

pub(super) fn physical_core_average(
    core: &CpuPhysicalCore,
    usages: &[f64],
    online: &[bool],
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for &cpu in &core.logical_cpus {
        // Skip offline SMT siblings: their usage slot is a placeholder 0.0 and
        // would otherwise halve the core's average.
        if !online.get(cpu).copied().unwrap_or(true) {
            continue;
        }
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

pub(super) fn physical_core_color(usage: f64, base: Color) -> Color {
    // Keep the P/E identity color at normal load, then smoothly heat up as a
    // physical core approaches saturation: base -> yellow -> orange -> red.
    interpolate_stops(
        usage,
        &[
            (0.0, base),
            (55.0, base),
            (80.0, YELLOW),
            (92.0, ORANGE),
            (100.0, RED),
        ],
    )
}

pub(super) fn mini_core_bar_spans(percent: f64, width: usize, color: Color) -> Vec<Span<'static>> {
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

pub(super) fn core_heatmap_rows(
    usages: &[f64],
    kinds: &[CpuCoreKind],
    online: &[bool],
    width: u16,
    busiest: Option<usize>,
    show_kind: bool,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if usages.is_empty() || max_rows == 0 {
        return vec![Line::from(vec![
            label_span(" CORES "),
            value_span("—", MUTED),
        ])];
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
                    online,
                    row * cols,
                    ((row + 1) * cols).min(usages.len()),
                    busiest,
                    show_kind,
                    digits,
                )
            })
            .collect();
    }

    compact_core_heatmap_rows(usages, online, busiest, max_rows, digits)
}

// A rendering helper with explicit layout parameters; grouping them into a
// struct would not read better.
#[allow(clippy::too_many_arguments)]
pub(super) fn core_heatmap_numbered_line(
    usages: &[f64],
    kinds: &[CpuCoreKind],
    online: &[bool],
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
                CpuCoreKind::LowPower => "L",
                CpuCoreKind::Unknown => "?",
            }
        } else {
            ""
        };
        // An offline hole is not an idle 0% core: show a muted dash.
        if !online.get(index).copied().unwrap_or(true) {
            spans.push(Span::styled(
                format!("{index:0digits$}{suffix}"),
                Style::default().fg(MUTED),
            ));
            spans.push(Span::styled(
                "\u{2014}".to_string(),
                Style::default().fg(MUTED),
            ));
            continue;
        }
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
        spans.push(Span::styled(
            core_usage_glyph(usage).to_string(),
            heat_style,
        ));
    }

    Line::from(spans)
}

pub(super) fn compact_core_heatmap_rows(
    usages: &[f64],
    online: &[bool],
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
            if !online.get(index).copied().unwrap_or(true) {
                spans.push(Span::styled(
                    "\u{2014}".to_string(),
                    Style::default().fg(MUTED),
                ));
                continue;
            }
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

pub(super) fn core_usage_color(usage: f64) -> Color {
    match clamp_percent(usage) {
        value if value >= 90.0 => ORANGE,
        value if value >= 75.0 => YELLOW,
        value if value >= 50.0 => BRIGHT_GREEN,
        value if value >= 25.0 => ORK_GREEN,
        value if value >= 2.0 => DIM_GREEN,
        _ => BAR_EMPTY,
    }
}

pub(super) fn core_usage_glyph(usage: f64) -> char {
    match clamp_percent(usage) {
        value if value >= 75.0 => '█',
        value if value >= 50.0 => '▓',
        value if value >= 25.0 => '▒',
        value if value >= 2.0 => '░',
        _ => '·',
    }
}
