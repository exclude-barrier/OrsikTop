from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


gpu_path = Path("src/gpu.rs")
gpu = gpu_path.read_text()

gpu = replace_once(
    gpu,
    "use nvml_wrapper::{\n    enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor},\n    Nvml,\n};",
    "use nvml_wrapper::{\n    bitmasks::device::ThrottleReasons,\n    enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor},\n    Nvml,\n};",
    "gpu import",
)

gpu = replace_once(
    gpu,
    "    pub pstate: String,\n    pub graphics_clock_mhz: Option<f64>,",
    "    pub pstate: String,\n    pub limit_reason: String,\n    pub graphics_clock_mhz: Option<f64>,",
    "limit_reason field",
)

gpu = replace_once(
    gpu,
    "            pstate: device\n                .performance_state()\n                .map(|state| normalize_pstate(&format!(\"{state:?}\")))\n                .unwrap_or_else(|_| \"—\".to_string()),\n            graphics_clock_mhz:",
    "            pstate: device\n                .performance_state()\n                .map(|state| normalize_pstate(&format!(\"{state:?}\")))\n                .unwrap_or_else(|_| \"—\".to_string()),\n            limit_reason: device\n                .current_throttle_reasons()\n                .map(format_throttle_reasons)\n                .unwrap_or_else(|_| \"—\".to_string()),\n            graphics_clock_mhz:",
    "limit_reason sample",
)

marker = "fn sanitize(stats: &mut GpuStats) {"
formatter = r'''fn format_throttle_reasons(reasons: ThrottleReasons) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }

    let mut labels = Vec::new();

    if reasons.contains(ThrottleReasons::SW_POWER_CAP) {
        labels.push("power");
    }
    if reasons.intersects(
        ThrottleReasons::SW_THERMAL_SLOWDOWN | ThrottleReasons::HW_THERMAL_SLOWDOWN,
    ) {
        labels.push("thermal");
    }
    if reasons.contains(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN) {
        labels.push("power-brake");
    }
    if reasons.contains(ThrottleReasons::HW_SLOWDOWN) {
        labels.push("hw");
    }
    if reasons.contains(ThrottleReasons::SYNC_BOOST) {
        labels.push("sync");
    }
    if reasons.contains(ThrottleReasons::APPLICATIONS_CLOCKS_SETTING) {
        labels.push("app-clock");
    }
    if reasons.contains(ThrottleReasons::DISPLAY_CLOCK_SETTING) {
        labels.push("display");
    }
    if reasons.contains(ThrottleReasons::GPU_IDLE) {
        labels.push("idle");
    }

    if labels.is_empty() {
        "other".to_string()
    } else {
        labels.join("+")
    }
}

'''
gpu = replace_once(gpu, marker, formatter + marker, "throttle formatter")

test_marker = "    #[test]\n    fn clamps_utilization_but_allows_fan_above_100() {"
test = r'''    #[test]
    fn formats_nvml_throttle_reasons() {
        assert_eq!(format_throttle_reasons(ThrottleReasons::empty()), "none");
        assert_eq!(
            format_throttle_reasons(ThrottleReasons::SW_POWER_CAP),
            "power"
        );
        assert_eq!(
            format_throttle_reasons(
                ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_THERMAL_SLOWDOWN
            ),
            "power+thermal"
        );
        assert_eq!(format_throttle_reasons(ThrottleReasons::GPU_IDLE), "idle");
    }

'''
gpu = replace_once(gpu, test_marker, test + test_marker, "throttle tests")
gpu_path.write_text(gpu)

ui_path = Path("src/ui.rs")
ui = ui_path.read_text()

ui = replace_once(
    ui,
    "const ORK_GREEN: Color = Color::Rgb(105, 210, 70);",
    "const BG_BLACK: Color = Color::Rgb(0, 0, 0);\nconst BAR_EMPTY: Color = Color::Rgb(48, 52, 48);\nconst ORK_GREEN: Color = Color::Rgb(105, 210, 70);",
    "UI colors",
)
ui = replace_once(
    ui,
    "Block::default().style(Style::default().bg(Color::Black))",
    "Block::default().style(Style::default().bg(BG_BLACK))",
    "true black canvas",
)

start = ui.index("fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats) {")
end = ui.index("\nfn draw_llm_and_system", start)
new_draw_gpu = r'''fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats) {
    let title = if gpu.available {
        format!(" GPU{} · {} ", gpu.index, gpu.name)
    } else {
        format!(" GPU{} · NVIDIA / NVML unavailable ", gpu.index)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 28 || inner.height == 0 {
        return;
    }

    if !gpu.available {
        frame.render_widget(
            Paragraph::new(" GPU telemetry unavailable")
                .style(Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            inner,
        );
        return;
    }

    let vram_pct = percent(gpu.memory_used_mib, gpu.memory_total_mib);
    let power_pct = match (gpu.power_w, gpu.power_limit_w) {
        (Some(power), Some(limit)) if limit > 0.0 => Some(percent(power, limit)),
        _ => None,
    };
    let bar_width = gpu_bar_width(inner.width);

    let draw_text = match (gpu.power_w, gpu.power_limit_w) {
        (Some(power), Some(limit)) => format!("{power:.0}/{limit:.0} W"),
        (Some(power), None) => format!("{power:.0} W"),
        _ => "—".to_string(),
    };
    let temp_text = optional_number(gpu.temperature_c, 0, "°C");
    let fan_text = optional_number(gpu.fan_percent, 0, "%");
    let core_text = optional_number(gpu.graphics_clock_mhz, 0, " MHz");
    let vclk_text = optional_number(gpu.memory_clock_mhz, 0, " MHz");
    let power_pct_value = power_pct.unwrap_or(0.0);
    let power_tint = power_pct.map(power_color).unwrap_or(MUTED);

    let mut lines = vec![
        meter_line(
            "GPU",
            gpu.utilization,
            bar_width,
            ORK_GREEN,
            format!("{:>3.0}%", gpu.utilization),
            vec![
                fixed_data_pair("CORE", core_text, ORK_GREEN, 21),
                fixed_data_pair("PSTATE", gpu.pstate.clone(), CYAN, 16),
            ],
        ),
        meter_line(
            "VRAM",
            vram_pct,
            bar_width,
            vram_color(vram_pct),
            format!("{:>3.0}%", vram_pct),
            vec![
                fixed_data_pair(
                    "USED",
                    format!(
                        "{:.1}/{:.1} GiB",
                        gpu.memory_used_mib / 1024.0,
                        gpu.memory_total_mib / 1024.0
                    ),
                    vram_color(vram_pct),
                    25,
                ),
                fixed_data_pair("VCLK", vclk_text, CYAN, 20),
            ],
        ),
        meter_line(
            "PWR",
            power_pct_value,
            bar_width,
            power_tint,
            format!("{:>3.0}%", power_pct_value),
            vec![
                fixed_data_pair("DRAW", draw_text, power_tint, 21),
                fixed_data_pair(
                    "TEMP",
                    temp_text,
                    gpu.temperature_c.map(temperature_color).unwrap_or(MUTED),
                    16,
                ),
                fixed_data_pair("FAN", fan_text, ORK_GREEN, 13),
            ],
        ),
        Line::from(vec![
            label_span(" I/O    "),
            Span::raw(" ".repeat(bar_width + 9)),
            fixed_data_pair(
                "MEMCTRL",
                format!("{:.0}%", gpu.memory_utilization),
                CYAN,
                17,
            ),
            fixed_data_pair(
                "PCIe RX",
                optional_number(gpu.pcie_rx_mb_s, 1, " MB/s"),
                CYAN,
                22,
            ),
            fixed_data_pair("TX", optional_number(gpu.pcie_tx_mb_s, 1, " MB/s"), CYAN, 18),
            fixed_data_pair(
                "ENC",
                optional_number(gpu.encoder_utilization, 0, "%"),
                WHITE,
                11,
            ),
            fixed_data_pair(
                "DEC",
                optional_number(gpu.decoder_utilization, 0, "%"),
                WHITE,
                11,
            ),
            fixed_data_pair(
                "LIMIT",
                gpu.limit_reason.clone(),
                limit_reason_color(&gpu.limit_reason),
                20,
            ),
        ]),
    ];

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}
'''
ui = ui[:start] + new_draw_gpu + ui[end:]

old_data_pair = r'''fn data_pair(label: &'static str, value: String, color: Color) -> Span<'static> {
    Span::styled(format!("  {label} {value}"), Style::default().fg(color))
}
'''
new_data_pair = r'''fn fixed_data_pair(
    label: &'static str,
    value: String,
    color: Color,
    width: usize,
) -> Span<'static> {
    let text = format!("  {label:<7}{value}");
    Span::styled(format!("{text:<width$}"), Style::default().fg(color))
}
'''
ui = replace_once(ui, old_data_pair, new_data_pair, "fixed data pair")

old_meter = r'''fn meter_line(
    label: &'static str,
    percent: f64,
    width: usize,
    color: Color,
    value: String,
    suffix: Vec<Span<'static>>,
) -> Line<'static> {
    let pct = clamp_percent(percent);
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let mut spans = vec![
        Span::styled(format!(" {label:<5}"), Style::default().fg(MUTED)),
        Span::styled("▪".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "·".repeat(width.saturating_sub(filled)),
            Style::default().fg(INNER_GREEN),
        ),
        Span::styled(format!(" {value}"), Style::default().fg(WHITE)),
    ];
    spans.extend(suffix);
    Line::from(spans)
}
'''
new_meter = r'''fn meter_line(
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

fn fine_bar(percent: f64, width: usize) -> (String, String) {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

    if width == 0 {
        return (String::new(), String::new());
    }

    let units = ((clamp_percent(percent) / 100.0) * (width * 8) as f64).round() as usize;
    let full = (units / 8).min(width);
    let remainder = if full < width { units % 8 } else { 0 };

    let mut filled = "█".repeat(full);
    if remainder > 0 {
        filled.push(PARTIAL[remainder]);
    }

    let used_cells = full + usize::from(remainder > 0);
    let empty = "·".repeat(width.saturating_sub(used_cells));
    (filled, empty)
}
'''
ui = replace_once(ui, old_meter, new_meter, "fine meter")

old_vram = r'''fn vram_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 96.0 => ORANGE,
        v if v >= 90.0 => YELLOW,
        _ => CYAN,
    }
}
'''
new_vram = r'''fn vram_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 98.0 => ORANGE,
        v if v >= 95.0 => YELLOW,
        _ => CYAN,
    }
}

fn limit_reason_color(reason: &str) -> Color {
    if reason == "none" || reason == "idle" || reason == "—" {
        MUTED
    } else if reason.contains("thermal") || reason.contains("power-brake") || reason.contains("hw") {
        RED
    } else if reason.contains("power") {
        ORANGE
    } else {
        YELLOW
    }
}
'''
ui = replace_once(ui, old_vram, new_vram, "VRAM thresholds and limiter color")

test_marker = "    #[test]\n    fn nan_percent_is_safely_clamped() {"
test = r'''    #[test]
    fn fine_bar_has_subcell_resolution() {
        assert_eq!(fine_bar(0.0, 4), ("".to_string(), "····".to_string()));
        assert_eq!(fine_bar(12.5, 1), ("▏".to_string(), "".to_string()));
        assert_eq!(fine_bar(50.0, 2), ("█".to_string(), "·".to_string()));
        assert_eq!(fine_bar(100.0, 2), ("██".to_string(), "".to_string()));
    }

'''
ui = replace_once(ui, test_marker, test + test_marker, "fine bar test")
ui_path.write_text(ui)
