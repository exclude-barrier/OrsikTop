//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

pub(super) fn format_sample_age(age: Duration) -> String {
    let millis = age.as_millis();
    if millis < 1_000 {
        return format!("{millis} ms");
    }
    let seconds = age.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1} s");
    }
    let total = age.as_secs();
    format!("{}m {:02}s", total / 60, total % 60)
}

pub(super) fn format_uptime(uptime: Duration) -> String {
    let total = uptime.as_secs();
    let days = total / 86_400;
    let hours = (total / 3_600) % 24;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

pub(super) fn compact_rate(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        "0".to_string()
    } else if value >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

pub(super) fn llm_metric_cell(text: &str, width: usize, color: Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(format!("{text:<width$}"), style)
}

pub(super) fn llm_sep() -> Span<'static> {
    Span::styled("  │  ", Style::default().fg(INNER_GREEN))
}

pub(super) fn grouped_u64(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(super) fn grouped_f64(value: f64) -> String {
    if value.is_finite() && value >= 0.0 {
        grouped_u64(value.round() as u64)
    } else {
        "—".to_string()
    }
}

/// Compact token count for the per-slot overview: `58745` -> `58.7k`,
/// small values stay plain (`999`).
pub(super) fn compact_tokens(value: u64) -> String {
    if value < 1000 {
        value.to_string()
    } else {
        format!("{:.1}k", value as f64 / 1000.0)
    }
}

pub(super) fn compact_cpu_model(topology: &CpuTopology) -> String {
    let model = topology.model.trim();
    if model.is_empty() {
        return topology.vendor.label().to_string();
    }

    if topology.vendor == CpuVendor::Intel {
        let parts = model.split_whitespace().collect::<Vec<_>>();
        if let Some(part) = parts.iter().find(|part| {
            ["i3-", "i5-", "i7-", "i9-"]
                .iter()
                .any(|prefix| part.starts_with(prefix))
        }) {
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

    model
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn truncate_title(title: &str, max_len: usize) -> String {
    if title.chars().count() <= max_len {
        return title.to_string();
    }
    let mut result = title
        .chars()
        .take(max_len.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    result
}

pub(super) fn fit_cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = text.chars().take(width - 1).collect::<String>();
    out.push('…');
    out
}

pub(super) fn compact_memory(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1}G", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0}M", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0}K", bytes / KIB)
    } else {
        format!("{}B", bytes as u64)
    }
}

/// GPU-resident memory for the wide process table. Shows a dash when the
/// process holds no DRM fd (explicit, never a faked zero).
pub(super) fn gpu_cell(bytes: u64) -> String {
    if bytes > 0 {
        compact_memory(bytes)
    } else {
        "—".to_string()
    }
}

/// Thread count cell. A process whose `/proc/<pid>/status` could not be read
/// on this cycle renders `—` (unknown), never a fabricated value.
pub(super) fn thread_cell(threads: Option<usize>) -> String {
    threads
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".to_string())
}

pub(super) fn compact_endpoint(server: &str) -> String {
    // Never paint a credential, path or query token: reuse the diagnostics
    // redactor, then drop the scheme for the compact header.
    let safe = crate::redact::safe_endpoint(server);
    if safe == crate::redact::UNPARSEABLE_ENDPOINT {
        // A legacy scheme-less `host:port` with no userinfo/path/query/fragment
        // is safe to show; anything else stays hidden.
        let trimmed = server.trim();
        if !trimmed.is_empty() && !trimmed.contains(['@', '/', '?', '#', ' ']) {
            return trimmed.to_string();
        }
        return safe;
    }
    safe.trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string()
}

pub(super) fn button_span(text: &'static str) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Black)
            .bg(ORK_GREEN)
            .add_modifier(Modifier::BOLD),
    )
}

pub(super) fn label_span(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(MUTED))
}

pub(super) fn value_span(text: &str, color: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(color))
}

pub(super) fn fixed_data_pair(
    label: &'static str,
    value: String,
    color: Color,
    width: usize,
) -> Span<'static> {
    let text = format!("  {label:<7} {value}");
    Span::styled(format!("{text:<width$}"), Style::default().fg(color))
}

pub(super) fn meter_line(
    label: &'static str,
    percent: f64,
    width: usize,
    color: Color,
    value: String,
    suffix: Vec<Span<'static>>,
) -> Line<'static> {
    let pct = clamp_percent(percent);
    let mut spans = vec![Span::styled(
        format!(" {label:<5}"),
        Style::default().fg(MUTED),
    )];
    spans.extend(fine_bar_spans(label, pct, width, color));
    spans.push(Span::styled(
        format!(" {value:<8}"),
        Style::default().fg(WHITE),
    ));
    spans.extend(suffix);
    Line::from(spans)
}

/// A `meter_line`-shaped placeholder for a metric the driver does not expose
/// (e.g. GPU utilization on Intel, where no busy counter is surfaced). The
/// pre-suffix width matches `meter_line` (`6 + width + 9` cells) so the trailing
/// data pairs stay aligned with the rows above and below.
pub(super) fn unavailable_meter_line(
    label: &'static str,
    width: usize,
    suffix: Vec<Span<'static>>,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!(" {label:<5}"), Style::default().fg(MUTED)),
        Span::styled(
            format!("{:<width$}", " \u{2014}", width = width + 9),
            Style::default().fg(MUTED),
        ),
    ];
    spans.extend(suffix);
    Line::from(spans)
}

#[cfg(test)]
pub(super) fn fine_bar(percent: f64, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }

    // Braille gives two horizontal subcells per terminal cell.
    // Full = ⣿, half = ⣇ (left column filled + baseline), empty = ⣀.
    let units = ((clamp_percent(percent) / 100.0) * (width * 2) as f64).round() as usize;
    let full = (units / 2).min(width);
    let half = full < width && units % 2 == 1;

    let mut filled = "⣿".repeat(full);
    if half {
        filled.push('⣇');
    }

    let used_cells = full + usize::from(half);
    let empty = "⣀".repeat(width.saturating_sub(used_cells));
    (filled, empty)
}

pub(super) fn fine_bar_spans(
    label: &str,
    percent: f64,
    width: usize,
    fallback: Color,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let total_units = width * 2;
    let units = ((clamp_percent(percent) / 100.0) * total_units as f64).round() as usize;
    let mut spans = Vec::with_capacity(width);

    for cell in 0..width {
        let start = cell * 2;
        let filled_units = units.saturating_sub(start).min(2);
        if filled_units == 0 {
            spans.push(Span::styled("⣀", Style::default().fg(BAR_EMPTY)));
            continue;
        }

        let glyph = if filled_units == 2 { "⣿" } else { "⣇" };
        let level = ((start + filled_units) as f64 / total_units as f64 * 100.0).clamp(0.0, 100.0);
        spans.push(Span::styled(
            glyph,
            Style::default().fg(bar_gradient_color(label, level, fallback)),
        ));
    }

    spans
}

pub(super) fn bar_gradient_color(label: &str, level: f64, fallback: Color) -> Color {
    const DARK_GREEN: Color = Color::Rgb(35, 90, 38);
    const DARK_CYAN: Color = Color::Rgb(24, 82, 100);

    let stops: &[(f64, Color)] = match label {
        "GPU" | "CPU" => &[(0.0, DARK_GREEN), (100.0, ORK_GREEN)],
        "VRAM" => &[
            (0.0, DARK_CYAN),
            (90.0, CYAN),
            (95.0, YELLOW),
            (98.0, ORANGE),
            (100.0, RED),
        ],
        "RAM" => &[
            (0.0, DARK_CYAN),
            (70.0, CYAN),
            (85.0, YELLOW),
            (95.0, ORANGE),
            (100.0, RED),
        ],
        "PWR" => &[
            (0.0, DARK_GREEN),
            (65.0, ORK_GREEN),
            (85.0, YELLOW),
            (95.0, ORANGE),
            (100.0, RED),
        ],
        "CTX" => &[
            (0.0, DARK_GREEN),
            (65.0, ORK_GREEN),
            (80.0, YELLOW),
            (90.0, ORANGE),
            (97.0, RED),
            (100.0, RED),
        ],
        _ => return fallback,
    };

    interpolate_stops(level, stops)
}

pub(super) fn interpolate_stops(level: f64, stops: &[(f64, Color)]) -> Color {
    let level = clamp_percent(level);
    for pair in stops.windows(2) {
        let (start_at, start_color) = pair[0];
        let (end_at, end_color) = pair[1];
        if level <= end_at {
            let span = (end_at - start_at).max(f64::EPSILON);
            let t = ((level - start_at) / span).clamp(0.0, 1.0);
            return mix_color(start_color, end_color, t);
        }
    }
    stops.last().map(|(_, color)| *color).unwrap_or(WHITE)
}

pub(super) fn mix_color(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let lerp = |x: u8, y: u8| -> u8 {
                (x as f64 + (y as f64 - x as f64) * t)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
        }
        _ if t < 0.5 => a,
        _ => b,
    }
}

pub(super) fn percent(value: f64, total: f64) -> f64 {
    if value.is_finite() && total.is_finite() && total > 0.0 {
        clamp_percent(value / total * 100.0)
    } else {
        0.0
    }
}

pub(super) fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

pub(super) fn gpu_bar_width(width: u16) -> usize {
    match width {
        0..=89 => 14,
        90..=119 => 22,
        _ => 30,
    }
}

pub(super) fn optional_number(value: Option<f64>, decimals: usize, suffix: &str) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.decimals$}{suffix}"),
        _ => "—".to_string(),
    }
}

pub(super) fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}
