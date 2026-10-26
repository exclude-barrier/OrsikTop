use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

#[cfg(test)]
use crate::cpu::{CpuCoreKind, CpuPhysicalCore, CpuTopology};
#[cfg(test)]
use crate::domain::{ActivityPhase, LlmSlotInfo};
#[cfg(test)]
use crate::gpu;
use crate::{
    config::{AppConfig, MAX_OFFLINE_GRACE_MS, MAX_PROCESS_REFRESH_MS, MIN_PROCESS_REFRESH_MS},
    domain::{
        GpuMapping, GpuSelector, GpuStats, LlmStats, ProcessIdentity, ProcessStats, ServerSummary,
        SystemStats, MAX_REFRESH_MS, MIN_REFRESH_MS,
    },
};

// Extracted submodules (behavior-preserving refactor).
mod dialogs;
mod gpu_panel;
mod history;
mod llm;
mod processes;
mod state_llm;
mod state_process;
mod state_settings;
mod state_shell;
mod system;
mod widgets;
use dialogs::{
    build_endpoint, default_port_for_scheme, draw_help_popup, draw_server_selector,
    draw_settings_popup, endpoint_parts, parse_endpoint_base, parse_setting_u64,
};
use gpu_panel::{draw_gpu, enc_dec_idle};
#[cfg(test)]
use gpu_panel::{gpu_attribution_label, gpu_in_mapping};
#[cfg(test)]
use history::{
    braille_char, llm_rate_history_has_activity, resample_history, smooth_llm_sample_interval,
    trend_lines,
};
use history::{draw_history, observe_llm_into, push_history_at};
use llm::{draw_llm, friendly_llm_error, llm_link_status, metrics_unavailable_reason};
#[cfg(test)]
use llm::{
    llm_last_sample_text, llm_phase, llm_slot_overview_line, llm_spec_row, slot_overview_entries,
    valid_cache_counter,
};
use processes::{
    draw_processes, grouped_process_rows_filtered, process_row_target, take_pinned_group_rows,
};
#[cfg(test)]
use processes::{
    grouped_process_rows, process_group_line_wide, process_header_wide, process_matches_search,
    sorted_processes_with_pin,
};
use state_llm::{LlmHistory, TimedSample};
use state_process::{
    ProcessDisplayRow, ProcessGroup, ProcessHeaderHit, ProcessRowTarget, ProcessRowsHit,
    ProcessSortKey,
};
use state_settings::SettingsField;
#[cfg(test)]
use system::{
    core_heatmap_rows, core_usage_color, core_usage_glyph, physical_core_average,
    physical_core_color, system_full_view,
};
use system::{draw_system, load_average_text, system_panel_height};
#[cfg(test)]
use widgets::{
    bar_gradient_color, clamp_percent, fine_bar, fine_bar_spans, fixed_data_pair, grouped_f64,
    grouped_u64, label_span, meter_line, optional_number, thread_cell,
};
use widgets::{button_span, compact_endpoint, fit_cell, percent};

#[cfg(test)]
mod layout_regression_tests;
#[cfg(test)]
mod settings_redaction_tests;
#[cfg(test)]
mod ux_baseline_tests;
#[cfg(test)]
mod width_regression_tests;

const HISTORY_WINDOW: Duration = Duration::from_secs(60);
const HISTORY_MAX_SAMPLES: usize = 720;
const REFRESH_CONTROL_WIDTH: u16 = 22;
const PROCESS_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Hard floor below which even the reduced layout cannot be drawn usefully.
/// At or above this size the dashboard stays usable; below it the too-small
/// notice is shown. The width is the narrowest that still fits the compact
/// LLM/context summary line (labels plus a model/context value); the height
/// leaves room for the status line plus that summary.
/// Upper bound for a typed additional-server endpoint (long enough for
/// token-bearing URLs, short enough to keep the draft line bounded).
const MAX_SERVER_DRAFT_CHARS: usize = 1024;
const COMPACT_MIN_WIDTH: u16 = 54;
const COMPACT_MIN_HEIGHT: u16 = 16;
/// Floor of the full dashboard layout. Between the compact floor and this the
/// reduced [`draw_compact`] layout is drawn instead of the dead-end notice.
const FULL_MIN_WIDTH: u16 = 72;
const FULL_MIN_HEIGHT: u16 = 22;

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RefreshControls {
    pub minus: Rect,
    pub plus: Rect,
}

pub struct UiState {
    gpu_history: VecDeque<TimedSample>,
    vram_history: VecDeque<TimedSample>,
    cpu_history: VecDeque<TimedSample>,
    ram_history: VecDeque<TimedSample>,
    /// History/connection state of the currently selected server.
    llm_history: LlmHistory,
    /// Per-server history/connection state, keyed by stable server identity.
    llm_histories: HashMap<String, LlmHistory>,
    /// Server key the active `llm_history` belongs to.
    llm_history_key: String,
    process_selected_pid: Option<ProcessIdentity>,
    process_pinned_pid: Option<ProcessIdentity>,
    last_process_click: Option<(ProcessIdentity, Instant)>,
    process_selected_group: Option<String>,
    process_pinned_group: Option<String>,
    last_process_group_click: Option<(String, Instant)>,
    expanded_process_groups: HashSet<String>,
    process_scroll: usize,
    process_display_total: usize,
    process_scroll_visible: usize,
    process_sort_key: ProcessSortKey,
    process_sort_desc: bool,
    process_header_hits: Vec<ProcessHeaderHit>,
    process_pane: Option<Rect>,
    process_rows: Option<ProcessRowsHit>,
    process_search_open: bool,
    process_search_query: String,
    help_open: bool,
    settings_open: bool,
    settings_field: SettingsField,
    settings_host: String,
    settings_port: String,
    /// Exact endpoint string the dialog was opened with, returned verbatim
    /// when host/port are left unchanged so no URL part is silently lost.
    settings_server_original: String,
    /// Parsed form of the endpoint, used to rebuild the URL while preserving
    /// scheme, path, userinfo, query and fragment.
    settings_server_base: Option<reqwest::Url>,
    settings_gpu: String,
    settings_refresh_ms: String,
    settings_process_refresh_ms: String,
    settings_offline_grace_ms: String,
    settings_auto_discovery: bool,
    /// Additional configured endpoints, preserved verbatim across a settings
    /// save (the dialog edits host/port of the primary endpoint).
    settings_servers: Vec<String>,
    /// Highlighted entry of the additional-server list.
    settings_server_index: usize,
    /// Text of the not-yet-committed server entry. `Some` means the list is in
    /// edit/insert mode and key presses type into the draft.
    settings_server_draft: Option<String>,
    /// Index of the additional-server entry being edited, or `None` when the
    /// active draft inserts a new entry.
    settings_server_edit_index: Option<usize>,
    /// Validation/duplicate error for the additional-server list, kept apart
    /// from the save error so a list hint never hides a save failure.
    settings_server_error: Option<String>,
    settings_error: Option<String>,
    /// Multi-server selector: open state and highlighted row.
    server_selector_open: bool,
    server_selector_index: usize,
    /// Snapshots dropped because their bounded channel was full. Shown in the
    /// footer only when nonzero, so healthy runs keep the normal status line.
    fast_snapshot_drops: u64,
    llm_snapshot_drops: u64,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            gpu_history: VecDeque::with_capacity(600),
            vram_history: VecDeque::with_capacity(600),
            cpu_history: VecDeque::with_capacity(600),
            ram_history: VecDeque::with_capacity(600),
            llm_history: LlmHistory::default(),
            llm_histories: HashMap::new(),
            llm_history_key: String::new(),
            process_selected_pid: None,
            process_pinned_pid: None,
            last_process_click: None,
            process_selected_group: None,
            process_pinned_group: None,
            last_process_group_click: None,
            expanded_process_groups: HashSet::new(),
            process_scroll: 0,
            process_display_total: 0,
            process_scroll_visible: 0,
            process_sort_key: ProcessSortKey::Cpu,
            process_sort_desc: true,
            process_header_hits: Vec::new(),
            process_pane: None,
            process_rows: None,
            process_search_open: false,
            process_search_query: String::new(),
            help_open: false,
            settings_open: false,
            settings_field: SettingsField::Host,
            settings_host: "127.0.0.1".to_string(),
            settings_port: "8080".to_string(),
            settings_server_original: String::new(),
            settings_server_base: None,
            settings_gpu: "0".to_string(),
            settings_refresh_ms: "1000".to_string(),
            settings_process_refresh_ms: "1000".to_string(),
            settings_offline_grace_ms: "2500".to_string(),
            settings_auto_discovery: true,
            settings_servers: Vec::new(),
            settings_server_index: 0,
            settings_server_draft: None,
            settings_server_edit_index: None,
            settings_server_error: None,
            settings_error: None,
            server_selector_open: false,
            server_selector_index: 0,
            fast_snapshot_drops: 0,
            llm_snapshot_drops: 0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    system: &SystemStats,
    llm: &LlmStats,
    gpu: &GpuStats,
    gpu_map: &GpuMapping,
    servers: &[ServerSummary],
    selected_server: &str,
    // Configured endpoints beyond the monitoring cap; listed as "limit reached".
    overflow: &[crate::app::ServerSpec],
    state: &mut UiState,
    server: &str,
    refresh_ms: u64,
    server_auto: bool,
) {
    let area = frame.area();
    state.clear_process_interaction();

    frame.render_widget(Block::default().style(Style::default().bg(BG_BLACK)), area);

    if area.width < COMPACT_MIN_WIDTH || area.height < COMPACT_MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    // Between the hard floor and the full-layout floor there is not room for
    // the whole dashboard, but a dead-end notice wastes a usable terminal.
    // Draw the reduced layout and still honour any open overlay on top.
    if area.width < FULL_MIN_WIDTH || area.height < FULL_MIN_HEIGHT {
        draw_compact(
            frame,
            area,
            llm,
            system,
            state,
            server,
            refresh_ms,
            server_auto,
            servers,
            selected_server,
        );
        draw_overlays(frame, area, servers, selected_server, overflow, state);
        return;
    }

    let llm_height = if llm.connected { 12 } else { 5 };
    let system_height = system_panel_height(system);
    let middle_height = llm_height.max(system_height);
    let gpu_height = if enc_dec_idle(gpu) { 6 } else { 7 };
    let history_required = 3 + gpu_height + middle_height + 5 + 3;
    let show_history = area.height >= history_required;

    let rows = if show_history {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(gpu_height),
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
                Constraint::Length(gpu_height),
                Constraint::Length(middle_height),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area)
    };

    draw_header(
        frame,
        rows[0],
        llm,
        state,
        server,
        refresh_ms,
        server_auto,
        servers,
        selected_server,
        overflow.len(),
    );
    draw_gpu(frame, rows[1], gpu, gpu_map);
    draw_llm_and_system(frame, rows[2], system, llm, state, gpu_map);

    if show_history {
        draw_bottom(frame, rows[3], state, &system.processes);
    }
    draw_footer(frame, rows[4], llm, gpu, state);

    draw_overlays(frame, area, servers, selected_server, overflow, state);
}

/// Draw whichever modal overlay is open, on top of the dashboard (full or
/// compact). Only one overlay is shown at a time; the selector wins so an
/// accidental double-open cannot hide it.
fn draw_overlays(
    frame: &mut Frame,
    area: Rect,
    servers: &[ServerSummary],
    selected_server: &str,
    overflow: &[crate::app::ServerSpec],
    state: &UiState,
) {
    if state.server_selector_open {
        draw_server_selector(
            frame,
            area,
            servers,
            selected_server,
            state.server_selector_index,
            overflow,
        );
    } else if state.settings_open {
        draw_settings_popup(frame, area, state);
    } else if state.help_open {
        draw_help_popup(frame, area);
    }
}

/// Reduced dashboard for terminals between the hard floor (`COMPACT_MIN_*`) and
/// the full-layout floor (`FULL_MIN_*`). The full layout cannot fit there, but
/// the essentials still can: the connection-status header and a compact
/// LLM/context summary, with a system load line when the reduced height leaves
/// room. GPU, history and the process table are dropped.
#[allow(clippy::too_many_arguments)]
fn draw_compact(
    frame: &mut Frame,
    area: Rect,
    llm: &LlmStats,
    system: &SystemStats,
    state: &UiState,
    server: &str,
    refresh_ms: u64,
    server_auto: bool,
    servers: &[ServerSummary],
    selected_server: &str,
) {
    let block = Block::default()
        .title(" OrsikTop · compact ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (status, status_color) = llm_link_status(state, llm);
    let mut header = vec![
        Span::styled(
            format!(" {status} "),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ", Style::default().fg(DIM_GREEN)),
        Span::styled(compact_endpoint(server), Style::default().fg(MUTED)),
    ];
    if server_auto {
        header.push(Span::styled(" ·auto", Style::default().fg(MUTED)));
    }
    let mut lines = vec![Line::from(header)];

    let model = if llm.model.is_empty() {
        "—".to_string()
    } else {
        fit_cell(&llm.model, inner.width.saturating_sub(8) as usize)
    };
    lines.push(Line::from(vec![
        Span::styled(" MODEL ", Style::default().fg(MUTED)),
        Span::styled(model, Style::default().fg(WHITE)),
    ]));

    let ctx = match (llm.context_used, llm.context_size) {
        (Some(used), size) if size > 0 => format!("{used}/{size}"),
        (None, size) if size > 0 => format!("—/{size}"),
        _ => "—".to_string(),
    };
    let tps = llm.generation_tps.map_or_else(
        || "— tok/s".to_string(),
        |value| format!("{value:.1} tok/s"),
    );
    lines.push(Line::from(vec![
        Span::styled(" CTX ", Style::default().fg(MUTED)),
        Span::styled(format!("{ctx:<18}"), Style::default().fg(WHITE)),
        Span::styled("GEN ", Style::default().fg(MUTED)),
        Span::styled(tps, Style::default().fg(WHITE)),
    ]));

    if servers.len() > 1 {
        let index = servers
            .iter()
            .position(|summary| summary.key == selected_server)
            .map_or(0, |position| position + 1);
        lines.push(Line::from(Span::styled(
            format!(" [{index}/{}] servers · s = selector", servers.len()),
            Style::default().fg(MUTED),
        )));
    }

    // A search opened in the compact layout has no process table or footer to
    // render it, so show its live state here; otherwise typing would vanish
    // into an invisible mode. The hint below deliberately omits `[/] search`
    // because the compact layout has no process list to search.
    if state.process_search_open {
        lines.push(Line::from(vec![
            Span::styled(" SEARCH ", Style::default().fg(CYAN)),
            Span::styled(
                format!(
                    "/{}",
                    fit_cell(
                        &state.process_search_query,
                        inner.width.saturating_sub(20) as usize
                    )
                ),
                Style::default().fg(WHITE),
            ),
            Span::styled("  · Esc cancel", Style::default().fg(MUTED)),
        ]));
    }

    // Keep this short enough to survive the 52-column inner width of the
    // 54-wide hard floor even with a 5-digit refresh interval.
    lines.push(Line::from(Span::styled(
        format!(" {refresh_ms}ms · h help · q settings · esc quit"),
        Style::default().fg(MUTED),
    )));

    if inner.height as usize > lines.len() {
        lines.push(Line::from(vec![
            Span::styled(" LOAD ", Style::default().fg(MUTED)),
            Span::styled(load_average_text(system), Style::default().fg(WHITE)),
            Span::styled("  CPU ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.0}%", system.cpu_usage),
                Style::default().fg(WHITE),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The STATE label the selected-server panel shows for `llm`, so other
/// modules' tests can check it against the per-server overview.
#[cfg(test)]
pub(crate) fn llm_phase_label(llm: &LlmStats) -> &'static str {
    llm_phase(llm).0
}

/// Whether the terminal is large enough for the full dashboard layout. The
/// compact range draws a different header with no refresh buttons, so callers
/// outside the render path (e.g. mouse hit-testing) use this to avoid acting
/// on controls that are not on screen.
pub(crate) fn uses_full_layout(width: u16, height: u16) -> bool {
    width >= FULL_MIN_WIDTH && height >= FULL_MIN_HEIGHT
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("OrsikTop needs at least 54x16 terminal cells")
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

#[allow(clippy::too_many_arguments)]
fn draw_header(
    frame: &mut Frame,
    area: Rect,
    llm: &LlmStats,
    state: &UiState,
    server: &str,
    refresh_ms: u64,
    server_auto: bool,
    servers: &[ServerSummary],
    selected_server: &str,
    overflow_count: usize,
) {
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
        let (status, status_color) = llm_link_status(state, llm);
        let status_style = Style::default()
            .fg(status_color)
            .add_modifier(Modifier::BOLD);
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
            Span::styled("LLM ENDPOINT ", Style::default().fg(WHITE)),
            Span::styled(endpoint, Style::default().fg(MUTED)),
        ]);
        if server_auto {
            spans.push(Span::styled(" ·auto", Style::default().fg(MUTED)));
        }
        if servers.len() > 1 || overflow_count > 0 {
            let index = servers
                .iter()
                .position(|summary| summary.key == selected_server)
                .map_or(0, |position| position + 1);
            spans.push(Span::styled(
                format!("  [{index}/{}]  s = servers", servers.len()),
                Style::default().fg(MUTED),
            ));
            if overflow_count > 0 {
                spans.push(Span::styled(
                    format!("  · {overflow_count} over limit"),
                    Style::default().fg(YELLOW),
                ));
            }
        }

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

fn draw_llm_and_system(
    frame: &mut Frame,
    area: Rect,
    system: &SystemStats,
    llm: &LlmStats,
    state: &UiState,
    gpu_map: &GpuMapping,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    draw_llm(frame, cols[0], llm, state, gpu_map);
    draw_system(frame, cols[1], system);
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

/// The footer status line. A reachable server whose `/metrics` failed must not
/// be reported offline just because the error text mentions the connection; a
/// truly unreachable server still is.
fn footer_status(llm: &LlmStats, gpu: &GpuStats) -> String {
    if !llm.error.is_empty() {
        if llm.connected && !llm.metrics_available {
            metrics_unavailable_reason(&llm.error)
        } else {
            friendly_llm_error(&llm.error)
        }
    } else if !gpu.error.is_empty() {
        format!("GPU · {}", gpu.error)
    } else {
        "READY · MORE POWER, HAPPIER ORKS".to_string()
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, llm: &LlmStats, gpu: &GpuStats, state: &UiState) {
    let drops = state.snapshot_drop_text();
    let status = match &drops {
        Some(drops) => format!("{drops} · {}", footer_status(llm, gpu)),
        None => footer_status(llm, gpu),
    };

    let status_color = if !llm.error.is_empty() || !gpu.error.is_empty() || drops.is_some() {
        YELLOW
    } else {
        ORK_GREEN
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let help = if state.process_search_open {
        format!(
            " SEARCH /{}▏  [enter] apply  [esc] clear ",
            fit_cell(&state.process_search_query, 30)
        )
    } else if inner.width >= 110 {
        " [esc] quit  [h] help  [s] servers  [q] settings  [-/+] refresh  [/] search  [dbl] pin "
            .to_string()
    } else {
        " [esc] quit  [h] help  [q] settings  [/] search  [dbl] pin ".to_string()
    };
    let help_width = help.chars().count() as u16;
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(if state.process_search_open {
            CYAN
        } else {
            MUTED
        })),
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

#[cfg(test)]
mod tests;
