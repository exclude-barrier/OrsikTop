use std::collections::VecDeque;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};

use crate::{gpu::GpuStats, llama::LlmStats};

pub const MIN_REFRESH_MS: u64 = 100;
pub const MAX_REFRESH_MS: u64 = 10_000;
pub const REFRESH_STEP_MS: u64 = 100;

const HISTORY_LEN: usize = 180;
const REFRESH_CONTROL_WIDTH: u16 = 31;

const ORK_GREEN: Color = Color::Rgb(105, 210, 70);
const DIM_GREEN: Color = Color::Rgb(70, 135, 60);
const MUTED: Color = Color::Rgb(145, 150, 145);
const PIXEL_OFF: Color = Color::Rgb(48, 55, 50);
const YELLOW: Color = Color::Rgb(210, 210, 70);
const ORANGE: Color = Color::Rgb(230, 145, 60);
const RED: Color = Color::Rgb(235, 75, 75);
const CYAN: Color = Color::Rgb(70, 195, 220);

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
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            gpu_history: VecDeque::with_capacity(HISTORY_LEN),
            vram_history: VecDeque::with_capacity(HISTORY_LEN),
        }
    }
}

impl UiState {
    pub fn push_gpu_sample(&mut self, gpu: &GpuStats) {
        let vram = if gpu.memory_total_mib > 0.0 {
            gpu.memory_used_mib / gpu.memory_total_mib * 100.0
        } else {
            0.0
        };
        push_history(&mut self.gpu_history, gpu.utilization);
        push_history(&mut self.vram_history, vram);
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

    if area.width < 64 || area.height < 20 {
        draw_too_small(frame, area);
        return;
    }

    let gpu_height = if area.height >= 28 { 13 } else { 9 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(gpu_height),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, rows[0], llm, server, refresh_ms);
    draw_gpu(frame, rows[1], gpu, state);
    draw_llm_and_system(frame, rows[2], system, llm);
    draw_footer(frame, rows[3], llm, gpu, refresh_ms);
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("OrsikTop needs at least 64x20 terminal cells")
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
    let status = if llm.connected { "ONLINE" } else { "OFFLINE" };
    let status_style = if llm.connected {
        Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(RED).add_modifier(Modifier::BOLD)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let controls = refresh_controls(area);
    let control_x = controls
        .map(|c| c.minus.x.saturating_sub(9))
        .unwrap_or(inner.x.saturating_add(inner.width));
    let left_width = control_x.saturating_sub(inner.x);

    if left_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " OrsikTop ",
                    Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
                ),
                Span::raw("— btop for local LLM Orks  "),
                Span::styled(status, status_style),
                Span::styled(format!("  {server}"), Style::default().fg(MUTED)),
            ])),
            Rect::new(inner.x, inner.y, left_width, 1),
        );
    }

    if let Some(c) = controls {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("REFRESH  ", Style::default().fg(MUTED)),
                Span::styled(
                    "[ - ]",
                    Style::default()
                        .fg(Color::Black)
                        .bg(ORK_GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {:>5} ms  ", refresh_ms)),
                Span::styled(
                    "[ + ]",
                    Style::default()
                        .fg(Color::Black)
                        .bg(ORK_GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            Rect::new(
                c.minus.x.saturating_sub(9),
                inner.y,
                REFRESH_CONTROL_WIDTH,
                1,
            ),
        );
    }
}

fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats, state: &UiState) {
    let title = if gpu.available {
        format!(
            " gpu0  {}  {:>4.0} MHz  {:>3.0}°C  {} ",
            gpu.name, gpu.graphics_clock_mhz, gpu.temperature_c, gpu.pstate
        )
    } else {
        " gpu0 — NVIDIA / NVML unavailable ".to_string()
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(32),
            Constraint::Percentage(39),
            Constraint::Percentage(29),
        ])
        .split(inner);

    draw_gpu_history(frame, cols[0], state);
    draw_gpu_core(frame, cols[1], gpu);
    draw_gpu_memory(frame, cols[2], gpu, state);
}

fn draw_gpu_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .title(" GPU LOAD HISTORY ")
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(pixel_history_lines(
            &state.gpu_history,
            inner.width as usize,
            inner.height as usize,
        )),
        inner,
    );
}

fn draw_gpu_core(frame: &mut Frame, area: Rect, gpu: &GpuStats) {
    let block = Block::default()
        .title(" CORE / POWER ")
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 12 || inner.height == 0 {
        return;
    }

    let bar_width = inner.width.saturating_sub(18).max(8) as usize;
    let power_pct = if gpu.power_limit_w > 0.0 {
        gpu.power_w / gpu.power_limit_w * 100.0
    } else {
        0.0
    };

    let mut lines = vec![
        pixel_meter(
            "GPU",
            gpu.utilization,
            bar_width,
            format!("{:>3.0}%", gpu.utilization),
        ),
        pixel_meter(
            "PWR",
            power_pct,
            bar_width,
            format!("{:>3.0}W", gpu.power_w),
        ),
        pixel_meter(
            "ENC",
            gpu.encoder_utilization,
            bar_width,
            format!("{:>3.0}%", gpu.encoder_utilization),
        ),
        pixel_meter(
            "DEC",
            gpu.decoder_utilization,
            bar_width,
            format!("{:>3.0}%", gpu.decoder_utilization),
        ),
        pixel_meter(
            "FAN",
            gpu.fan_percent,
            bar_width,
            format!("{:>3.0}%", gpu.fan_percent),
        ),
        Line::from(vec![
            Span::styled("CORE ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:>5.0} MHz", gpu.graphics_clock_mhz),
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("TEMP ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:>3.0}°C", gpu.temperature_c),
                Style::default().fg(temperature_color(gpu.temperature_c)),
            ),
            Span::styled("   P-STATE ", Style::default().fg(MUTED)),
            Span::styled(
                gpu.pstate.clone(),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("LIMIT ", Style::default().fg(MUTED)),
            Span::raw(format!("{:>5.0} W", gpu.power_limit_w)),
        ]),
    ];

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_gpu_memory(frame: &mut Frame, area: Rect, gpu: &GpuStats, state: &UiState) {
    let block = Block::default().title(" VRAM / BUS ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let vram_pct = if gpu.memory_total_mib > 0.0 {
        gpu.memory_used_mib / gpu.memory_total_mib * 100.0
    } else {
        0.0
    };

    let header_height = 5u16.min(inner.height);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(1)])
        .split(inner);

    let info = vec![
        Line::from(vec![
            Span::styled("VRAM ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{:>4.1}/{:>4.1} GiB",
                    gpu.memory_used_mib / 1024.0,
                    gpu.memory_total_mib / 1024.0
                ),
                Style::default()
                    .fg(heat_color(vram_pct))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {:>3.0}%", vram_pct)),
        ]),
        Line::from(vec![
            Span::styled("VCLK ", Style::default().fg(MUTED)),
            Span::raw(format!("{:>5.0} MHz", gpu.memory_clock_mhz)),
        ]),
        Line::from(vec![
            Span::styled("MEM  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:>3.0}%", gpu.memory_utilization),
                Style::default().fg(heat_color(gpu.memory_utilization)),
            ),
        ]),
        Line::from(vec![
            Span::styled("RX   ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:>7.1} MiB/s", gpu.pcie_rx_mib_s),
                Style::default().fg(CYAN),
            ),
        ]),
        Line::from(vec![
            Span::styled("TX   ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:>7.1} MiB/s", gpu.pcie_tx_mib_s),
                Style::default().fg(CYAN),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(info), parts[0]);

    if parts[1].height > 0 {
        let mut matrix =
            pixel_fill_matrix(vram_pct, parts[1].width as usize, parts[1].height as usize);
        if !state.vram_history.is_empty() && parts[1].height >= 2 {
            let trend = pixel_history_lines(&state.vram_history, parts[1].width as usize, 1);
            if let Some(line) = trend.into_iter().next() {
                let last = matrix.len().saturating_sub(1);
                matrix[last] = line;
            }
        }
        frame.render_widget(Paragraph::new(matrix), parts[1]);
    }
}

fn draw_llm_and_system(frame: &mut Frame, area: Rect, system: &SystemStats, llm: &LlmStats) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
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

    let context_used = if llm.slots_available {
        llm.context_used
    } else {
        llm.context_high_watermark
    };
    let context_ratio = if llm.context_size > 0 {
        context_used as f64 / llm.context_size as f64
    } else {
        0.0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(format!(" MODEL    {}", llm.model)), rows[0]);
    frame.render_widget(
        Paragraph::new(format!(
            " LIVE     prompt {:>8.1} tok/s   generate {:>8.1} tok/s",
            llm.prompt_tps, llm.generation_tps
        )),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            " AVG      prompt {:>8.1} tok/s   generate {:>8.1} tok/s",
            llm.prompt_avg_tps, llm.generation_avg_tps
        )),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(format!(
            " TOKENS   prompt {:>10.0}   generated {:>10.0}",
            llm.prompt_total, llm.generated_total
        )),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(format!(
            " QUEUE    {:.0} active / {:.0} deferred   SLOTS {}/{}   MTP {:.0}%",
            llm.active_requests,
            llm.deferred_requests,
            llm.busy_slots,
            llm.slot_count,
            llm.spec_acceptance_pct
        )),
        rows[4],
    );

    let context_title = if llm.slots_available {
        " CONTEXT / ACTIVE SLOT "
    } else {
        " CONTEXT HIGH-WATERMARK "
    };
    frame.render_widget(
        Gauge::default()
            .block(Block::default().title(context_title).borders(Borders::TOP))
            .gauge_style(Style::default().fg(heat_color(context_ratio * 100.0)))
            .ratio(context_ratio.clamp(0.0, 1.0))
            .label(if llm.context_size > 0 {
                format!(
                    "{} / {} tokens ({:.1}%)",
                    context_used,
                    llm.context_size,
                    context_ratio * 100.0
                )
            } else {
                "waiting for context data".to_string()
            }),
        rows[5],
    );
}

fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(" SYSTEM ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let ram_ratio = if system.memory_total_bytes > 0 {
        system.memory_used_bytes as f64 / system.memory_total_bytes as f64
    } else {
        0.0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio((system.cpu_usage / 100.0).clamp(0.0, 1.0))
            .label(format!("CPU {:>5.1}%", system.cpu_usage)),
        rows[0],
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio(ram_ratio.clamp(0.0, 1.0))
            .label(format!(
                "RAM {:.1} / {:.1} GiB",
                bytes_to_gib(system.memory_used_bytes),
                bytes_to_gib(system.memory_total_bytes)
            )),
        rows[1],
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, llm: &LlmStats, gpu: &GpuStats, refresh_ms: u64) {
    let error = if !llm.error.is_empty() {
        Some(llm.error.as_str())
    } else if !gpu.error.is_empty() {
        Some(gpu.error.as_str())
    } else {
        None
    };

    let footer_text = match error {
        Some(error) => format!(
            " q/Esc quit  |  click [ - ] / [ + ]  |  {refresh_ms} ms  |  {error} "
        ),
        None => format!(
            " q/Esc quit  |  click [ - ] / [ + ] or use -/+  |  {refresh_ms} ms  |  MORE POWER, HAPPIER ORKS "
        ),
    };

    frame.render_widget(
        Paragraph::new(footer_text)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM_GREEN)),
            ),
        area,
    );
}

pub fn refresh_controls(area: Rect) -> Option<RefreshControls> {
    if area.height < 3 || area.width < REFRESH_CONTROL_WIDTH + 4 {
        return None;
    }

    let inner_right = area.x + area.width.saturating_sub(1);
    let start_x = inner_right.saturating_sub(REFRESH_CONTROL_WIDTH);
    let y = area.y + 1;

    Some(RefreshControls {
        minus: Rect::new(start_x + 9, y, 5, 1),
        plus: Rect::new(start_x + 26, y, 5, 1),
    })
}

pub fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

fn push_history(history: &mut VecDeque<u64>, value: f64) {
    if history.len() >= HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(value.clamp(0.0, 100.0) as u64);
}

fn pixel_meter(label: &str, percent: f64, width: usize, value: String) -> Line<'static> {
    let pct = percent.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    Line::from(vec![
        Span::styled(format!("{label:<3} "), Style::default().fg(MUTED)),
        Span::styled("▪".repeat(filled), Style::default().fg(heat_color(pct))),
        Span::styled(
            "·".repeat(width.saturating_sub(filled)),
            Style::default().fg(PIXEL_OFF),
        ),
        Span::styled(format!(" {value}"), Style::default().fg(Color::White)),
    ])
}

fn pixel_history_lines(history: &VecDeque<u64>, width: usize, height: usize) -> Vec<Line<'static>> {
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
                if *sample >= threshold {
                    spans.push(Span::styled(
                        "▪",
                        Style::default().fg(heat_color(*sample as f64)),
                    ));
                } else {
                    spans.push(Span::styled("·", Style::default().fg(PIXEL_OFF)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn pixel_fill_matrix(percent: f64, width: usize, height: usize) -> Vec<Line<'static>> {
    let pct = percent.clamp(0.0, 100.0);
    let total = width.saturating_mul(height).max(1);
    let filled = ((pct / 100.0) * total as f64).round() as usize;
    let color = heat_color(pct);

    (0..height.max(1))
        .map(|row| {
            let row_start = row.saturating_mul(width);
            let row_filled = filled.saturating_sub(row_start).min(width);
            Line::from(vec![
                Span::styled("▪".repeat(row_filled), Style::default().fg(color)),
                Span::styled(
                    "·".repeat(width.saturating_sub(row_filled)),
                    Style::default().fg(PIXEL_OFF),
                ),
            ])
        })
        .collect()
}

fn heat_color(value: f64) -> Color {
    match value {
        v if v >= 90.0 => RED,
        v if v >= 75.0 => ORANGE,
        v if v >= 55.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn temperature_color(celsius: f64) -> Color {
    match celsius {
        v if v >= 85.0 => RED,
        v if v >= 75.0 => ORANGE,
        v if v >= 65.0 => YELLOW,
        _ => ORK_GREEN,
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
}
