use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

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

const HISTORY_WINDOW: Duration = Duration::from_secs(60);
const HISTORY_MAX_SAMPLES: usize = 720;
const REFRESH_CONTROL_WIDTH: u16 = 22;

const BG_BLACK: Color = Color::Rgb(0, 0, 0);
const BAR_EMPTY: Color = Color::Rgb(48, 52, 48);
const ORK_GREEN: Color = Color::Rgb(105, 210, 70);
const DIM_GREEN: Color = Color::Rgb(70, 135, 60);
const INNER_GREEN: Color = Color::Rgb(42, 83, 48);
const MUTED: Color = Color::Rgb(145, 150, 145);
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

#[derive(Copy, Clone, Debug)]
struct TimedSample {
    at: Instant,
    value: u64,
}

pub struct UiState {
    gpu_history: VecDeque<TimedSample>,
    vram_history: VecDeque<TimedSample>,
    cpu_history: VecDeque<TimedSample>,
    ram_history: VecDeque<TimedSample>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            gpu_history: VecDeque::with_capacity(600),
            vram_history: VecDeque::with_capacity(600),
            cpu_history: VecDeque::with_capacity(600),
            ram_history: VecDeque::with_capacity(600),
        }
    }
}

impl UiState {
    pub fn push_sample(&mut self, gpu: &GpuStats, system: &SystemStats) {
        let now = Instant::now();
        let vram = percent(gpu.memory_used_mib, gpu.memory_total_mib);
        let ram = percent(
            system.memory_used_bytes as f64,
            system.memory_total_bytes as f64,
        );

        push_history_at(&mut self.gpu_history, gpu.utilization, now);
        push_history_at(&mut self.vram_history, vram, now);
        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        push_history_at(&mut self.ram_history, ram, now);
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

    frame.render_widget(Block::default().style(Style::default().bg(BG_BLACK)), area);

    if area.width < 72 || area.height < 22 {
        draw_too_small(frame, area);
        return;
    }

    let llm_height = if llm.connected { 9 } else { 5 };
    let history_required = 3 + 7 + llm_height + 5 + 3;
    let show_history = area.height >= history_required;

    let rows = if show_history {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Length(llm_height),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Length(llm_height),
                Constraint::Min(0),
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
        draw_footer(frame, rows[4], llm, gpu, refresh_ms);
    }
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("OrsikTop needs at least 72x22 terminal cells")
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
                Span::styled(
                    format!("  {:>4} ms  ", refresh_ms),
                    Style::default().fg(WHITE),
                ),
                button_span("[ + ]"),
            ])),
            Rect::new(start_x, inner.y, REFRESH_CONTROL_WIDTH, 1),
        );
    }
}

fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats) {
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
            Span::raw(" ".repeat(bar_width + 7)),
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
            fixed_data_pair(
                "TX",
                optional_number(gpu.pcie_tx_mb_s, 1, " MB/s"),
                CYAN,
                18,
            ),
        ]),
        Line::from(vec![
            label_span("        "),
            Span::raw(" ".repeat(bar_width + 7)),
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
    let (mtp, mtp_color) = if llm.spec_enabled {
        (
            llm.spec_n_max
                .filter(|value| *value > 0)
                .map(|value| format!("MTP{value}"))
                .unwrap_or_else(|| "MTP".to_string()),
            CYAN,
        )
    } else {
        ("MTP OFF".to_string(), MUTED)
    };
    let (acc, acc_color) = match llm.spec_acceptance_pct {
        Some(value) => (format!("{value:.0}%"), ORK_GREEN),
        None => ("—".to_string(), MUTED),
    };
    let (phase, phase_color) = llm_phase(llm);
    let live_pp = if llm.prompt_tps > 0.05 {
        format!("{:>7.1} tok/s", llm.prompt_tps)
    } else {
        "      — tok/s".to_string()
    };
    let live_tg = if llm.generation_tps > 0.05 {
        format!("{:>7.1} tok/s", llm.generation_tps)
    } else {
        "      — tok/s".to_string()
    };
    let request_pp = if llm.busy_slots > 0 {
        llm.request_prompt_tokens.to_string()
    } else {
        "—".to_string()
    };
    let request_tg = if llm.busy_slots > 0 {
        llm.request_generated_tokens.to_string()
    } else {
        "—".to_string()
    };
    let cache = llm
        .prompt_cached_total
        .map(|value| format!("{value:.0}"))
        .unwrap_or_else(|| "—".to_string());

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL    "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" STATE    "),
            value_span(phase, phase_color),
            Span::raw("    "),
            label_span("SLOT "),
            value_span(&slots, CYAN),
            Span::raw("    "),
            label_span("QUEUED "),
            value_span(&format!("{:.0}", llm.deferred_requests), WHITE),
            Span::raw("    "),
            value_span(&mtp, mtp_color),
            Span::raw("    "),
            label_span("ACC "),
            value_span(&acc, acc_color),
        ]),
        Line::from(vec![
            label_span(" LIVE     "),
            Span::styled(format!("PP {live_pp}"), Style::default().fg(CYAN)),
            Span::raw("    "),
            Span::styled(
                format!("TG {live_tg}"),
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" SERVER   "),
            Span::styled(
                format!("PP {:>7.1} tok/s", llm.prompt_avg_tps),
                Style::default().fg(MUTED),
            ),
            Span::raw("    "),
            Span::styled(
                format!("TG {:>7.1} tok/s", llm.generation_avg_tps),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            label_span(" REQUEST  "),
            value_span(&format!("PP {request_pp}"), WHITE),
            Span::raw("    "),
            value_span(&format!("TG {request_tg}"), WHITE),
        ]),
        Line::from(vec![
            label_span(" TOTAL    "),
            value_span(&format!("PP {:.0}", llm.prompt_total), WHITE),
            Span::raw("    "),
            value_span(&format!("TG {:.0}", llm.generated_total), WHITE),
            Span::raw("    "),
            label_span("CACHE "),
            value_span(&cache, CYAN),
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

fn llm_phase(llm: &LlmStats) -> (&'static str, Color) {
    if llm.generation_tps > 0.05 {
        ("GENERATING", ORK_GREEN)
    } else if llm.prompt_tps > 0.05 {
        ("PREFILL", CYAN)
    } else if llm.busy_slots > 0 || llm.active_requests > 0.0 {
        ("PROCESSING", YELLOW)
    } else if llm.deferred_requests > 0.0 {
        ("QUEUED", YELLOW)
    } else {
        ("IDLE", MUTED)
    }
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
}

fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .title(" HISTORY · 60 s ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 32 || inner.height < 3 {
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
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
) {
    let block = if right_border {
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(INNER_GREEN))
    } else {
        Block::default()
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height < 2 {
        return;
    }

    let latest = history.back().map(|sample| sample.value).unwrap_or(0);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {title:<5}"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>3}%", latest),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(trend_lines(
            history,
            rows[1].width as usize,
            rows[1].height as usize,
            color,
        )),
        rows[1],
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
            Paragraph::new(status).style(
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
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

fn fixed_data_pair(
    label: &'static str,
    value: String,
    color: Color,
    width: usize,
) -> Span<'static> {
    let text = format!("  {label:<7} {value}");
    Span::styled(format!("{text:<width$}"), Style::default().fg(color))
}

fn meter_line(
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

fn trend_lines(
    history: &VecDeque<TimedSample>,
    width: usize,
    height: usize,
    color: Color,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let now = Instant::now();
    let columns = resample_history(history, width * 2, now);
    let sub_height = height * 4;

    (0..height)
        .map(|cell_y| {
            let mut row = String::with_capacity(width);

            for cell_x in 0..width {
                let mut mask = 0u8;

                for dot_x in 0..2 {
                    let sample = columns[cell_x * 2 + dot_x];
                    let Some(sample) = sample else {
                        continue;
                    };

                    let mut filled = ((sample as f64 / 100.0) * sub_height as f64).round() as usize;
                    if sample > 0 && filled == 0 {
                        filled = 1;
                    }
                    filled = filled.min(sub_height);
                    let start = sub_height.saturating_sub(filled);

                    for dot_y in 0..4 {
                        let sub_y = cell_y * 4 + dot_y;
                        if sub_y >= start {
                            mask |= braille_bit(dot_x, dot_y);
                        }
                    }
                }

                row.push(braille_char(mask));
            }

            Line::from(Span::styled(row, Style::default().fg(color)))
        })
        .collect()
}

fn resample_history(
    history: &VecDeque<TimedSample>,
    columns: usize,
    now: Instant,
) -> Vec<Option<u64>> {
    let columns = columns.max(1);
    let window_secs = HISTORY_WINDOW.as_secs_f64();
    let mut points = history
        .iter()
        .filter_map(|sample| {
            let age = now.saturating_duration_since(sample.at);
            (age <= HISTORY_WINDOW)
                .then_some((window_secs - age.as_secs_f64(), sample.value.min(100)))
        })
        .peekable();

    let mut result = vec![None; columns];
    let mut current = None;

    for (x, value) in result.iter_mut().enumerate() {
        let target = if columns == 1 {
            window_secs
        } else {
            x as f64 / (columns - 1) as f64 * window_secs
        };

        while let Some(&(offset, sample)) = points.peek() {
            if offset > target {
                break;
            }
            current = Some(sample);
            points.next();
        }

        *value = current;
    }

    result
}

fn braille_bit(x: usize, y: usize) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

fn braille_char(mask: u8) -> char {
    if mask == 0 {
        ' '
    } else {
        char::from_u32(0x2800 + mask as u32).unwrap_or(' ')
    }
}

fn push_history_at(history: &mut VecDeque<TimedSample>, value: f64, now: Instant) {
    history.push_back(TimedSample {
        at: now,
        value: clamp_percent(value) as u64,
    });

    while history
        .front()
        .is_some_and(|sample| now.saturating_duration_since(sample.at) > HISTORY_WINDOW)
    {
        history.pop_front();
    }
    while history.len() > HISTORY_MAX_SAMPLES {
        history.pop_front();
    }
}

fn percent(value: f64, total: f64) -> f64 {
    if value.is_finite() && total.is_finite() && total > 0.0 {
        clamp_percent(value / total * 100.0)
    } else {
        0.0
    }
}

fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn gpu_bar_width(width: u16) -> usize {
    match width {
        0..=89 => 14,
        90..=119 => 22,
        _ => 30,
    }
}

fn temperature_color(celsius: f64) -> Color {
    match celsius {
        v if v >= 85.0 => RED,
        v if v >= 80.0 => ORANGE,
        v if v >= 70.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn power_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 95.0 => ORANGE,
        v if v >= 85.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn vram_color(percent: f64) -> Color {
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
    } else if reason.contains("thermal") || reason.contains("power-brake") || reason.contains("hw")
    {
        RED
    } else if reason.contains("power") {
        ORANGE
    } else {
        YELLOW
    }
}

fn context_color(percent: f64) -> Color {
    match percent {
        v if v >= 97.0 => RED,
        v if v >= 90.0 => ORANGE,
        v if v >= 80.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

fn optional_number(value: Option<f64>, decimals: usize, suffix: &str) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.decimals$}{suffix}"),
        _ => "—".to_string(),
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
    fn history_uses_a_fixed_time_window() {
        let now = Instant::now();
        let mut history = VecDeque::new();
        push_history_at(
            &mut history,
            10.0,
            now - HISTORY_WINDOW - Duration::from_secs(1),
        );
        push_history_at(&mut history, 20.0, now);
        assert_eq!(history.len(), 1);
        assert_eq!(history.back().unwrap().value, 20);
    }

    #[test]
    fn endpoint_is_compact() {
        assert_eq!(compact_endpoint("http://127.0.0.1:8081/"), "127.0.0.1:8081");
    }

    #[test]
    fn trend_plot_keeps_requested_dimensions() {
        let now = Instant::now();
        let history = VecDeque::from([
            TimedSample {
                at: now - Duration::from_secs(30),
                value: 25,
            },
            TimedSample {
                at: now,
                value: 100,
            },
        ]);
        let lines = trend_lines(&history, 5, 3, ORK_GREEN);
        let rendered = lines.iter().map(|line| line.width()).collect::<Vec<_>>();
        assert_eq!(rendered, vec![5, 5, 5]);
    }

    #[test]
    fn history_resampling_holds_the_latest_value_between_samples() {
        let now = Instant::now();
        let history = VecDeque::from([
            TimedSample {
                at: now - Duration::from_secs(30),
                value: 25,
            },
            TimedSample {
                at: now - Duration::from_secs(10),
                value: 50,
            },
        ]);

        let columns = resample_history(&history, 7, now);
        assert_eq!(
            columns,
            vec![None, None, None, Some(25), Some(25), Some(50), Some(50)]
        );
    }

    #[test]
    fn braille_masks_map_to_expected_cells() {
        assert_eq!(braille_char(0), ' ');
        assert_eq!(braille_char(0xc0), '⣀');
        assert_eq!(braille_char(0xff), '⣿');
    }

    #[test]
    fn fine_bar_has_subcell_resolution() {
        assert_eq!(fine_bar(0.0, 4), ("".to_string(), "⣀⣀⣀⣀".to_string()));
        assert_eq!(
            fine_bar(10.0, 10),
            ("⣿".to_string(), "⣀⣀⣀⣀⣀⣀⣀⣀⣀".to_string())
        );
        assert_eq!(fine_bar(25.0, 2), ("⣇".to_string(), "⣀".to_string()));
        assert_eq!(fine_bar(50.0, 2), ("⣿".to_string(), "⣀".to_string()));
        assert_eq!(fine_bar(100.0, 2), ("⣿⣿".to_string(), "".to_string()));
    }

    #[test]
    fn nan_percent_is_safely_clamped() {
        assert_eq!(clamp_percent(f64::NAN), 0.0);
    }
}
