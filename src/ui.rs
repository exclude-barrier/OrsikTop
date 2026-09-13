use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    cpu::{CpuCoreKind, CpuPhysicalCore, CpuTopology, CpuVendor},
    gpu::GpuStats,
    llama::LlmStats,
};

pub const MIN_REFRESH_MS: u64 = 100;
pub const MAX_REFRESH_MS: u64 = 10_000;
pub const REFRESH_STEP_MS: u64 = 100;

const HISTORY_WINDOW: Duration = Duration::from_secs(60);
const HISTORY_MAX_SAMPLES: usize = 720;
const REFRESH_CONTROL_WIDTH: u16 = 22;
const PROCESS_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

const BG_BLACK: Color = Color::Rgb(0, 0, 0);
const BAR_EMPTY: Color = Color::Rgb(48, 52, 48);
const ORK_GREEN: Color = Color::Rgb(105, 210, 70);
const BRIGHT_GREEN: Color = Color::Rgb(150, 235, 95);
const DIM_GREEN: Color = Color::Rgb(70, 135, 60);
const INNER_GREEN: Color = Color::Rgb(42, 83, 48);
const MUTED: Color = Color::Rgb(145, 150, 145);
const YELLOW: Color = Color::Rgb(220, 205, 75);
const ORANGE: Color = Color::Rgb(230, 145, 60);
const RED: Color = Color::Rgb(235, 75, 75);
const CYAN: Color = Color::Rgb(70, 195, 220);
const WHITE: Color = Color::Rgb(225, 225, 225);
const PROCESS_SELECTED_BG: Color = Color::Rgb(24, 54, 24);

#[derive(Clone, Debug, Default)]
pub struct ProcessStats {
    pub pid: u32,
    pub program: String,
    pub command: String,
    pub cpu_pct: f64,
    pub memory_bytes: u64,
    pub threads: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub per_cpu_usage: Vec<f64>,
    pub cpu_topology: CpuTopology,
    pub cpu_frequency_mhz: Option<f64>,
    pub cpu_temperature_c: Option<f64>,
    pub io_wait_pct: Option<f64>,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub processes: Vec<ProcessStats>,
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProcessSortKey {
    Pid,
    Program,
    Cpu,
    Memory,
    Threads,
}

#[derive(Copy, Clone, Debug)]
struct ProcessHeaderHit {
    rect: Rect,
    key: ProcessSortKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessRowTarget {
    Process(u32),
    Group(String),
}

#[derive(Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    targets: Vec<ProcessRowTarget>,
}

pub struct UiState {
    gpu_history: VecDeque<TimedSample>,
    vram_history: VecDeque<TimedSample>,
    cpu_history: VecDeque<TimedSample>,
    ram_history: VecDeque<TimedSample>,
    process_selected_pid: Option<u32>,
    process_pinned_pid: Option<u32>,
    last_process_click: Option<(u32, Instant)>,
    process_selected_group: Option<String>,
    process_pinned_group: Option<String>,
    last_process_group_click: Option<(String, Instant)>,
    expanded_process_groups: HashSet<String>,
    process_scroll: usize,
    process_display_total: usize,
    process_sort_key: ProcessSortKey,
    process_sort_desc: bool,
    process_header_hits: Vec<ProcessHeaderHit>,
    process_pane: Option<Rect>,
    process_rows: Option<ProcessRowsHit>,
    help_open: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            gpu_history: VecDeque::with_capacity(600),
            vram_history: VecDeque::with_capacity(600),
            cpu_history: VecDeque::with_capacity(600),
            ram_history: VecDeque::with_capacity(600),
            process_selected_pid: None,
            process_pinned_pid: None,
            last_process_click: None,
            process_selected_group: None,
            process_pinned_group: None,
            last_process_group_click: None,
            expanded_process_groups: HashSet::new(),
            process_scroll: 0,
            process_display_total: 0,
            process_sort_key: ProcessSortKey::Cpu,
            process_sort_desc: true,
            process_header_hits: Vec::new(),
            process_pane: None,
            process_rows: None,
            help_open: false,
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

    pub fn move_process_selection(&mut self, delta: isize, processes: &[ProcessStats]) {
        let targets = self.process_navigation_targets(processes);
        if targets.is_empty() {
            self.process_selected_pid = None;
            self.process_selected_group = None;
            self.process_scroll = 0;
            return;
        }

        let current = self
            .current_process_target()
            .and_then(|selected| targets.iter().position(|target| *target == selected))
            .unwrap_or_else(|| self.process_scroll.min(targets.len() - 1));
        let target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(targets.len() - 1)
        };
        self.select_process_target(&targets[target]);
    }

    pub fn process_home(&mut self, processes: &[ProcessStats]) {
        let targets = self.process_navigation_targets(processes);
        if let Some(target) = targets.first() {
            self.select_process_target(target);
        } else {
            self.process_selected_pid = None;
            self.process_selected_group = None;
        }
        self.process_scroll = 0;
    }

    pub fn process_end(&mut self, processes: &[ProcessStats]) {
        let targets = self.process_navigation_targets(processes);
        if let Some(target) = targets.last() {
            self.select_process_target(target);
        } else {
            self.process_selected_pid = None;
            self.process_selected_group = None;
        }
        self.process_scroll = targets.len().saturating_sub(1);
    }

    fn current_process_target(&self) -> Option<ProcessRowTarget> {
        if let Some(pid) = self.process_selected_pid {
            Some(ProcessRowTarget::Process(pid))
        } else {
            self.process_selected_group
                .as_ref()
                .map(|program| ProcessRowTarget::Group(program.clone()))
        }
    }

    fn select_process_target(&mut self, target: &ProcessRowTarget) {
        match target {
            ProcessRowTarget::Process(pid) => {
                self.process_selected_pid = Some(*pid);
                self.process_selected_group = None;
            }
            ProcessRowTarget::Group(program) => {
                self.process_selected_pid = None;
                self.process_selected_group = Some(program.clone());
            }
        }
    }

    fn process_navigation_targets(&self, processes: &[ProcessStats]) -> Vec<ProcessRowTarget> {
        let (pinned, mut rows) = grouped_process_rows(
            processes,
            self.process_sort_key,
            self.process_sort_desc,
            self.process_pinned_pid,
            &self.expanded_process_groups,
        );
        let pinned_group_rows =
            take_pinned_group_rows(&mut rows, self.process_pinned_group.as_deref());

        let mut targets = Vec::with_capacity(
            rows.len() + pinned_group_rows.len() + usize::from(pinned.is_some()),
        );
        if let Some(process) = pinned {
            targets.push(ProcessRowTarget::Process(process.pid));
        }
        targets.extend(pinned_group_rows.iter().map(process_row_target));
        targets.extend(rows.iter().map(process_row_target));
        targets
    }

    pub fn clamp_process_selection(&mut self, processes: &[ProcessStats]) {
        if self
            .process_selected_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_selected_pid = None;
        }
        if self
            .process_pinned_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_pinned_pid = None;
        }
        let programs = processes
            .iter()
            .map(|process| process.program.as_str())
            .collect::<HashSet<_>>();
        if self
            .process_selected_group
            .as_ref()
            .is_some_and(|program| !programs.contains(program.as_str()))
        {
            self.process_selected_group = None;
        }
        if self
            .process_pinned_group
            .as_ref()
            .is_some_and(|program| !programs.contains(program.as_str()))
        {
            self.process_pinned_group = None;
        }
        self.expanded_process_groups
            .retain(|program| programs.contains(program.as_str()));
        self.process_scroll = self
            .process_scroll
            .min(self.process_display_total.saturating_sub(1));
    }

    pub fn scroll_processes(&mut self, delta: isize) {
        let total = self.process_display_total;
        if total == 0 {
            self.process_scroll = 0;
            return;
        }
        self.process_scroll = if delta.is_negative() {
            self.process_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.process_scroll
                .saturating_add(delta as usize)
                .min(total.saturating_sub(1))
        };
    }

    pub fn clear_process_selection(&mut self) {
        self.process_selected_pid = None;
        self.process_pinned_pid = None;
        self.last_process_click = None;
        self.process_selected_group = None;
        self.process_pinned_group = None;
        self.last_process_group_click = None;
    }

    pub fn process_pane_contains(&self, x: u16, y: u16) -> bool {
        self.process_pane
            .is_some_and(|pane| rect_contains(pane, x, y))
    }

    pub fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
    }

    pub fn close_help(&mut self) {
        self.help_open = false;
    }

    pub fn is_help_open(&self) -> bool {
        self.help_open
    }

    pub fn click_process_sort(&mut self, x: u16, y: u16) -> bool {
        let Some(key) = self
            .process_header_hits
            .iter()
            .find(|hit| rect_contains(hit.rect, x, y))
            .map(|hit| hit.key)
        else {
            return false;
        };

        if self.process_sort_key == key {
            self.process_sort_desc = !self.process_sort_desc;
        } else {
            self.process_sort_key = key;
            self.process_sort_desc = matches!(
                key,
                ProcessSortKey::Cpu | ProcessSortKey::Memory | ProcessSortKey::Threads
            );
        }
        if self.process_selected_pid.is_none() {
            self.process_scroll = 0;
        }
        true
    }

    pub fn click_process_row(&mut self, x: u16, y: u16) -> bool {
        let Some(rows) = self.process_rows.as_ref() else {
            return false;
        };
        if !rect_contains(rows.rect, x, y) {
            return false;
        }
        let row = y.saturating_sub(rows.rect.y) as usize;
        let Some(target) = rows.targets.get(row).cloned() else {
            return false;
        };

        let now = Instant::now();
        match target {
            ProcessRowTarget::Process(pid) => {
                let is_double_click = self.last_process_click.is_some_and(|(last_pid, at)| {
                    last_pid == pid
                        && now.saturating_duration_since(at) <= PROCESS_DOUBLE_CLICK_WINDOW
                });

                self.process_selected_group = None;
                self.process_pinned_group = None;
                self.last_process_group_click = None;
                self.process_selected_pid = Some(pid);
                if is_double_click {
                    self.process_pinned_pid = Some(pid);
                    self.last_process_click = None;
                } else {
                    if self.process_pinned_pid.is_some() && self.process_pinned_pid != Some(pid) {
                        self.process_pinned_pid = None;
                    }
                    self.last_process_click = Some((pid, now));
                }
            }
            ProcessRowTarget::Group(program) => {
                let is_double_click =
                    self.last_process_group_click
                        .as_ref()
                        .is_some_and(|(last_program, at)| {
                            last_program == &program
                                && now.saturating_duration_since(*at) <= PROCESS_DOUBLE_CLICK_WINDOW
                        });

                self.process_selected_pid = None;
                self.process_pinned_pid = None;
                self.last_process_click = None;
                self.process_selected_group = Some(program.clone());
                if is_double_click {
                    self.process_pinned_group = Some(program);
                    self.last_process_group_click = None;
                } else {
                    if self.process_pinned_group.as_deref() != Some(program.as_str()) {
                        self.process_pinned_group = None;
                    }
                    self.last_process_group_click = Some((program, now));
                }
            }
        }
        true
    }

    pub fn right_click_process_group(&mut self, x: u16, y: u16) -> bool {
        let Some(rows) = self.process_rows.as_ref() else {
            return false;
        };
        if !rect_contains(rows.rect, x, y) {
            return false;
        }
        let row = y.saturating_sub(rows.rect.y) as usize;
        let Some(target) = rows.targets.get(row).cloned() else {
            return false;
        };
        let ProcessRowTarget::Group(program) = target else {
            return false;
        };

        if !self.expanded_process_groups.insert(program.clone()) {
            self.expanded_process_groups.remove(&program);
        }
        true
    }

    fn clear_process_interaction(&mut self) {
        self.process_header_hits.clear();
        self.process_pane = None;
        self.process_rows = None;
    }
}

pub fn draw(
    frame: &mut Frame,
    system: &SystemStats,
    llm: &LlmStats,
    gpu: &GpuStats,
    state: &mut UiState,
    server: &str,
    refresh_ms: u64,
) {
    let area = frame.area();
    state.clear_process_interaction();

    frame.render_widget(Block::default().style(Style::default().bg(BG_BLACK)), area);

    if area.width < 72 || area.height < 22 {
        draw_too_small(frame, area);
        return;
    }

    let llm_height = if llm.connected { 12 } else { 5 };
    let system_height = system_panel_height(system);
    let middle_height = llm_height.max(system_height);
    let history_required = 3 + 7 + middle_height + 5 + 3;
    let show_history = area.height >= history_required;

    let rows = if show_history {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Length(middle_height),
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
                Constraint::Length(middle_height),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area)
    };

    draw_header(frame, rows[0], llm, server, refresh_ms);
    draw_gpu(frame, rows[1], gpu);
    draw_llm_and_system(frame, rows[2], system, llm);

    if show_history {
        draw_bottom(frame, rows[3], state, &system.processes);
        draw_footer(frame, rows[4], llm, gpu);
    } else {
        draw_footer(frame, rows[4], llm, gpu);
    }

    if state.help_open {
        draw_help_popup(frame, area);
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
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
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
    let (mtp, mtp_color) = if llm.spec_is_mtp {
        (
            llm.spec_n_max
                .filter(|value| *value > 0)
                .map(|value| format!("MTP{value}"))
                .unwrap_or_else(|| "MTP".to_string()),
            CYAN,
        )
    } else if llm.spec_enabled {
        ("SPEC".to_string(), CYAN)
    } else {
        ("MTP OFF".to_string(), MUTED)
    };
    let (acc, acc_color) = match llm.spec_acceptance_pct {
        Some(value) => (format!("{value:.0}%"), ORK_GREEN),
        None => ("—".to_string(), MUTED),
    };
    let (phase, phase_color) = llm_phase(llm);

    let live_pp = if llm.prompt_tps > 0.05 {
        format!("{:.1} tok/s", llm.prompt_tps)
    } else {
        "— tok/s".to_string()
    };
    let live_tg = if llm.generation_tps > 0.05 {
        format!("{:.1} tok/s", llm.generation_tps)
    } else {
        "— tok/s".to_string()
    };
    let request_pp = if llm.busy_slots > 0 {
        format!("{} tok", grouped_u64(llm.request_prompt_tokens))
    } else {
        "— tok".to_string()
    };
    let request_tg = if llm.busy_slots > 0 {
        format!("{} tok", grouped_u64(llm.request_generated_tokens))
    } else {
        "— tok".to_string()
    };
    let total_pp = format!("{} tok", grouped_f64(llm.prompt_total));
    let total_tg = format!("{} tok", grouped_f64(llm.generated_total));
    let cache_available = llm.prompt_cached_total.is_some();
    let cache = llm
        .prompt_cached_total
        .map(grouped_f64)
        .unwrap_or_else(|| "—".to_string());
    let cache_color = if cache_available { CYAN } else { MUTED };

    let pp_active = llm.prompt_tps > 0.05;
    let tg_active = llm.generation_tps > 0.05;
    let pp_header_color = if pp_active { CYAN } else { MUTED };
    let tg_header_color = if tg_active { ORK_GREEN } else { MUTED };
    let pp_live_color = if pp_active { CYAN } else { MUTED };
    let tg_live_color = if tg_active { ORK_GREEN } else { MUTED };
    let request_color = if llm.busy_slots > 0 { WHITE } else { MUTED };

    let metric_width = ((inner.width as usize).saturating_sub(12) / 2).clamp(16, 30);
    let mut state = vec![
        label_span(" STATE      "),
        value_span(phase, phase_color),
        llm_sep(),
        label_span("SLOT "),
        value_span(&slots, CYAN),
        llm_sep(),
        label_span("QUEUE "),
        value_span(&format!("{:.0}", llm.deferred_requests), WHITE),
        llm_sep(),
        value_span(&mtp, mtp_color),
        llm_sep(),
        label_span("ACC "),
        value_span(&acc, acc_color),
    ];
    if inner.width >= 82 {
        state.push(llm_sep());
        state.push(label_span("CACHE "));
        state.push(value_span(&cache, cache_color));
    }
    if inner.width < 66 {
        state = vec![
            label_span(" STATE      "),
            value_span(phase, phase_color),
            Span::raw("  "),
            label_span("SLOT "),
            value_span(&slots, CYAN),
            Span::raw("  "),
            value_span(&mtp, mtp_color),
        ];
    }

    let total_line = vec![
        label_span(" TOTAL      "),
        llm_metric_cell(&total_pp, metric_width, WHITE, false),
        llm_metric_cell(&total_tg, metric_width, WHITE, false),
    ];

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL      "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(state),
        Line::from(vec![
            label_span("            "),
            llm_metric_cell("PREFILL / PP", metric_width, pp_header_color, pp_active),
            llm_metric_cell("DECODE / TG", metric_width, tg_header_color, tg_active),
        ]),
        Line::from(vec![
            label_span(" LIVE       "),
            llm_metric_cell(&live_pp, metric_width, pp_live_color, pp_active),
            llm_metric_cell(&live_tg, metric_width, tg_live_color, tg_active),
        ]),
        Line::from(vec![
            label_span(" SERVER AVG "),
            llm_metric_cell(
                &format!("{:.1} tok/s", llm.prompt_avg_tps),
                metric_width,
                MUTED,
                false,
            ),
            llm_metric_cell(
                &format!("{:.1} tok/s", llm.generation_avg_tps),
                metric_width,
                MUTED,
                false,
            ),
        ]),
        Line::from(vec![
            label_span(" REQUEST    "),
            llm_metric_cell(&request_pp, metric_width, request_color, false),
            llm_metric_cell(&request_tg, metric_width, request_color, false),
        ]),
        Line::from(total_line),
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            vec![Span::styled(
                if llm.context_size > 0 {
                    format!(
                        " {} / {} tok",
                        grouped_u64(context_used),
                        grouped_u64(llm.context_size)
                    )
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

fn llm_metric_cell(text: &str, width: usize, color: Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(format!("{text:<width$}"), style)
}

fn llm_sep() -> Span<'static> {
    Span::styled("  │  ", Style::default().fg(INNER_GREEN))
}

fn grouped_u64(value: u64) -> String {
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

fn grouped_f64(value: f64) -> String {
    if value.is_finite() && value >= 0.0 {
        grouped_u64(value.round() as u64)
    } else {
        "—".to_string()
    }
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

fn system_panel_height(system: &SystemStats) -> u16 {
    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {
        let p = system
            .cpu_topology
            .physical_core_groups
            .iter()
            .filter(|core| core.kind == CpuCoreKind::Performance)
            .count();
        let e = system
            .cpu_topology
            .physical_core_groups
            .iter()
            .filter(|core| core.kind == CpuCoreKind::Efficiency)
            .count();
        let rows = p.max(e).max(1);
        return (rows as u16 + 9).max(12);
    }
    12
}

fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
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

    if inner.width < 35 || inner.height < 15 {
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

    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {
        lines.extend(physical_core_minibar_rows(
            &system.per_cpu_usage,
            &system.cpu_topology.physical_core_groups,
            inner.width,
        ));
    } else {
        lines.extend(core_heatmap_rows(
            &system.per_cpu_usage,
            &system.cpu_topology.core_kinds,
            inner.width,
            busiest,
            system.cpu_topology.is_hybrid(),
            4,
        ));
    }

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

fn truncate_title(title: &str, max_len: usize) -> String {
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

fn physical_core_minibar_rows(
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
            let usage = physical_core_average(core, usages);
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

fn core_heatmap_rows(
    usages: &[f64],
    kinds: &[CpuCoreKind],
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
        spans.push(Span::styled(
            core_usage_glyph(usage).to_string(),
            heat_style,
        ));
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

fn core_usage_color(usage: f64) -> Color {
    match clamp_percent(usage) {
        value if value >= 90.0 => ORANGE,
        value if value >= 75.0 => YELLOW,
        value if value >= 50.0 => BRIGHT_GREEN,
        value if value >= 25.0 => ORK_GREEN,
        value if value >= 2.0 => DIM_GREEN,
        _ => BAR_EMPTY,
    }
}

fn core_usage_glyph(usage: f64) -> char {
    match clamp_percent(usage) {
        value if value >= 75.0 => '█',
        value if value >= 50.0 => '▓',
        value if value >= 25.0 => '▒',
        value if value >= 2.0 => '░',
        _ => '·',
    }
}

fn draw_bottom(frame: &mut Frame, area: Rect, state: &mut UiState, processes: &[ProcessStats]) {
    // Keep very narrow terminals useful instead of crushing both panes.
    if area.width < 100 {
        draw_history(frame, area, state);
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);

    draw_history(frame, panes[0], state);
    draw_processes(frame, panes[1], processes, state);
}

fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .title(" HISTORY · 60 s ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 32 || inner.height < 5 {
        return;
    }

    // A 2x2 grid gives time-series data horizontal resolution while leaving the
    // right side of the dashboard available for the btop-style process table.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    draw_history_column(
        frame,
        top[0],
        "GPU",
        &state.gpu_history,
        ORK_GREEN,
        true,
        true,
    );
    draw_history_column(
        frame,
        top[1],
        "VRAM",
        &state.vram_history,
        CYAN,
        false,
        true,
    );
    draw_history_column(
        frame,
        bottom[0],
        "CPU",
        &state.cpu_history,
        ORK_GREEN,
        true,
        false,
    );
    draw_history_column(
        frame,
        bottom[1],
        "RAM",
        &state.ram_history,
        CYAN,
        false,
        false,
    );
}

fn draw_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
    bottom_border: bool,
) {
    let borders = match (right_border, bottom_border) {
        (true, true) => Borders::RIGHT | Borders::BOTTOM,
        (true, false) => Borders::RIGHT,
        (false, true) => Borders::BOTTOM,
        (false, false) => Borders::NONE,
    };
    let block = Block::default()
        .borders(borders)
        .border_style(Style::default().fg(INNER_GREEN));
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

#[derive(Clone, Debug)]
struct ProcessGroup<'a> {
    program: &'a str,
    members: Vec<&'a ProcessStats>,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: usize,
    min_pid: u32,
}

#[derive(Clone, Debug)]
enum ProcessDisplayRow<'a> {
    Process {
        process: &'a ProcessStats,
        child: bool,
    },
    Group {
        program: &'a str,
        count: usize,
        cpu_pct: f64,
        memory_bytes: u64,
        threads: usize,
        expanded: bool,
    },
}

fn process_row_target(row: &ProcessDisplayRow<'_>) -> ProcessRowTarget {
    match row {
        ProcessDisplayRow::Process { process, .. } => ProcessRowTarget::Process(process.pid),
        ProcessDisplayRow::Group { program, .. } => ProcessRowTarget::Group((*program).to_string()),
    }
}

fn grouped_process_rows<'a>(
    processes: &'a [ProcessStats],
    key: ProcessSortKey,
    descending: bool,
    pinned_pid: Option<u32>,
    expanded_groups: &HashSet<String>,
) -> (Option<&'a ProcessStats>, Vec<ProcessDisplayRow<'a>>) {
    let (pinned, unpinned) = sorted_processes_with_pin(processes, key, descending, pinned_pid);
    let mut by_program = HashMap::<&str, Vec<&ProcessStats>>::new();
    for process in unpinned {
        by_program
            .entry(process.program.as_str())
            .or_default()
            .push(process);
    }

    let mut groups = by_program
        .into_iter()
        .map(|(program, members)| ProcessGroup {
            program,
            cpu_pct: members.iter().map(|process| process.cpu_pct).sum(),
            memory_bytes: members.iter().map(|process| process.memory_bytes).sum(),
            threads: members.iter().map(|process| process.threads).sum(),
            min_pid: members.iter().map(|process| process.pid).min().unwrap_or(0),
            members,
        })
        .collect::<Vec<_>>();

    groups.sort_by(|a, b| {
        let ordering = match key {
            ProcessSortKey::Pid => a.min_pid.cmp(&b.min_pid),
            ProcessSortKey::Program => a.program.cmp(b.program),
            ProcessSortKey::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
            ProcessSortKey::Memory => a.memory_bytes.cmp(&b.memory_bytes),
            ProcessSortKey::Threads => a.threads.cmp(&b.threads),
        };
        let ordering = if descending {
            ordering.reverse()
        } else {
            ordering
        };
        ordering
            .then_with(|| a.program.cmp(b.program))
            .then_with(|| a.min_pid.cmp(&b.min_pid))
    });

    let mut rows = Vec::new();
    for group in groups {
        if group.members.len() == 1 {
            rows.push(ProcessDisplayRow::Process {
                process: group.members[0],
                child: false,
            });
            continue;
        }

        let expanded = expanded_groups.contains(group.program);
        rows.push(ProcessDisplayRow::Group {
            program: group.program,
            count: group.members.len(),
            cpu_pct: group.cpu_pct,
            memory_bytes: group.memory_bytes,
            threads: group.threads,
            expanded,
        });
        if expanded {
            rows.extend(
                group
                    .members
                    .into_iter()
                    .map(|process| ProcessDisplayRow::Process {
                        process,
                        child: true,
                    }),
            );
        }
    }

    (pinned, rows)
}

fn take_pinned_group_rows<'a>(
    rows: &mut Vec<ProcessDisplayRow<'a>>,
    pinned_group: Option<&str>,
) -> Vec<ProcessDisplayRow<'a>> {
    let Some(group_name) = pinned_group else {
        return Vec::new();
    };
    let Some(index) = rows.iter().position(|row| {
        matches!(
            row,
            ProcessDisplayRow::Group { program, .. } if *program == group_name
        )
    }) else {
        return Vec::new();
    };

    let mut pinned = vec![rows.remove(index)];
    while index < rows.len() {
        let is_child = matches!(
            rows.get(index),
            Some(ProcessDisplayRow::Process {
                process,
                child: true,
            }) if process.program == group_name
        );
        if !is_child {
            break;
        }
        pinned.push(rows.remove(index));
    }
    pinned
}

fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats], state: &mut UiState) {
    state.process_pane = Some(area);
    let (pinned, mut rows) = grouped_process_rows(
        processes,
        state.process_sort_key,
        state.process_sort_desc,
        state.process_pinned_pid,
        &state.expanded_process_groups,
    );
    if state.process_pinned_pid.is_some() && pinned.is_none() {
        state.process_pinned_pid = None;
    }
    let pinned_group_name = state.process_pinned_group.clone();
    let pinned_group_rows = take_pinned_group_rows(&mut rows, pinned_group_name.as_deref());
    if pinned_group_name.is_some() && pinned_group_rows.is_empty() {
        state.process_pinned_group = None;
    }
    state.process_display_total = rows.len();

    let visible = area.height.saturating_sub(3) as usize;
    let pinned_rows =
        usize::from(pinned.is_some() && visible > 0).saturating_add(pinned_group_rows.len());
    let scroll_visible = visible.saturating_sub(pinned_rows);
    let max_start = rows.len().saturating_sub(scroll_visible);
    let start = state.process_scroll.min(max_start);
    state.process_scroll = start;
    let end = start.saturating_add(scroll_visible).min(rows.len());

    let sort_name = process_sort_name(state.process_sort_key);
    let sort_arrow = if state.process_sort_desc {
        "↓"
    } else {
        "↑"
    };
    let process_total = processes.len();
    let row_total = rows.len();
    let range = if scroll_visible == 0 || rows.is_empty() {
        "0".to_string()
    } else {
        format!("{}–{}", start + 1, end)
    };
    let title = if process_total == 0 {
        format!(" PROCESSES · SORT {sort_name} {sort_arrow} · 0 ")
    } else if let Some(process) = pinned {
        format!(
            " PROCESSES · PIN {} · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_total} PROC ",
            process.pid
        )
    } else if let Some(program) = state.process_pinned_group.as_deref() {
        format!(
            " PROCESSES · PIN GROUP {program} · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_total} PROC "
        )
    } else {
        format!(
            " PROCESSES · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_total} PROC "
        )
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 30 || inner.height < 2 {
        return;
    }

    let has_scrollbar = rows.len() > scroll_visible && scroll_visible > 0;
    let table_width = inner.width.saturating_sub(u16::from(has_scrollbar));
    let wide = table_width >= 96;
    let header = if wide {
        process_header_wide(
            table_width as usize,
            state.process_sort_key,
            state.process_sort_desc,
        )
    } else {
        process_header_compact(
            table_width as usize,
            state.process_sort_key,
            state.process_sort_desc,
        )
    };
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, table_width, 1),
    );
    state.process_header_hits = process_header_hits(inner, table_width, wide);

    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        table_width,
        inner.height.saturating_sub(1),
    );

    let mut visible_rows = Vec::with_capacity(visible);
    if let Some(process) = pinned {
        visible_rows.push(ProcessDisplayRow::Process {
            process,
            child: false,
        });
    }
    visible_rows.extend(pinned_group_rows.iter().cloned());
    visible_rows.extend(rows.iter().skip(start).take(scroll_visible).cloned());

    state.process_rows = Some(ProcessRowsHit {
        rect: body,
        targets: visible_rows.iter().map(process_row_target).collect(),
    });

    let mut lines = Vec::with_capacity(visible);
    for row in visible_rows {
        match row {
            ProcessDisplayRow::Process { process, child } => {
                let is_selected = state.process_selected_pid == Some(process.pid)
                    || state.process_pinned_pid == Some(process.pid);
                lines.push(if wide {
                    process_line_wide(process, table_width as usize, is_selected, child)
                } else {
                    process_line_compact(process, table_width as usize, is_selected, child)
                });
            }
            ProcessDisplayRow::Group {
                program,
                count,
                cpu_pct,
                memory_bytes,
                threads,
                expanded,
            } => {
                let is_selected = state.process_selected_group.as_deref() == Some(program)
                    || state.process_pinned_group.as_deref() == Some(program);
                lines.push(if wide {
                    process_group_line_wide(
                        program,
                        count,
                        cpu_pct,
                        memory_bytes,
                        threads,
                        expanded,
                        is_selected,
                        table_width as usize,
                    )
                } else {
                    process_group_line_compact(
                        program,
                        count,
                        cpu_pct,
                        memory_bytes,
                        threads,
                        expanded,
                        is_selected,
                        table_width as usize,
                    )
                });
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " waiting for process samples…",
            Style::default().fg(MUTED),
        )));
    }

    frame.render_widget(Paragraph::new(lines), body);

    if has_scrollbar {
        draw_process_scrollbar(
            frame,
            Rect::new(
                inner.x.saturating_add(inner.width.saturating_sub(1)),
                body.y.saturating_add(pinned_rows as u16),
                1,
                body.height.saturating_sub(pinned_rows as u16),
            ),
            start,
            scroll_visible,
            rows.len(),
        );
    }
}

fn sorted_processes(
    processes: &[ProcessStats],
    key: ProcessSortKey,
    descending: bool,
) -> Vec<&ProcessStats> {
    let mut sorted = processes.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| {
        let ordering = match key {
            ProcessSortKey::Pid => a.pid.cmp(&b.pid),
            ProcessSortKey::Program => a.program.cmp(&b.program),
            ProcessSortKey::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
            ProcessSortKey::Memory => a.memory_bytes.cmp(&b.memory_bytes),
            ProcessSortKey::Threads => a.threads.cmp(&b.threads),
        };
        let ordering = if descending {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then_with(|| a.pid.cmp(&b.pid))
    });
    sorted
}

fn sorted_processes_with_pin(
    processes: &[ProcessStats],
    key: ProcessSortKey,
    descending: bool,
    pinned_pid: Option<u32>,
) -> (Option<&ProcessStats>, Vec<&ProcessStats>) {
    let mut sorted = sorted_processes(processes, key, descending);
    let pinned = pinned_pid.and_then(|pid| {
        let index = sorted.iter().position(|process| process.pid == pid)?;
        Some(sorted.remove(index))
    });
    (pinned, sorted)
}

fn process_sort_name(key: ProcessSortKey) -> &'static str {
    match key {
        ProcessSortKey::Pid => "PID",
        ProcessSortKey::Program => "PROGRAM",
        ProcessSortKey::Cpu => "CPU",
        ProcessSortKey::Memory => "MEM",
        ProcessSortKey::Threads => "THR",
    }
}

fn process_header_button(
    label: &str,
    key: ProcessSortKey,
    active: ProcessSortKey,
    descending: bool,
    width: usize,
    right_align: bool,
) -> Span<'static> {
    let is_active = key == active;
    let arrow = if is_active {
        if descending {
            "↓"
        } else {
            "↑"
        }
    } else {
        "↕"
    };
    let button = format!("[{label}{arrow}]");
    let cell = if right_align {
        format!("{button:>width$}")
    } else {
        format!("{button:<width$}")
    };
    let mut style = Style::default()
        .fg(if is_active { ORK_GREEN } else { CYAN })
        .add_modifier(Modifier::BOLD);
    if is_active {
        style = style
            .bg(PROCESS_SELECTED_BG)
            .add_modifier(Modifier::UNDERLINED);
    }
    Span::styled(cell, style)
}

fn process_header_hits(inner: Rect, table_width: u16, wide: bool) -> Vec<ProcessHeaderHit> {
    let width = table_width as usize;
    let mut hits = Vec::with_capacity(5);
    let mut x = inner.x;

    hits.push(ProcessHeaderHit {
        rect: Rect::new(x, inner.y, 7, 1),
        key: ProcessSortKey::Pid,
    });
    x = x.saturating_add(7);

    if wide {
        hits.push(ProcessHeaderHit {
            rect: Rect::new(x, inner.y, 16, 1),
            key: ProcessSortKey::Program,
        });
        x = x.saturating_add(16);
        let command_width = width.saturating_sub(7 + 17 + 7 + 9 + 6).max(12) as u16;
        x = x.saturating_add(command_width);
    } else {
        let program_width = width.saturating_sub(7 + 7 + 9 + 6).max(8) as u16;
        hits.push(ProcessHeaderHit {
            rect: Rect::new(x, inner.y, program_width, 1),
            key: ProcessSortKey::Program,
        });
        x = x.saturating_add(program_width);
    }

    hits.push(ProcessHeaderHit {
        rect: Rect::new(x, inner.y, 6, 1),
        key: ProcessSortKey::Cpu,
    });
    x = x.saturating_add(6);
    hits.push(ProcessHeaderHit {
        rect: Rect::new(x, inner.y, 9, 1),
        key: ProcessSortKey::Memory,
    });
    x = x.saturating_add(9);
    hits.push(ProcessHeaderHit {
        rect: Rect::new(x, inner.y, 6, 1),
        key: ProcessSortKey::Threads,
    });

    hits
}

fn draw_process_scrollbar(
    frame: &mut Frame,
    area: Rect,
    start: usize,
    visible: usize,
    total: usize,
) {
    if area.height == 0 || visible == 0 || total <= visible {
        return;
    }

    let track = area.height as usize;
    let thumb_len = ((visible as f64 / total as f64) * track as f64)
        .round()
        .max(1.0)
        .min(track as f64) as usize;
    let max_start = total.saturating_sub(visible).max(1);
    let thumb_start = ((start as f64 / max_start as f64) * track.saturating_sub(thumb_len) as f64)
        .round() as usize;

    let lines = (0..track)
        .map(|row| {
            let in_thumb = row >= thumb_start && row < thumb_start + thumb_len;
            Line::from(Span::styled(
                if in_thumb { "█" } else { "│" },
                Style::default().fg(if in_thumb { ORK_GREEN } else { INNER_GREEN }),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn process_header_compact(width: usize, active: ProcessSortKey, descending: bool) -> Line<'static> {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(10);
    Line::from(vec![
        Span::raw(" "),
        process_header_button("PID", ProcessSortKey::Pid, active, descending, 6, false),
        process_header_button(
            "PROGRAM",
            ProcessSortKey::Program,
            active,
            descending,
            program_width,
            false,
        ),
        process_header_button("CPU", ProcessSortKey::Cpu, active, descending, 6, true),
        Span::raw(" "),
        process_header_button("MEM", ProcessSortKey::Memory, active, descending, 8, true),
        Span::raw(" "),
        process_header_button("THR", ProcessSortKey::Threads, active, descending, 5, true),
    ])
}

fn process_header_wide(width: usize, active: ProcessSortKey, descending: bool) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    Line::from(vec![
        Span::raw(" "),
        process_header_button("PID", ProcessSortKey::Pid, active, descending, 6, false),
        process_header_button(
            "PROGRAM",
            ProcessSortKey::Program,
            active,
            descending,
            16,
            false,
        ),
        Span::styled(
            format!("{:<command_width$}", "COMMAND"),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        process_header_button("CPU", ProcessSortKey::Cpu, active, descending, 6, true),
        Span::raw(" "),
        process_header_button("MEM", ProcessSortKey::Memory, active, descending, 8, true),
        Span::raw(" "),
        process_header_button("THR", ProcessSortKey::Threads, active, descending, 5, true),
    ])
}

#[allow(clippy::too_many_arguments)]
fn process_group_line_compact(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: usize,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    let count_label = format!("×{count}");
    let arrow = if expanded { "▾" } else { "▸" };
    let program = fit_cell(&format!("{arrow} {program}"), program_width);
    let cpu = process_cpu_color(cpu_pct);
    let marker = if selected { "›" } else { " " };

    Line::from(vec![
        Span::styled(
            format!("{marker}{:<6}", count_label),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!("{program:<program_width$}"),
            process_cell_style(ORK_GREEN, selected, true),
        ),
        Span::styled(
            format!("{:>6.1}", cpu_pct),
            process_cell_style(cpu, selected, false),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(memory_bytes)),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!(" {:>5}", threads),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

fn process_line_compact(
    process: &ProcessStats,
    width: usize,
    selected: bool,
    child: bool,
) -> Line<'static> {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    let program = fit_cell(&process.program, program_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let program_bold = is_llm_process(&process.program, &process.command);
    let marker = if selected {
        "›"
    } else if child {
        "↳"
    } else {
        " "
    };

    Line::from(vec![
        Span::styled(
            format!("{marker}{:<6}", process.pid),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!("{program:<program_width$}"),
            process_cell_style(
                if program_bold { ORK_GREEN } else { DIM_GREEN },
                selected,
                program_bold,
            ),
        ),
        Span::styled(
            format!("{:>6.1}", process.cpu_pct),
            process_cell_style(cpu, selected, false),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

#[allow(clippy::too_many_arguments)]
fn process_group_line_wide(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: usize,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    let count_label = format!("×{count}");
    let arrow = if expanded { "▾" } else { "▸" };
    let program = fit_cell(&format!("{arrow} {program}"), 16);
    let command = fit_cell(&format!("{count} processes"), command_width);
    let cpu = process_cpu_color(cpu_pct);
    let marker = if selected { "›" } else { " " };

    Line::from(vec![
        Span::styled(
            format!("{marker}{:<6}", count_label),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!("{program:<16}"),
            process_cell_style(ORK_GREEN, selected, true),
        ),
        Span::styled(
            format!("{command:<command_width$}"),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!("{:>6.1}", cpu_pct),
            process_cell_style(cpu, selected, false),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(memory_bytes)),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!(" {:>5}", threads),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

fn process_line_wide(
    process: &ProcessStats,
    width: usize,
    selected: bool,
    child: bool,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    let program = fit_cell(&process.program, 16);
    let command = fit_cell(&process.command, command_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let program_bold = is_llm_process(&process.program, &process.command);
    let marker = if selected {
        "›"
    } else if child {
        "↳"
    } else {
        " "
    };

    Line::from(vec![
        Span::styled(
            format!("{marker}{:<6}", process.pid),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!("{program:<16}"),
            process_cell_style(
                if program_bold { ORK_GREEN } else { DIM_GREEN },
                selected,
                program_bold,
            ),
        ),
        Span::styled(
            format!("{command:<command_width$}"),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!("{:>6.1}", process.cpu_pct),
            process_cell_style(cpu, selected, false),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            process_cell_style(CYAN, selected, false),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

fn process_cell_style(color: Color, selected: bool, bold: bool) -> Style {
    let mut style = Style::default().fg(color);
    if selected {
        style = style.bg(PROCESS_SELECTED_BG).add_modifier(Modifier::BOLD);
    } else if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn fit_cell(text: &str, width: usize) -> String {
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

fn compact_memory(bytes: u64) -> String {
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

fn process_cpu_color(cpu_pct: f64) -> Color {
    if cpu_pct >= 100.0 {
        ORANGE
    } else if cpu_pct >= 50.0 {
        YELLOW
    } else if cpu_pct >= 10.0 {
        BRIGHT_GREEN
    } else {
        ORK_GREEN
    }
}

fn is_llm_process(program: &str, command: &str) -> bool {
    let program = program.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    program.contains("llama")
        || program.contains("orsiktop")
        || command.contains("llama")
        || command.contains("orsiktop")
}

fn draw_footer(frame: &mut Frame, area: Rect, llm: &LlmStats, gpu: &GpuStats) {
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

    let help = " [q] quit  [h] help ";
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

fn draw_help_popup(frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(6).min(72);
    let height = area.height.saturating_sub(4).min(20);
    if width < 48 || height < 16 {
        return;
    }

    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" HELP · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let key = |text: &'static str| {
        Span::styled(
            format!(" {text:<15}"),
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |text: &'static str| Span::styled(text, Style::default().fg(WHITE));

    let lines = vec![
        Line::from(Span::styled(
            " KEYBOARD",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("q"), desc("Quit")]),
        Line::from(vec![key("h"), desc("Toggle this help")]),
        Line::from(vec![
            key("- / +"),
            desc("Decrease / increase refresh interval"),
        ]),
        Line::from(vec![key("↑ / k"), desc("Select previous visible row")]),
        Line::from(vec![key("↓ / j"), desc("Select next visible row")]),
        Line::from(vec![key("PgUp / PgDn"), desc("Jump 10 processes")]),
        Line::from(vec![key("Home / End"), desc("First / last process")]),
        Line::from(""),
        Line::from(Span::styled(
            " MOUSE",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("Left click"), desc("Select process or group")]),
        Line::from(vec![key("Double click"), desc("Pin process or group")]),
        Line::from(vec![key("Right click"), desc("Expand / collapse group")]),
        Line::from(vec![key("Header click"), desc("Sort process table")]),
        Line::from(vec![key("Mouse wheel"), desc("Scroll process table")]),
        Line::from(vec![key("[-] / [+]"), desc("Change refresh interval")]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
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

#[cfg(test)]
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

fn fine_bar_spans(label: &str, percent: f64, width: usize, fallback: Color) -> Vec<Span<'static>> {
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
    fn same_named_processes_are_grouped_with_aggregate_metrics() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "qemu-system-x86".to_string(),
                cpu_pct: 120.0,
                memory_bytes: 2_000,
                threads: 8,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "qemu-system-x86".to_string(),
                cpu_pct: 80.0,
                memory_bytes: 3_000,
                threads: 7,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                program: "llama".to_string(),
                cpu_pct: 50.0,
                ..ProcessStats::default()
            },
        ];
        let expanded = HashSet::new();
        let (_, rows) =
            grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);

        assert_eq!(rows.len(), 2);
        match &rows[0] {
            ProcessDisplayRow::Group {
                program,
                count,
                cpu_pct,
                memory_bytes,
                threads,
                expanded,
            } => {
                assert_eq!(*program, "qemu-system-x86");
                assert_eq!(*count, 2);
                assert_eq!(*cpu_pct, 200.0);
                assert_eq!(*memory_bytes, 5_000);
                assert_eq!(*threads, 15);
                assert!(!expanded);
            }
            _ => panic!("expected grouped qemu row"),
        }
    }

    #[test]
    fn expanded_group_exposes_individual_process_rows() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                cpu_pct: 20.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                cpu_pct: 10.0,
                ..ProcessStats::default()
            },
        ];
        let expanded = HashSet::from(["chromium".to_string()]);
        let (_, rows) =
            grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);

        assert_eq!(rows.len(), 3);
        assert!(matches!(
            rows[0],
            ProcessDisplayRow::Group { expanded: true, .. }
        ));
        assert!(matches!(
            rows[1],
            ProcessDisplayRow::Process { child: true, .. }
        ));
        assert!(matches!(
            rows[2],
            ProcessDisplayRow::Process { child: true, .. }
        ));
    }

    #[test]
    fn right_click_toggles_group_expansion() {
        let mut state = UiState {
            process_rows: Some(ProcessRowsHit {
                rect: Rect::new(10, 10, 40, 1),
                targets: vec![ProcessRowTarget::Group("chromium".to_string())],
            }),
            ..UiState::default()
        };

        assert!(state.right_click_process_group(12, 10));
        assert!(state.expanded_process_groups.contains("chromium"));
        assert!(state.right_click_process_group(12, 10));
        assert!(!state.expanded_process_groups.contains("chromium"));
    }

    #[test]
    fn pinned_process_is_removed_from_sorted_stream() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                cpu_pct: 90.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                cpu_pct: 40.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                cpu_pct: 70.0,
                ..ProcessStats::default()
            },
        ];

        let (pinned, rest) =
            sorted_processes_with_pin(&processes, ProcessSortKey::Cpu, true, Some(30));
        assert_eq!(pinned.map(|process| process.pid), Some(30));
        assert_eq!(
            rest.iter().map(|process| process.pid).collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    fn process_click_test_state() -> UiState {
        UiState {
            process_rows: Some(ProcessRowsHit {
                rect: Rect::new(10, 10, 40, 2),
                targets: vec![ProcessRowTarget::Process(10), ProcessRowTarget::Process(20)],
            }),
            ..Default::default()
        }
    }

    fn group_click_test_state() -> UiState {
        UiState {
            process_rows: Some(ProcessRowsHit {
                rect: Rect::new(10, 10, 40, 1),
                targets: vec![ProcessRowTarget::Group("chromium".to_string())],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn single_click_selects_group_without_pinning() {
        let mut state = group_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
        assert_eq!(state.process_pinned_group, None);
    }

    #[test]
    fn double_click_pins_group() {
        let mut state = group_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
        assert_eq!(state.process_pinned_group.as_deref(), Some("chromium"));
    }

    #[test]
    fn pinned_group_is_taken_with_expanded_children() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                cpu_pct: 20.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                cpu_pct: 10.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                program: "llama".to_string(),
                cpu_pct: 5.0,
                ..ProcessStats::default()
            },
        ];
        let expanded = HashSet::from(["chromium".to_string()]);
        let (_, mut rows) =
            grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);
        let pinned = take_pinned_group_rows(&mut rows, Some("chromium"));

        assert_eq!(pinned.len(), 3);
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            pinned[0],
            ProcessDisplayRow::Group {
                program: "chromium",
                ..
            }
        ));
    }

    #[test]
    fn single_click_selects_process_without_pinning() {
        let mut state = process_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_pid, Some(10));
        assert_eq!(state.process_pinned_pid, None);
    }

    #[test]
    fn double_click_pins_process() {
        let mut state = process_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_pid, Some(10));
        assert_eq!(state.process_pinned_pid, Some(10));
    }

    #[test]
    fn clicking_another_process_releases_pin_and_selects_new_process() {
        let mut state = process_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_pinned_pid, Some(10));

        assert!(state.click_process_row(12, 11));
        assert_eq!(state.process_selected_pid, Some(20));
        assert_eq!(state.process_pinned_pid, None);
    }

    #[test]
    fn keyboard_navigation_treats_collapsed_group_as_one_row() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                cpu_pct: 30.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                cpu_pct: 20.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                program: "llama".to_string(),
                cpu_pct: 10.0,
                ..ProcessStats::default()
            },
        ];
        let mut state = UiState::default();

        state.process_home(&processes);
        assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
        assert_eq!(state.process_selected_pid, None);

        state.move_process_selection(1, &processes);
        assert_eq!(state.process_selected_pid, Some(30));
        assert_eq!(state.process_selected_group, None);
    }

    #[test]
    fn keyboard_navigation_enters_children_only_when_group_is_expanded() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                cpu_pct: 30.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                cpu_pct: 20.0,
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                program: "llama".to_string(),
                cpu_pct: 10.0,
                ..ProcessStats::default()
            },
        ];
        let mut state = UiState {
            expanded_process_groups: HashSet::from(["chromium".to_string()]),
            ..UiState::default()
        };

        state.process_home(&processes);
        assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));

        state.move_process_selection(1, &processes);
        assert!(matches!(state.process_selected_pid, Some(10 | 20)));
        assert_eq!(state.process_selected_group, None);
    }

    #[test]
    fn physical_core_average_combines_smt_threads() {
        let core = CpuPhysicalCore {
            kind: CpuCoreKind::Performance,
            logical_cpus: vec![0, 1],
        };
        assert_eq!(physical_core_average(&core, &[80.0, 20.0]), 50.0);
    }

    #[test]
    fn physical_core_color_heats_up_toward_saturation() {
        assert_eq!(physical_core_color(0.0, ORK_GREEN), ORK_GREEN);
        assert_eq!(physical_core_color(55.0, CYAN), CYAN);
        assert_eq!(physical_core_color(80.0, ORK_GREEN), YELLOW);
        assert_eq!(physical_core_color(92.0, CYAN), ORANGE);
        assert_eq!(physical_core_color(100.0, ORK_GREEN), RED);

        let mid = physical_core_color(86.0, ORK_GREEN);
        assert_ne!(mid, YELLOW);
        assert_ne!(mid, ORANGE);
    }

    #[test]
    fn core_usage_glyph_uses_heatmap_levels() {
        assert_eq!(core_usage_glyph(0.0), '·');
        assert_eq!(core_usage_glyph(2.0), '░');
        assert_eq!(core_usage_glyph(25.0), '▒');
        assert_eq!(core_usage_glyph(50.0), '▓');
        assert_eq!(core_usage_glyph(75.0), '█');
        assert_eq!(core_usage_glyph(100.0), '█');
    }

    #[test]
    fn core_usage_color_uses_non_red_load_scale() {
        assert_eq!(core_usage_color(0.0), BAR_EMPTY);
        assert_eq!(core_usage_color(2.0), DIM_GREEN);
        assert_eq!(core_usage_color(25.0), ORK_GREEN);
        assert_eq!(core_usage_color(50.0), BRIGHT_GREEN);
        assert_eq!(core_usage_color(75.0), YELLOW);
        assert_eq!(core_usage_color(90.0), ORANGE);
        assert_eq!(core_usage_color(100.0), ORANGE);
    }

    #[test]
    fn help_popup_state_toggles_and_closes() {
        let mut state = UiState::default();
        assert!(!state.is_help_open());
        state.toggle_help();
        assert!(state.is_help_open());
        state.toggle_help();
        assert!(!state.is_help_open());
        state.toggle_help();
        state.close_help();
        assert!(!state.is_help_open());
    }

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
    fn bar_gradients_use_expected_endpoints() {
        assert_eq!(bar_gradient_color("GPU", 100.0, WHITE), ORK_GREEN);
        assert_eq!(bar_gradient_color("VRAM", 90.0, WHITE), CYAN);
        assert_eq!(bar_gradient_color("VRAM", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("PWR", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("CTX", 100.0, WHITE), RED);
        assert_eq!(bar_gradient_color("OTHER", 50.0, CYAN), CYAN);
    }

    #[test]
    fn token_counts_are_grouped_for_readability() {
        assert_eq!(grouped_u64(999), "999");
        assert_eq!(grouped_u64(1_000), "1,000");
        assert_eq!(grouped_u64(196_608), "196,608");
        assert_eq!(grouped_f64(447_924.0), "447,924");
    }

    #[test]
    fn llm_phase_marks_idle_without_activity() {
        let stats = LlmStats::default();
        let (phase, color) = llm_phase(&stats);
        assert_eq!(phase, "IDLE");
        assert_eq!(color, MUTED);
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
}
