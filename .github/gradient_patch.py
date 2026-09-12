from pathlib import Path

p = Path('src/ui.rs')
s = p.read_text()
old = '''fn meter_line(
    label: &'static str,
    percent: f64,
    width: usize,
    color: Color,
    value: String,
    suffix: Vec<Span<'static>>,
) -> Line<'static> {
    let pct = clamp_percent(percent);
    let (filled, empty) = fine_bar(pct, width);
    let mut spans = vec![
        Span::styled(format!(" {label:<5}"), Style::default().fg(MUTED)),
        Span::styled(filled, Style::default().fg(color)),
        Span::styled(empty, Style::default().fg(BAR_EMPTY)),
        Span::styled(format!(" {value:<8}"), Style::default().fg(WHITE)),
    ];
    spans.extend(suffix);
    Line::from(spans)
}
'''
new = '''fn meter_line(
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
'''
if old not in s:
    raise SystemExit('meter_line block not found')
s = s.replace(old, new)
anchor = '''fn fine_bar(percent: f64, width: usize) -> (String, String) {
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
'''
insert = anchor + '''
fn fine_bar_spans(
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

fn bar_gradient_color(label: &str, level: f64, fallback: Color) -> Color {
    const DARK_GREEN: Color = Color::Rgb(35, 90, 38);
    const DARK_CYAN: Color = Color::Rgb(24, 82, 100);

    let stops: &[(f64, Color)] = match label {
        "GPU" | "CPU" => &[(0.0, DARK_GREEN), (100.0, ORK_GREEN)],
        "VRAM" => &[
            (0.0, DARK_CYAN),
            (80.0, CYAN),
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

fn interpolate_stops(level: f64, stops: &[(f64, Color)]) -> Color {
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

fn mix_color(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let lerp = |x: u8, y: u8| -> u8 {
                (x as f64 + (y as f64 - x as f64) * t).round().clamp(0.0, 255.0) as u8
            };
            Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
        }
        _ if t < 0.5 => a,
        _ => b,
    }
}
'''
if anchor not in s:
    raise SystemExit('fine_bar block not found')
s = s.replace(anchor, insert)
old_test = '''    #[test]
    fn nan_percent_is_safely_clamped() {
        assert_eq!(clamp_percent(f64::NAN), 0.0);
    }
'''
new_test = '''    #[test]
    fn bar_gradients_use_expected_endpoints() {
        assert_eq!(bar_gradient_color("GPU", 100.0, WHITE), ORK_GREEN);
        assert_eq!(bar_gradient_color("VRAM", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("PWR", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("CTX", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("OTHER", 50.0, CYAN), CYAN);
    }

    #[test]
    fn gradient_bar_preserves_terminal_width() {
        let spans = fine_bar_spans("GPU", 50.0, 12, ORK_GREEN);
        assert_eq!(Line::from(spans).width(), 12);
    }

    #[test]
    fn nan_percent_is_safely_clamped() {
        assert_eq!(clamp_percent(f64::NAN), 0.0);
    }
'''
if old_test not in s:
    raise SystemExit('test anchor not found')
s = s.replace(old_test, new_test)
p.write_text(s)
