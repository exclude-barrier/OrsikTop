use std::collections::VecDeque;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{gpu::GpuStats, llama::LlmStats};

pub const MIN_REFRESH_MS: u64 = 100;
pub const MAX_REFRESH_MS: u64 = 10_000;
pub const REFRESH_STEP_MS: u64 = 100;

const HISTORY_LEN: usize = 180;
const REFRESH_CONTROL_WIDTH: u16 = 22;

const ORK_GREEN: Color = Color::Rgb(105, 210, 70);
const DIM_GREEN: Color = Color::Rgb(70, 135, 60);
const MUTED: Color = Color::Rgb(145, 150, 145);
const PIXEL_OFF: Color = Color::Rgb(48, 55, 50);
const YELLOW: Color = Color::Rgb(220, 205, 75);
const ORANGE: Color = Color::Rgb(230, 145, 60);
const RED: Color = Color::Rgb(235, 75, 75);
const CYAN: Color = Color::Rgb(70, 195, 220);
const WHITE: Color = Color::Rgb(225, 225, 225);

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RefreshControls {
    pub minus: Rect,
    pub plus: Rect,
}

pub struct UiState {
    gpu_history: VecDeque<u64>,
    vram_history: VecDeque<u64>,
    cpu_history: VecDeque<u64>,
    ram_history: VecDeque<u64>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            gpu_history: VecDeque::with_capacity(HISTORY_LEN),
            vram_history: VecDeque::with_capacity(HISTORY_LEN),
            cpu_history: VecDeque::with_capacity(HISTORY_LEN),
            ram_history: VecDeque::with_capacity(HISTORY_LEN),
        }
    }
}

impl UiState {
    pub fn push_sample(&mut self, gpu: &GpuStats, system: &SystemStats) {
        let vram = percent(gpu.memory_used_mib, gpu.memory_total_mib);
        let ram = percent(
            system.memory_used_bytes as f64,
            system.memory_total_bytes as f64,
        );

        push_history(&mut self.gpu_history, gpu.utilization);
        push_history(&mut self.vram_history, vram);
        push_history(&mut self.cpu_history, system.cpu_usage);
        push_history(&mut self.ram_history, ram);
    }
}

pub fn draw(
    frame: &mut Frame,
    system: &SystemStats,
    llm: &LlmStats,
    gpu: &GpuStats,
    state: &UiState,
    server: &str,
    refresh_ms: u64,
) {
    let area = frame.area();

    if area.width < 72 || area.height < 24 {
        draw_too_small(frame, area);
        return;
    }

    let show_history = area.height >= 31;
    let rows = if show_history {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Min(7),
                Constraint::Length(3),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Min(9),
                Constraint::Length(3),
            ])
            .split(area)
    };

    draw_header(frame, rows[0], llm, server, refresh_ms);
    draw_gpu(frame, rows[1], gpu);
    draw_llm_and_system(frame, rows[2], system, llm);

    if show_history {
        draw_history(frame, rows[3], state);
        draw_footer(frame, rows[4], llm, gpu, refresh_ms);
    } else {
        draw_footer(frame, rows[3], llm, gpu, refresh_ms);
    }
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("OrsikTop needs at least 72x24 terminal cells")
            .style(Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD))
            .block(
                Block::default()
                    .title(" OrsikTop ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM_GREEN)),
            ),
        area,
    );
}

fn draw_header(frame: &mut Frame, area: Rect, llm: &LlmStats, server: &str, refresh_ms: u64) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let controls = refresh_controls(area);
    let controls_start = controls
        .map(|c| c.minus.x.saturating_sub(1))
        .unwrap_or(inner.x.saturating_add(inner.width));
    let left_width = controls_start.saturating_sub(inner.x).saturating_sub(1);

    if left_width > 0 {
        let status = if llm.connected { "ONLINE" } else { "OFFLINE" };
        let status_style = if llm.connected {
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(RED).add_modifier(Modifier::BOLD)
        };
        let endpoint = compact_endpoint(server);

        let mut spans = vec![
            Span::styled(
                " OrsikTop ",
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled("· ", Style::default().fg(DIM_GREEN)),
        ];

        if left_width >= 78 {
            spans.push(Span::styled(
                "btop for local LLM Orks   ",
                Style::default().fg(WHITE),
            ));
        }

        spans.extend([
            Span::styled(status, status_style),
            Span::styled("   ", Style::default()),
            Span::styled(endpoint, Style::default().fg(MUTED)),
        ]);

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y, left_width, 1),
        );
    }

    if let Some(c) = controls {
        let start_x = c.minus.x.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                button_span("[ - ]"),
                Span::styled(format!("  {:>4} ms  ", refresh_ms), Style::default().fg(WHITE)),
                button_span("[ + ]"),
            ])),
            Rect::new(start_x, inner.y, REFRESH_CONTROL_WIDTH, 1),
        );
    }
}

fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats) {
    let title = if gpu.available {
        format!(" GPU0 · {} ", gpu.name)
    } else {
        " GPU0 · NVIDIA / NVML unavailable ".to_string()
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
    let power_pct = percent(gpu.power_w, gpu.power_limit_w);
    let bar_width = gpu_bar_width(inner.width);

    let mut lines = vec![
        meter_line(
            "GPU",
            gpu.utilization,
            bar_width,
            ORK_GREEN,
            format!("{:>3.0}%", gpu.utilization),
            vec![
                data_pair("CORE", format!("{:.0} MHz", gpu.graphics_clock_mhz), ORK_GREEN),
                data_pair(
                    "TEMP",
                    format!("{:.0}°C", gpu.temperature_c),
                    temperature_color(gpu.temperature_c),
                ),
                data_pair("P", gpu.pstate.clone(), CYAN),
            ],
        ),
        meter_line(
            "PWR",
            power_pct,
            bar_width,
            power_color(power_pct),
            format!("{:>3.0}W", gpu.power_w),
            vec![
                data_pair("LIMIT", format!("{:.0}W", gpu.power_limit_w), WHITE),
                data_pair("FAN", format!("{:.0}%", gpu.fan_percent), ORK_GREEN),
            ],
        ),
        meter_line(
            "VRM",
            vram_pct,
            bar_width,
            vram_color(vram_pct),
            format!("{:>3.0}%", vram_pct),
            vec![
                data_pair(
                    "VRAM",
                    format!(
                        "{:.1}/{:.1} GiB",
                        gpu.memory_used_mib / 1024.0,
                        gpu.memory_total_mib / 1024.0
                    ),
                    vram_color(vram_pct),
                ),
                data_pair("VCLK", format!("{:.0} MHz", gpu.memory_clock_mhz), CYAN),
            ],
        ),
        Line::from(vec![
            label_span(" BUS "),
            data_pair("MEM", format!("{:.0}%", gpu.memory_utilization), CYAN),
            data_pair("ENC", format!("{:.0}%", gpu.encoder_utilization), WHITE),
            data_pair("DEC", format!("{:.0}%", gpu.decoder_utilization), WHITE),
            data_pair("RX", format!("{:.1} MiB/s", gpu.pcie_rx_mib_s), CYAN),
            data_pair("TX", format!("{:.1} MiB/s", gpu.pcie_tx_mib_s), CYAN),
        ]),
    ];

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_llm_and_system(frame: &mut Frame, area: Rect, system: &SystemStats, llm: &LlmStats) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    draw_llm(frame, cols[0], llm);
    draw_system(frame, cols[1], system);
}

fn draw_llm(frame: &mut Frame, area: Rect, llm: &LlmStats) {
    let block = Block::default()
        .title(" LLM INFERENCE ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    if !llm.connected {
        let lines = vec![
            Line::from(vec![label_span(" STATUS   "), value_span("METRICS OFF", YELLOW)]),
            Line::from(vec![
                label_span(" LLAMA    "),
                Span::styled("restart server with --metrics", Style::default().fg(MUTED)),
            ]),
            Line::from(vec![label_span(" MODEL    "), value_span("—", WHITE)]),
            Line::from(vec![label_span(" CONTEXT  "), value_span("—", WHITE)]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
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

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL    "),
            Span::styled(llm.model.clone(), Style::default().fg(WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            label_span(" SPEED    "),
            Span::styled(format!("PROMPT {:>7.1} tok/s", llm.prompt_tps), Style::default().fg(CYAN)),
            Span::styled("    ", Style::default()),
            Span::styled(
                format!("GENERATE {:>7.1} tok/s", llm.generation_tps),
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" AVG      "),
            Span::styled(format!("PROMPT {:>7.1}", llm.prompt_avg_tps), Style::default().fg(MUTED)),
            Span::styled("    ", Style::default()),
            Span::styled(format!("GENERATE {:>7.1}", llm.generation_avg_tps), Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            label_span(" TOKENS   "),
            value_span(&format!("{:>10.0} prompt", llm.prompt_total), WHITE),
            Span::raw("    "),
            value_span(&format!("{:>10.0} generated", llm.generated_total), WHITE),
        ]),
        Line::from(vec![
            label_span(" QUEUE    "),
            value_span(&format!("{:.0} active", llm.active_requests), WHITE),
            Span::raw(" / "),
            value_span(&format!("{:.0} deferred", llm.deferred_requests), WHITE),
            Span::raw("    "),
            label_span("SLOTS "),
            value_span(&format!("{}/{}", llm.busy_slots, llm.slot_count), CYAN),
            Span::raw("    "),
            label_span("MTP "),
            value_span(&format!("{:.0}%", llm.spec_acceptance_pct), ORK_GREEN),
        ]),
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            vec![Span::styled(
                if llm.context_size > 0 {
                    format!(" {context_used} / {} tok", llm.context_size)
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

fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(" SYSTEM ")
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

    let mut lines = vec![
        meter_line(
            "CPU",
            system.cpu_usage,
            bar_width,
            ORK_GREEN,
            format!("{:>4.1}%", system.cpu_usage),
            vec![],
        ),
        meter_line(
            "RAM",
            ram_pct,
            bar_width,
            vram_color(ram_pct),
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
}

fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .title(" HISTORY · latest samples ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 32 || inner.height < 2 {
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(inner);

    draw_history_column(frame, cols[0], "GPU", &state.gpu_history, ORK_GREEN, true);
    draw_history_column(frame, cols[1], "VRAM", &state.vram_history, CYAN, true);
    draw_history_column(frame, cols[2], "CPU", &state.cpu_history, ORK_GREEN, true);
    draw_history_column(frame, cols[3], "RAM", &state.ram_history, CYAN, false);
}

fn draw_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<u64>,
    color: Color,
    right_border: bool,
) {
    let mut block = Block::default().title(format!(" {title} "));
    if right_border {
        block = block
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(DIM_GREEN));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(history_lines(
            history,
            inner.width as usize,
            inner.height as usize,
            color,
        )),
        inner,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, llm: &LlmStats, gpu: &GpuStats, refresh_ms: u64) {
    let status = if !llm.error.is_empty() {
        friendly_llm_error(&llm.error)
    } else if !gpu.error.is_empty() {
        format!("GPU/NVML · {}", gpu.error)
    } else {
        "READY · MORE POWER, HAPPIER ORKS".to_string()
    };

    let status_color = if !llm.error.is_empty() || !gpu.error.is_empty() {
        YELLOW
    } else {
        ORK_GREEN
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let help = format!(" [q] quit   [-]/[+] refresh   {refresh_ms} ms   ");
    let help_width = help.chars().count() as u16;
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y, inner.width.min(help_width), 1),
    );

    if inner.width > help_width {
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            Rect::new(
                inner.x.saturating_add(help_width),
                inner.y,
                inner.width.saturating_sub(help_width),
                1,
            ),
        );
    }
}

pub fn refresh_controls(area: Rect) -> Option<RefreshControls> {
    if area.height < 3 || area.width < REFRESH_CONTROL_WIDTH + 6 {
        return None;
    }

    let inner_right = area.x + area.width.saturating_sub(1);
    let start_x = inner_right.saturating_sub(REFRESH_CONTROL_WIDTH);
    let y = area.y + 1;

    Some(RefreshControls {
        minus: Rect::new(start_x + 1, y, 5, 1),
        plus: Rect::new(start_x + 17, y, 5, 1),
    })
}

pub fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

fn compact_endpoint(server: &str) -> String {
    server
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

fn button_span(text: &'static str) -> Span<'static> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Black)
            .bg(ORK_GREEN)
            .add_modifier(Modifier::BOLD),
    )
}

fn label_span(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(MUTED))
}

fn value_span(text: &str, color: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(color))
}

fn data_pair(label: &'static str, value: String, color: Color) -> Span<'static> {
    Span::styled(
        format!("  {label} {value}"),
        Style::default().fg(color),
    )
}

fn meter_line(
    label: &'static str,
    percent: f64,
    width: usize,
    color: Color,
    value: String,
    suffix: Vec<Span<'static>>,
) -> Line<'static> {
    let pct = percent.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let mut spans = vec![
        Span::styled(format!(" {label:<3} "), Style::default().fg(MUTED)),
        Span::styled("▪".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "·".repeat(width.saturating_sub(filled)),
            Style::default().fg(PIXEL_OFF),
        ),
        Span::styled(format!(" {value}"), Style::default().fg(WHITE)),
    ];
    spans.extend(suffix);
    Line::from(spans)
}

fn history_lines(
    history: &VecDeque<u64>,
    width: usize,
    height: usize,
    color: Color,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let start = history.len().saturating_sub(width);
    let samples: Vec<u64> = history.iter().skip(start).copied().collect();
    let left_pad = width.saturating_sub(samples.len());

    (0..height)
        .map(|row| {
            let threshold = ((height - row) as f64 / height as f64 * 100.0) as u64;
            let mut spans = Vec::with_capacity(width + 1);
            if left_pad > 0 {
                spans.push(Span::styled(
                    "·".repeat(left_pad),
                    Style::default().fg(PIXEL_OFF),
                ));
            }
            for sample in &samples {
                let sample_color = if *sample >= 98 { RED } else { color };
                if *sample >= threshold {
                    spans.push(Span::styled("▪", Style::default().fg(sample_color)));
                } else {
                    spans.push(Span::styled("·", Style::default().fg(PIXEL_OFF)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn push_history(history: &mut VecDeque<u64>, value: f64) {
    if history.len() >= HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(value.clamp(0.0, 100.0) as u64);
}

fn percent(value: f64, total: f64) -> f64 {
    if total > 0.0 {
        value / total * 100.0
    } else {
        0.0
    }
}

fn gpu_bar_width(width: u16) -> usize {
    match width {
        0..=89 => 16,
        90..=119 => 24,
        _ => 32,
    }
}

fn temperature_color(celsius: f64) -> Color {
    match celsius {
        v if v >= 85.0 => RED,
        v if v >= 78.0 => ORANGE,
        v if v >= 70.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn power_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 94.0 => ORANGE,
        v if v >= 82.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn vram_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 96.0 => ORANGE,
        _ => CYAN,
    }
}

fn context_color(percent: f64) -> Color {
    match percent {
        v if v >= 95.0 => ORANGE,
        _ => ORK_GREEN,
    }
}

fn friendly_llm_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("501") || lower.contains("--metrics") || lower.contains("metrics endpoint") {
        "LLAMA METRICS OFF · restart server with --metrics".to_string()
    } else if lower.contains("cannot reach") || lower.contains("connection") {
        "LLAMA SERVER OFFLINE".to_string()
    } else {
        format!("LLAMA · {error}")
    }
}

fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_buttons_match_rendered_positions() {
        let area = Rect::new(0, 0, 120, 3);
        let controls = refresh_controls(area).unwrap();

        assert!(rect_contains(
            controls.minus,
            controls.minus.x + 2,
            controls.minus.y
        ));
        assert!(rect_contains(
            controls.plus,
            controls.plus.x + 2,
            controls.plus.y
        ));
        assert!(!rect_contains(
            controls.minus,
            controls.plus.x,
            controls.plus.y
        ));
    }

    #[test]
    fn refresh_controls_hide_when_terminal_is_too_narrow() {
        assert!(refresh_controls(Rect::new(0, 0, 20, 3)).is_none());
    }

    #[test]
    fn history_is_bounded() {
        let mut history = VecDeque::new();
        for i in 0..(HISTORY_LEN + 10) {
            push_history(&mut history, i as f64);
        }
        assert_eq!(history.len(), HISTORY_LEN);
    }

    #[test]
    fn endpoint_is_compact() {
        assert_eq!(compact_endpoint("http://127.0.0.1:8081/"), "127.0.0.1:8081");
    }
}