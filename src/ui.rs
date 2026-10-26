use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use crossterm::event::KeyCode;
use ratatui::{layout::Rect, style::Color};

#[cfg(test)]
use ratatui::{
    style::Style,
    text::{Line, Span},
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
mod draw;
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
pub use draw::draw;
pub(crate) use draw::uses_full_layout;
#[cfg(test)]
use draw::{draw_footer, footer_status};
pub use draw::{rect_contains, refresh_controls};
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
const PROCESS_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Upper bound for a typed additional-server endpoint (long enough for
/// token-bearing URLs, short enough to keep the draft line bounded).
const MAX_SERVER_DRAFT_CHARS: usize = 1024;

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

/// The STATE label the selected-server panel shows for `llm`, so other
/// modules' tests can check it against the per-server overview.
#[cfg(test)]
pub(crate) fn llm_phase_label(llm: &LlmStats) -> &'static str {
    llm_phase(llm).0
}

#[cfg(test)]
mod tests;
