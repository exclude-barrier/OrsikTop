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
    config::{AppConfig, MAX_OFFLINE_GRACE_MS, MAX_PROCESS_REFRESH_MS, MIN_PROCESS_REFRESH_MS},
    cpu::{CpuCoreKind, CpuPhysicalCore, CpuTopology, CpuVendor},
    domain::{
        GpuMapping, GpuSelector, GpuStats, LlmSlotInfo, LlmStats, MappedGpu, ProcessIdentity,
        ProcessStats, ServerSummary, SystemStats, MAX_REFRESH_MS, MIN_REFRESH_MS,
    },
    gpu,
};

// Extracted submodules (behavior-preserving refactor).
mod dialogs;
mod gpu_panel;
mod history;
mod llm;
mod processes;
mod system;
mod widgets;
#[allow(unused_imports)]
use dialogs::*;
#[allow(unused_imports)]
use gpu_panel::*;
#[allow(unused_imports)]
use history::*;
#[allow(unused_imports)]
use llm::*;
#[allow(unused_imports)]
use processes::*;
#[allow(unused_imports)]
use system::*;
#[allow(unused_imports)]
use widgets::*;

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RefreshControls {
    pub minus: Rect,
    pub plus: Rect,
}

#[derive(Copy, Clone, Debug)]
struct TimedSample {
    at: Instant,
    value: f64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProcessSortKey {
    Pid,
    Program,
    Cpu,
    Memory,
    Threads,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SettingsField {
    Host,
    Port,
    Gpu,
    Refresh,
    ProcessRefresh,
    OfflineGrace,
    AutoDiscovery,
}

#[derive(Copy, Clone, Debug)]
struct ProcessHeaderHit {
    rect: Rect,
    key: ProcessSortKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessRowTarget {
    Process(ProcessIdentity),
    Group(String),
}

#[derive(Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    targets: Vec<ProcessRowTarget>,
}

/// Bounded in-memory LLM telemetry history and connection state for one
/// server. Kept per server (keyed by stable identity) so every monitored
/// server accumulates independently and the history survives switching the
/// selected server. Host-level CPU/GPU histories are separate and unaffected.
#[derive(Clone, Default)]
struct LlmHistory {
    prefill: VecDeque<TimedSample>,
    decode: VecDeque<TimedSample>,
    last_fresh_at: Option<Instant>,
    sample_interval_ema_ms: Option<f64>,
    connected_since: Option<Instant>,
    connected_flash_until: Option<Instant>,
    was_connected: bool,
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
    settings_error: Option<String>,
    /// Multi-server selector: open state and highlighted row.
    server_selector_open: bool,
    server_selector_index: usize,
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
            settings_error: None,
            server_selector_open: false,
            server_selector_index: 0,
        }
    }
}

impl UiState {
    pub fn push_sample(&mut self, gpu: &GpuStats, system: &SystemStats) {
        let now = Instant::now();
        let vram = match (gpu.memory_used_mib, gpu.memory_total_mib) {
            (Some(used), Some(total)) if total > 0.0 => Some(percent(used, total)),
            _ => None,
        };
        let ram = if system.memory_total_bytes > 0 {
            Some(percent(
                system.memory_used_bytes as f64,
                system.memory_total_bytes as f64,
            ))
        } else {
            None
        };

        if let Some(utilization) = gpu.utilization {
            push_history_at(&mut self.gpu_history, utilization, now);
        }
        if let Some(vram) = vram {
            push_history_at(&mut self.vram_history, vram, now);
        }
        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        if let Some(ram) = ram {
            push_history_at(&mut self.ram_history, ram, now);
        }
    }

    /// Record one LLM sample for `key`, mirroring it into the active history
    /// when `key` is the selected server. Called exactly once per received
    /// sample, so a UI redraw never creates a duplicate history point.
    pub fn record_llm_sample(&mut self, key: &str, selected: &str, llm: &LlmStats) {
        let now = Instant::now();
        if key == selected {
            if self.llm_history_key != key {
                self.llm_history = LlmHistory::default();
                self.llm_history_key = key.to_string();
            }
            observe_llm_into(&mut self.llm_history, llm, now);
            self.llm_histories
                .insert(key.to_string(), self.llm_history.clone());
        } else {
            observe_llm_into(
                self.llm_histories.entry(key.to_string()).or_default(),
                llm,
                now,
            );
        }
    }

    /// Make `key`'s own history active (the selected server changed). Other
    /// servers' histories are left untouched.
    pub fn select_llm_history(&mut self, key: &str) {
        if self.llm_history_key == key {
            return;
        }
        self.llm_history = self.llm_histories.get(key).cloned().unwrap_or_default();
        self.llm_history_key = key.to_string();
    }

    /// Drop one server's history (e.g. a proven process restart at the same
    /// endpoint), so an incompatible session is not drawn as continuous.
    pub fn reset_llm_history(&mut self, key: &str) {
        self.llm_histories.remove(key);
        if self.llm_history_key == key {
            self.llm_history = LlmHistory::default();
        }
    }

    /// Keep histories only for servers that still exist, bounding memory and
    /// avoiding tombstone growth when servers disappear.
    pub fn retain_llm_histories(&mut self, keys: &HashSet<String>) {
        self.llm_histories.retain(|key, _| keys.contains(key));
        if !self.llm_history_key.is_empty() && !keys.contains(&self.llm_history_key) {
            self.llm_history = LlmHistory::default();
            self.llm_history_key.clear();
        }
    }

    pub fn is_server_selector_open(&self) -> bool {
        self.server_selector_open
    }

    /// Open the selector with the highlight on `selected_index` (the currently
    /// selected server), so opening and confirming does not switch away.
    pub fn open_server_selector(&mut self, selected_index: usize) {
        self.server_selector_open = true;
        self.server_selector_index = selected_index;
        self.help_open = false;
        self.process_search_open = false;
        self.settings_open = false;
    }

    pub fn close_server_selector(&mut self) {
        self.server_selector_open = false;
    }

    /// Move the highlight within `len` servers, wrapping around. No server is
    /// selected by moving; `Enter` commits the highlight.
    pub fn move_server_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.server_selector_index = 0;
            return;
        }
        let current = self.server_selector_index.min(len - 1) as isize;
        self.server_selector_index = (current + delta).rem_euclid(len as isize) as usize;
    }

    /// The highlighted index, clamped to the current list length.
    pub fn server_selector_index(&self, len: usize) -> Option<usize> {
        (len > 0).then(|| self.server_selector_index.min(len - 1))
    }

    pub fn move_process_selection(&mut self, delta: isize, processes: &[ProcessStats]) {
        self.normalize_process_selection(processes);
        let targets = self.process_navigation_targets(processes);
        if targets.is_empty() {
            self.process_selected_pid = None;
            self.process_selected_group = None;
            self.process_scroll = 0;
            return;
        }

        let current = self
            .current_process_target()
            .and_then(|selected| targets.iter().position(|target| *target == selected));
        let target = match current {
            Some(current) if delta.is_negative() => current.saturating_sub(delta.unsigned_abs()),
            Some(current) => current
                .saturating_add(delta as usize)
                .min(targets.len() - 1),
            None if delta.is_negative() => self.process_scroll.min(targets.len() - 1),
            None => self.process_scroll.min(targets.len() - 1),
        };
        self.select_process_target(&targets[target]);
        self.ensure_selected_process_visible(processes);
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
        self.ensure_selected_process_visible(processes);
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
        self.ensure_selected_process_visible(processes);
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
        let (pinned, mut rows) = grouped_process_rows_filtered(
            processes,
            self.process_sort_key,
            self.process_sort_desc,
            self.process_pinned_pid,
            &self.expanded_process_groups,
            self.process_search_query(),
        );
        let pinned_group_rows =
            take_pinned_group_rows(&mut rows, self.process_pinned_group.as_deref());

        let mut targets = Vec::with_capacity(
            rows.len() + pinned_group_rows.len() + usize::from(pinned.is_some()),
        );
        if let Some(process) = pinned {
            targets.push(ProcessRowTarget::Process(process.identity()));
        }
        targets.extend(pinned_group_rows.iter().map(process_row_target));
        targets.extend(rows.iter().map(process_row_target));
        targets
    }

    fn process_scroll_targets(&self, processes: &[ProcessStats]) -> Vec<ProcessRowTarget> {
        let (_, mut rows) = grouped_process_rows_filtered(
            processes,
            self.process_sort_key,
            self.process_sort_desc,
            self.process_pinned_pid,
            &self.expanded_process_groups,
            self.process_search_query(),
        );
        let _ = take_pinned_group_rows(&mut rows, self.process_pinned_group.as_deref());
        rows.iter().map(process_row_target).collect()
    }

    fn normalize_process_selection(&mut self, processes: &[ProcessStats]) {
        let Some(selected) = self.current_process_target() else {
            return;
        };
        let targets = self.process_navigation_targets(processes);
        if targets.contains(&selected) {
            return;
        }

        match selected {
            ProcessRowTarget::Process(identity) => {
                if let Some(process) = processes
                    .iter()
                    .find(|process| process.identity() == identity)
                {
                    let group = ProcessRowTarget::Group(process.program.clone());
                    if targets.contains(&group) {
                        self.select_process_target(&group);
                        return;
                    }
                }
            }
            ProcessRowTarget::Group(program) => {
                if let Some(process) = processes.iter().find(|process| process.program == program) {
                    let process_target = ProcessRowTarget::Process(process.identity());
                    if targets.contains(&process_target) {
                        self.select_process_target(&process_target);
                        return;
                    }
                }
            }
        }

        self.process_selected_pid = None;
        self.process_selected_group = None;
    }

    fn ensure_selected_process_visible(&mut self, processes: &[ProcessStats]) {
        let Some(selected) = self.current_process_target() else {
            return;
        };
        let targets = self.process_scroll_targets(processes);
        let total = targets.len();
        self.process_display_total = total;
        if total == 0 {
            self.process_scroll = 0;
            return;
        }

        let Some(index) = targets.iter().position(|target| *target == selected) else {
            // Pinned rows are always visible and are intentionally outside the scroll stream.
            self.process_scroll = self.process_scroll.min(total.saturating_sub(1));
            return;
        };

        let visible = self.process_scroll_visible;
        if visible == 0 {
            self.process_scroll = index;
            return;
        }

        let max_start = total.saturating_sub(visible);
        let mut start = self.process_scroll.min(max_start);
        if index < start {
            start = index;
        } else if index >= start.saturating_add(visible) {
            start = index.saturating_add(1).saturating_sub(visible);
        }
        self.process_scroll = start.min(max_start);
    }

    pub fn clamp_process_selection(&mut self, processes: &[ProcessStats]) {
        if self.process_selected_pid.is_some_and(|identity| {
            !processes
                .iter()
                .any(|process| process.identity() == identity)
        }) {
            self.process_selected_pid = None;
        }
        if self.process_pinned_pid.is_some_and(|identity| {
            !processes
                .iter()
                .any(|process| process.identity() == identity)
        }) {
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
        self.normalize_process_selection(processes);
        self.ensure_selected_process_visible(processes);
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

    pub fn open_process_search(&mut self) {
        self.help_open = false;
        self.settings_open = false;
        self.process_search_open = true;
        self.process_scroll = 0;
    }

    pub fn accept_process_search(&mut self) {
        self.process_search_open = false;
    }

    pub fn clear_process_search(&mut self) {
        self.process_search_open = false;
        self.process_search_query.clear();
        self.process_scroll = 0;
    }

    pub fn is_process_search_open(&self) -> bool {
        self.process_search_open
    }

    pub fn process_search_backspace(&mut self) {
        self.process_search_query.pop();
        self.process_scroll = 0;
    }

    pub fn process_search_insert_char(&mut self, ch: char) {
        if !ch.is_control() && self.process_search_query.chars().count() < 48 {
            self.process_search_query.push(ch);
            self.process_scroll = 0;
        }
    }

    fn process_search_query(&self) -> Option<&str> {
        let query = self.process_search_query.trim();
        (!query.is_empty()).then_some(query)
    }

    pub fn toggle_help(&mut self) {
        self.settings_open = false;
        self.process_search_open = false;
        self.help_open = !self.help_open;
    }

    pub fn close_help(&mut self) {
        self.help_open = false;
    }

    pub fn is_help_open(&self) -> bool {
        self.help_open
    }

    pub fn open_settings(&mut self, active_server: &str, settings: &AppConfig) {
        let editable_server = settings.server.as_deref().unwrap_or(active_server);
        let (host, port) = endpoint_parts(editable_server);
        self.help_open = false;
        self.process_search_open = false;
        self.settings_open = true;
        self.settings_field = SettingsField::Host;
        self.settings_host = host;
        self.settings_port = port.to_string();
        // Keep the exact original and, when parseable, a structured base so
        // saving can preserve everything the dialog does not edit (scheme,
        // path, userinfo, …) instead of rebuilding a bare `http://host:port`.
        self.settings_server_original = editable_server.to_string();
        self.settings_server_base = reqwest::Url::parse(editable_server.trim()).ok();
        self.settings_gpu = settings.gpu_selector.as_string();
        self.settings_refresh_ms = settings.refresh_ms.to_string();
        self.settings_process_refresh_ms = settings.process_refresh_ms.to_string();
        self.settings_offline_grace_ms = settings.offline_grace_ms.to_string();
        self.settings_auto_discovery = settings.auto_discovery;
        self.settings_servers = settings.servers.clone();
        self.settings_error = None;
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_error = None;
    }

    pub fn is_settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn settings_next_field(&mut self) {
        self.settings_field = match self.settings_field {
            SettingsField::Host => SettingsField::Port,
            SettingsField::Port => SettingsField::Gpu,
            SettingsField::Gpu => SettingsField::Refresh,
            SettingsField::Refresh => SettingsField::ProcessRefresh,
            SettingsField::ProcessRefresh => SettingsField::OfflineGrace,
            SettingsField::OfflineGrace => SettingsField::AutoDiscovery,
            SettingsField::AutoDiscovery => SettingsField::Host,
        };
        self.settings_error = None;
    }

    pub fn settings_previous_field(&mut self) {
        self.settings_field = match self.settings_field {
            SettingsField::Host => SettingsField::AutoDiscovery,
            SettingsField::Port => SettingsField::Host,
            SettingsField::Gpu => SettingsField::Port,
            SettingsField::Refresh => SettingsField::Gpu,
            SettingsField::ProcessRefresh => SettingsField::Refresh,
            SettingsField::OfflineGrace => SettingsField::ProcessRefresh,
            SettingsField::AutoDiscovery => SettingsField::OfflineGrace,
        };
        self.settings_error = None;
    }

    pub fn settings_backspace(&mut self) {
        match self.settings_field {
            SettingsField::Host => {
                self.settings_host.pop();
            }
            SettingsField::Port => {
                self.settings_port.pop();
            }
            SettingsField::Gpu => {
                self.settings_gpu.pop();
            }
            SettingsField::Refresh => {
                self.settings_refresh_ms.pop();
            }
            SettingsField::ProcessRefresh => {
                self.settings_process_refresh_ms.pop();
            }
            SettingsField::OfflineGrace => {
                self.settings_offline_grace_ms.pop();
            }
            SettingsField::AutoDiscovery => {}
        }
        self.settings_error = None;
    }

    pub fn settings_insert_char(&mut self, ch: char) {
        match self.settings_field {
            SettingsField::Host
                if !ch.is_control()
                    && !ch.is_whitespace()
                    && self.settings_host.chars().count() < 64 =>
            {
                self.settings_host.push(ch);
            }
            SettingsField::Port if ch.is_ascii_digit() && self.settings_port.len() < 5 => {
                self.settings_port.push(ch);
            }
            SettingsField::Gpu
                if !ch.is_control() && !ch.is_whitespace() && self.settings_gpu.len() < 40 =>
            {
                self.settings_gpu.push(ch);
            }
            SettingsField::Refresh if ch.is_ascii_digit() && self.settings_refresh_ms.len() < 6 => {
                self.settings_refresh_ms.push(ch);
            }
            SettingsField::ProcessRefresh
                if ch.is_ascii_digit() && self.settings_process_refresh_ms.len() < 6 =>
            {
                self.settings_process_refresh_ms.push(ch);
            }
            SettingsField::OfflineGrace
                if ch.is_ascii_digit() && self.settings_offline_grace_ms.len() < 6 =>
            {
                self.settings_offline_grace_ms.push(ch);
            }
            SettingsField::AutoDiscovery => {}
            _ => {}
        }
        self.settings_error = None;
    }

    pub fn settings_toggle_selected(&mut self) {
        if self.settings_field == SettingsField::AutoDiscovery {
            self.settings_auto_discovery = !self.settings_auto_discovery;
            self.settings_error = None;
        }
    }

    pub fn settings_config(&self) -> Result<AppConfig, String> {
        let endpoint = self.settings_endpoint()?;
        let gpu_selector = GpuSelector::parse(&self.settings_gpu);
        let refresh_ms = parse_setting_u64(
            &self.settings_refresh_ms,
            "Refresh",
            MIN_REFRESH_MS,
            MAX_REFRESH_MS,
        )?;
        let process_refresh_ms = parse_setting_u64(
            &self.settings_process_refresh_ms,
            "Process refresh",
            MIN_PROCESS_REFRESH_MS,
            MAX_PROCESS_REFRESH_MS,
        )?;
        let offline_grace_ms = parse_setting_u64(
            &self.settings_offline_grace_ms,
            "Offline grace",
            0,
            MAX_OFFLINE_GRACE_MS,
        )?;

        Ok(AppConfig {
            server: Some(endpoint),
            servers: self.settings_servers.clone(),
            gpu_selector,
            refresh_ms,
            process_refresh_ms,
            offline_grace_ms,
            auto_discovery: self.settings_auto_discovery,
        })
    }

    /// The endpoint to persist from the dialog.
    ///
    /// When the dialog was opened with a parseable URL, only host and port are
    /// edited: scheme, path, userinfo, query and fragment are preserved. If
    /// host and port are unchanged the exact original string is returned, so
    /// opening the dialog and saving without edits round-trips byte for byte
    /// (no silent `https` → `http` downgrade). Legacy endpoints without a
    /// scheme fall back to the historical `http://host:port` builder.
    fn settings_endpoint(&self) -> Result<String, String> {
        // `host_str` keeps the brackets of an IPv6 literal; compare and set
        // hosts in their bare form and re-add the brackets for the URL parser.
        let host = self
            .settings_host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']');
        if host.is_empty() {
            return Err("Host / IP must not be empty".to_string());
        }
        if host.chars().any(char::is_whitespace) {
            return Err("Host / IP contains invalid characters".to_string());
        }
        let port = self
            .settings_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Port must be between 1 and 65535".to_string())?;

        let Some(base) = &self.settings_server_base else {
            return build_endpoint(&self.settings_host, &self.settings_port);
        };

        let base_host = base
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']');
        let base_port = base.port_or_known_default().unwrap_or(0);
        if host.eq_ignore_ascii_case(base_host) && port == base_port {
            return Ok(self.settings_server_original.clone());
        }

        let mut url = base.clone();
        let host_for_url = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        url.set_host(Some(&host_for_url))
            .map_err(|_| "Host / IP is not a valid URL host".to_string())?;
        if default_port_for_scheme(url.scheme()) == Some(port) {
            url.set_port(None)
                .map_err(|_| "Port is not valid for this endpoint".to_string())?;
        } else {
            url.set_port(Some(port))
                .map_err(|_| "Port is not valid for this endpoint".to_string())?;
        }
        Ok(url.to_string())
    }

    pub fn set_settings_error(&mut self, error: String) {
        self.settings_error = Some(error);
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

    if area.width < 72 || area.height < 22 {
        draw_too_small(frame, area);
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

#[derive(Clone, Debug)]
struct ProcessGroup<'a> {
    program: &'a str,
    members: Vec<&'a ProcessStats>,
    cpu_pct: f64,
    memory_bytes: u64,
    gpu_bytes: u64,
    threads: Option<usize>,
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
        gpu_bytes: u64,
        threads: Option<usize>,
        expanded: bool,
    },
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
    let status = footer_status(llm, gpu);

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

    let help = if state.process_search_open {
        format!(
            " SEARCH /{}▏  [enter] apply  [esc] clear ",
            fit_cell(&state.process_search_query, 30)
        )
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
mod tests {
    use super::*;
    use crate::domain::{DeviceId, GpuEvidence, GpuVendor, MappedGpu};

    #[test]
    fn same_named_processes_are_grouped_with_aggregate_metrics() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "qemu-system-x86".to_string(),
                cpu_pct: 120.0,
                memory_bytes: 2_000,
                threads: Some(8),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "qemu-system-x86".to_string(),
                cpu_pct: 80.0,
                memory_bytes: 3_000,
                threads: Some(7),
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
                gpu_bytes,
                threads,
                expanded,
            } => {
                assert_eq!(*program, "qemu-system-x86");
                assert_eq!(*count, 2);
                assert_eq!(*cpu_pct, 200.0);
                assert_eq!(*memory_bytes, 5_000);
                assert_eq!(*gpu_bytes, 0);
                assert_eq!(*threads, Some(15));
                assert!(!expanded);
            }
            _ => panic!("expected grouped qemu row"),
        }
    }

    #[test]
    fn group_thread_total_is_unknown_when_any_member_is_unknown() {
        // A partial sum would present an unknown member as a verified total,
        // so the aggregate stays unknown until every member is readable.
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "qemu-system-x86".to_string(),
                threads: Some(4),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "qemu-system-x86".to_string(),
                threads: None,
                ..ProcessStats::default()
            },
        ];
        let expanded = HashSet::new();
        let (_, rows) =
            grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);
        match &rows[0] {
            ProcessDisplayRow::Group { threads, .. } => assert_eq!(*threads, None),
            _ => panic!("expected grouped row"),
        }
    }

    #[test]
    fn thread_cell_renders_unknown_as_dash() {
        assert_eq!(thread_cell(Some(12)), "12");
        assert_eq!(thread_cell(None), "—");
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
    fn process_search_matches_program_command_and_pid() {
        let process = ProcessStats {
            pid: 4242,
            program: "llama".to_string(),
            command: "/usr/local/bin/llama-server --model Qwen".to_string(),
            ..ProcessStats::default()
        };
        assert!(process_matches_search(&process, "LLAMA"));
        assert!(process_matches_search(&process, "qwen"));
        assert!(process_matches_search(&process, "424"));
        assert!(!process_matches_search(&process, "chromium"));
    }

    #[test]
    fn filtered_grouping_only_keeps_matching_processes() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 30,
                program: "llama".to_string(),
                command: "/usr/bin/llama-server".to_string(),
                ..ProcessStats::default()
            },
        ];
        let (_, rows) = grouped_process_rows_filtered(
            &processes,
            ProcessSortKey::Cpu,
            true,
            None,
            &HashSet::new(),
            Some("llama"),
        );
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0],
            ProcessDisplayRow::Process { process, .. } if process.pid == 30
        ));
    }

    #[test]
    fn keyboard_navigation_respects_process_search() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "llama".to_string(),
                ..ProcessStats::default()
            },
        ];
        let mut state = UiState {
            process_search_query: "llama".to_string(),
            ..UiState::default()
        };
        state.process_home(&processes);
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(20, 0))
        );
    }

    #[test]
    fn llm_rate_history_idle_requires_real_activity() {
        let mut history = VecDeque::new();
        history.push_back(TimedSample {
            at: Instant::now(),
            value: 0.0,
        });
        assert!(!llm_rate_history_has_activity(&history));
        history.push_back(TimedSample {
            at: Instant::now(),
            value: 12.5,
        });
        assert!(llm_rate_history_has_activity(&history));
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

        let (pinned, rest) = sorted_processes_with_pin(
            &processes,
            ProcessSortKey::Cpu,
            true,
            Some(ProcessIdentity::new(30, 0)),
        );
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
                targets: vec![
                    ProcessRowTarget::Process(ProcessIdentity::new(10, 0)),
                    ProcessRowTarget::Process(ProcessIdentity::new(20, 0)),
                ],
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
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(10, 0))
        );
        assert_eq!(state.process_pinned_pid, None);
    }

    #[test]
    fn double_click_pins_process() {
        let mut state = process_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(10, 0))
        );
        assert_eq!(state.process_pinned_pid, Some(ProcessIdentity::new(10, 0)));
    }

    #[test]
    fn clicking_another_process_releases_pin_and_selects_new_process() {
        let mut state = process_click_test_state();

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_pinned_pid, Some(ProcessIdentity::new(10, 0)));

        assert!(state.click_process_row(12, 11));
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(20, 0))
        );
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
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(30, 0))
        );
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
        assert!(matches!(
            state.process_selected_pid,
            Some(ProcessIdentity { pid: 10 | 20, .. })
        ));
        assert_eq!(state.process_selected_group, None);
    }

    #[test]
    fn keyboard_navigation_scrolls_selected_row_into_view() {
        let processes = (1..=8)
            .map(|pid| ProcessStats {
                pid,
                program: format!("proc-{pid}"),
                ..ProcessStats::default()
            })
            .collect::<Vec<_>>();
        let mut state = UiState {
            process_sort_key: ProcessSortKey::Pid,
            process_sort_desc: false,
            process_scroll_visible: 3,
            ..UiState::default()
        };

        state.process_home(&processes);
        for _ in 0..4 {
            state.move_process_selection(1, &processes);
        }

        assert_eq!(state.process_selected_pid, Some(ProcessIdentity::new(5, 0)));
        assert_eq!(state.process_scroll, 2);
    }

    #[test]
    fn hidden_selected_child_falls_back_to_collapsed_group() {
        let processes = vec![
            ProcessStats {
                pid: 10,
                program: "chromium".to_string(),
                ..ProcessStats::default()
            },
            ProcessStats {
                pid: 20,
                program: "chromium".to_string(),
                ..ProcessStats::default()
            },
        ];
        let mut state = UiState {
            process_selected_pid: Some(ProcessIdentity::new(10, 0)),
            process_scroll_visible: 10,
            ..UiState::default()
        };

        state.clamp_process_selection(&processes);

        assert_eq!(state.process_selected_pid, None);
        assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
    }

    #[test]
    fn selected_group_falls_back_to_single_process_when_group_disappears() {
        let processes = vec![ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        }];
        let mut state = UiState {
            process_selected_group: Some("chromium".to_string()),
            process_scroll_visible: 10,
            ..UiState::default()
        };

        state.clamp_process_selection(&processes);

        assert_eq!(state.process_selected_group, None);
        assert_eq!(
            state.process_selected_pid,
            Some(ProcessIdentity::new(10, 0))
        );
    }

    #[test]
    fn physical_core_average_combines_smt_threads() {
        let core = CpuPhysicalCore {
            kind: CpuCoreKind::Performance,
            logical_cpus: vec![0, 1],
        };
        assert_eq!(
            physical_core_average(&core, &[80.0, 20.0], &[true, true]),
            50.0
        );
    }

    #[test]
    fn core_heatmap_marks_offline_holes_instead_of_a_zero_percent_core() {
        let usages = [10.0, 0.0, 30.0, 40.0];
        let kinds = [
            CpuCoreKind::Performance,
            CpuCoreKind::Unknown,
            CpuCoreKind::Efficiency,
            CpuCoreKind::Efficiency,
        ];
        let online = [true, false, true, true];
        let rows = core_heatmap_rows(&usages, &kinds, &online, 80, Some(0), true, 4);
        let text: String = rows
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(
            text.contains("01?") && text.contains('—'),
            "offline hole must render as a muted dash, got: {text:?}"
        );
    }

    #[test]
    fn physical_core_average_skips_offline_smt_siblings() {
        // Thread 1 offline: its 0.0 placeholder must not halve the average.
        let core = CpuPhysicalCore {
            kind: CpuCoreKind::Performance,
            logical_cpus: vec![0, 1],
        };
        assert_eq!(
            physical_core_average(&core, &[80.0, 0.0], &[true, false]),
            80.0
        );
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
        assert_eq!(history.back().unwrap().value, 20.0);
    }

    #[test]
    fn llm_history_ignores_held_reconnect_samples() {
        let mut state = UiState::default();
        state.record_llm_sample(
            "srv",
            "srv",
            &LlmStats {
                connected: true,
                metrics_available: true,
                prompt_tps: Some(1200.0),
                generation_tps: Some(75.0),
                ..LlmStats::default()
            },
        );
        assert_eq!(state.llm_history.prefill.len(), 1);
        assert_eq!(state.llm_history.decode.len(), 1);
        assert!(state.llm_history.last_fresh_at.is_some());
        assert!(state.llm_history.connected_since.is_some());

        state.record_llm_sample(
            "srv",
            "srv",
            &LlmStats {
                connected: true,
                reconnecting: true,
                prompt_tps: Some(9999.0),
                generation_tps: Some(9999.0),
                ..LlmStats::default()
            },
        );
        assert_eq!(state.llm_history.prefill.len(), 1);
        assert_eq!(state.llm_history.decode.len(), 1);
    }

    #[test]
    fn llm_history_skips_samples_without_metrics() {
        // A reachable server whose /metrics is unavailable has unknown
        // throughput: no point may enter the rate history (that would draw a
        // fabricated flat line).
        let mut state = UiState::default();
        state.record_llm_sample(
            "srv",
            "srv",
            &LlmStats {
                connected: true,
                metrics_available: false,
                prompt_tps: Some(9999.0),
                generation_tps: Some(9999.0),
                ..LlmStats::default()
            },
        );
        assert!(state.llm_history.prefill.is_empty());
        assert!(state.llm_history.decode.is_empty());
        assert!(
            state.llm_history.last_fresh_at.is_some(),
            "the link is still fresh even without metrics"
        );
    }

    fn llm_with(prompt: f64, generation: f64) -> LlmStats {
        LlmStats {
            connected: true,
            metrics_available: true,
            prompt_tps: Some(prompt),
            generation_tps: Some(generation),
            ..LlmStats::default()
        }
    }

    #[test]
    fn per_server_histories_are_independent_and_survive_selection() {
        let mut state = UiState::default();
        // A accumulates while it is the selected server.
        state.record_llm_sample("A", "A", &llm_with(1200.0, 60.0));
        state.record_llm_sample("A", "A", &llm_with(1300.0, 70.0));
        assert_eq!(state.llm_history.prefill.len(), 2);

        // B is monitored in the background while A stays selected: B's sample
        // must not touch A's active view, but must be recorded for B.
        state.record_llm_sample("B", "A", &llm_with(10.0, 1.0));
        assert_eq!(
            state.llm_history.prefill.len(),
            2,
            "a background sample must not alter the selected server's history"
        );

        // Switch to B: B's own accumulated history is shown immediately.
        state.select_llm_history("B");
        assert_eq!(state.llm_history.prefill.len(), 1);
        // Switch back to A: A's history is preserved, not reset.
        state.select_llm_history("A");
        assert_eq!(state.llm_history.prefill.len(), 2);
    }

    #[test]
    fn reset_and_retain_bound_per_server_history() {
        let mut state = UiState::default();
        state.record_llm_sample("A", "A", &llm_with(1.0, 1.0));
        state.record_llm_sample("B", "A", &llm_with(1.0, 1.0));

        // A proven restart of B resets only B.
        state.reset_llm_history("B");
        state.select_llm_history("B");
        assert!(state.llm_history.prefill.is_empty());
        state.select_llm_history("A");
        assert_eq!(state.llm_history.prefill.len(), 1, "A is untouched");

        // Retaining only the live keys drops the servers that are gone.
        state.record_llm_sample("B", "A", &llm_with(2.0, 2.0));
        let live: HashSet<String> = ["A".to_string()].into_iter().collect();
        state.retain_llm_histories(&live);
        assert!(state.llm_histories.contains_key("A"));
        assert!(!state.llm_histories.contains_key("B"));
    }

    #[test]
    fn server_selector_navigation_wraps_and_clamps() {
        let mut state = UiState::default();
        assert!(!state.is_server_selector_open());
        state.open_server_selector(2);
        assert!(state.is_server_selector_open());
        assert_eq!(state.server_selector_index(3), Some(2), "opens on selected");
        state.move_server_selection(1, 3);
        assert_eq!(state.server_selector_index(3), Some(0), "wraps forward");
        state.move_server_selection(-1, 3);
        assert_eq!(state.server_selector_index(3), Some(2), "wraps backward");
        state.move_server_selection(1, 0);
        assert_eq!(state.server_selector_index(0), None);
        state.close_server_selector();
        assert!(!state.is_server_selector_open());
    }

    #[test]
    fn endpoint_is_compact() {
        assert_eq!(compact_endpoint("http://127.0.0.1:8081/"), "127.0.0.1:8081");
    }

    #[test]
    fn compact_endpoint_never_shows_credentials_path_or_query() {
        // Regression: the header used to paint the endpoint verbatim.
        assert_eq!(
            compact_endpoint("http://demo-user:demo-password@10.0.0.7:8080/v1?token=demo_secret"),
            "10.0.0.7:8080/…?…"
        );
        assert!(!compact_endpoint("https://user:s3cret@host:8443/x").contains("s3cret"));
        // A legacy scheme-less host:port stays readable.
        assert_eq!(compact_endpoint("127.0.0.1:8080"), "127.0.0.1:8080");
        // Anything ambiguous is hidden, never echoed.
        assert_eq!(
            compact_endpoint("localhost:8080?token=demo_secret"),
            crate::redact::UNPARSEABLE_ENDPOINT
        );
    }

    fn render_server_selector_text(
        servers: &[ServerSummary],
        selected: &str,
        area: Rect,
    ) -> String {
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_server_selector(frame, area, servers, selected, 0, &[]))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn summary(
        key: &str,
        connected: bool,
        slots_available: bool,
        busy: u64,
        total: u64,
    ) -> ServerSummary {
        ServerSummary {
            key: key.to_string(),
            endpoint: format!("http://127.0.0.1:{}", 8080 + key.len()),
            label: key.to_string(),
            connected,
            slots_available,
            busy_slots: busy,
            slot_count: total,
            ..Default::default()
        }
    }

    #[test]
    fn server_selector_renders_unavailable_slots_without_a_fake_zero() {
        // Regression for the M4 report: a connected server whose `/slots`
        // endpoint is down (metrics-only) appeared as "S 0/0". Unknown slots
        // must render `—/—`, never a fabricated zero.
        let area = Rect::new(0, 0, 80, 20);
        let metrics_only = vec![summary("metrics-only", true, false, 0, 0)];
        let text = render_server_selector_text(&metrics_only, "metrics-only", area);
        assert!(
            text.contains("S —/—"),
            "unavailable slots must render —/—:\n{text}"
        );
        assert!(
            !text.contains("S 0/0"),
            "unavailable slots must not fabricate 0/0:\n{text}"
        );

        // When /props still knows the total, the known capacity survives as
        // `—/N` rather than being dropped.
        let mut known_total = summary("metrics-only", true, false, 0, 0);
        known_total.props_slot_count = 3;
        let text = render_server_selector_text(&[known_total], "metrics-only", area);
        assert!(
            text.contains("S —/3"),
            "a known /props total must render as —/3:\n{text}"
        );
        assert!(!text.contains("S 0/0"), "{text}");

        // A valid empty `/slots` array is a genuine zero and stays numeric.
        let empty = vec![summary("empty", true, true, 0, 0)];
        let text = render_server_selector_text(&empty, "empty", area);
        assert!(
            text.contains("S 0/0"),
            "a real empty /slots array is 0/0:\n{text}"
        );
    }

    #[test]
    fn server_selector_shows_real_counts_and_no_cross_server_contamination() {
        let area = Rect::new(0, 0, 80, 20);
        let servers = vec![
            summary("healthy", true, true, 2, 4),
            summary("metrics-only", true, false, 0, 0),
            summary("dead", false, false, 0, 0),
        ];
        let text = render_server_selector_text(&servers, "healthy", area);
        assert!(text.contains("S 2/4  "), "healthy count lost:\n{text}");
        // Two unavailable rows, no fabricated 0/0 anywhere.
        assert!(!text.contains("S 0/0"), "fabricated zero leaked:\n{text}");
        assert_eq!(
            text.matches("S —/—").count(),
            2,
            "both unknown rows:\n{text}"
        );

        // A small but usable terminal still renders the row (the popup is
        // dropped only below its hard floor of a 48-column popup).
        let small = Rect::new(0, 0, 56, 10);
        let text = render_server_selector_text(&servers, "healthy", small);
        assert!(
            text.contains("S 2/4"),
            "small terminal lost the row:\n{text}"
        );

        // Below the floor it degrades quietly instead of panicking.
        let tiny = Rect::new(0, 0, 40, 4);
        let text = render_server_selector_text(&servers, "healthy", tiny);
        assert!(
            !text.contains("S 2/4"),
            "too-small popup should not draw:\n{text}"
        );
    }

    #[test]
    fn settings_endpoint_parses_and_rebuilds_ipv4() {
        assert_eq!(
            endpoint_parts("http://10.0.0.7:9090"),
            ("10.0.0.7".to_string(), 9090)
        );
        assert_eq!(
            build_endpoint("10.0.0.7", "9090").unwrap(),
            "http://10.0.0.7:9090"
        );
    }

    #[test]
    fn expanded_settings_build_runtime_config() {
        let mut state = UiState::default();
        let settings = AppConfig {
            server: Some("http://127.0.0.1:8081".to_string()),
            servers: Vec::new(),
            gpu_selector: GpuSelector::PciBusId("0000:41:00.0".to_string()),
            refresh_ms: 200,
            process_refresh_ms: 1500,
            offline_grace_ms: 3000,
            auto_discovery: false,
        };
        state.open_settings("http://127.0.0.1:8081", &settings);
        assert_eq!(state.settings_config().unwrap(), settings);
    }

    #[test]
    fn settings_auto_discovery_toggle_changes_saved_value() {
        let mut state = UiState {
            settings_field: SettingsField::AutoDiscovery,
            ..UiState::default()
        };
        let before = state.settings_auto_discovery;
        state.settings_toggle_selected();
        assert_ne!(state.settings_auto_discovery, before);
    }

    #[test]
    fn settings_endpoint_supports_ipv6() {
        assert_eq!(build_endpoint("::1", "8081").unwrap(), "http://[::1]:8081");
    }

    fn settings_for(server: &str) -> AppConfig {
        AppConfig {
            server: Some(server.to_string()),
            auto_discovery: false,
            ..AppConfig::default()
        }
    }

    #[test]
    fn opening_and_saving_an_https_endpoint_preserves_it() {
        let settings = settings_for("https://example.invalid:8443");
        let mut state = UiState::default();
        state.open_settings("https://example.invalid:8443", &settings);
        assert_eq!(
            state.settings_config().unwrap().server.as_deref(),
            Some("https://example.invalid:8443")
        );
    }

    #[test]
    fn changing_only_refresh_keeps_endpoint_scheme_and_path() {
        let server = "https://example.invalid:8443/v1?x=1";
        let settings = settings_for(server);
        let mut state = UiState::default();
        state.open_settings(server, &settings);
        // Change only the refresh interval.
        state.settings_field = SettingsField::Refresh;
        state.settings_refresh_ms = "500".to_string();
        let saved = state.settings_config().unwrap();
        assert_eq!(saved.refresh_ms, 500);
        assert_eq!(saved.server.as_deref(), Some(server));
    }

    #[test]
    fn https_without_an_explicit_port_round_trips() {
        let settings = settings_for("https://example.invalid");
        let mut state = UiState::default();
        state.open_settings("https://example.invalid", &settings);
        assert_eq!(state.settings_port, "443");
        assert_eq!(
            state.settings_config().unwrap().server.as_deref(),
            Some("https://example.invalid")
        );
    }

    #[test]
    fn editing_host_preserves_scheme_port_and_path() {
        let mut state = UiState::default();
        let settings = settings_for("https://old.invalid:8443/v1");
        state.open_settings("https://old.invalid:8443/v1", &settings);
        state.settings_host = "new.invalid".to_string();
        let endpoint = state.settings_config().unwrap().server.unwrap();
        let url = reqwest::Url::parse(&endpoint).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("new.invalid"));
        assert_eq!(url.port(), Some(8443));
        assert_eq!(url.path(), "/v1");
    }

    #[test]
    fn ipv6_and_hostname_endpoints_round_trip() {
        for server in ["https://[::1]:8443", "http://llama.local:9090"] {
            let settings = settings_for(server);
            let mut state = UiState::default();
            state.open_settings(server, &settings);
            assert_eq!(
                state.settings_config().unwrap().server.as_deref(),
                Some(server),
                "endpoint {server} changed"
            );
        }
    }

    #[test]
    fn trend_plot_keeps_requested_dimensions() {
        let now = Instant::now();
        let history = VecDeque::from([
            TimedSample {
                at: now - Duration::from_secs(30),
                value: 25.0,
            },
            TimedSample {
                at: now,
                value: 100.0,
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
                value: 25.0,
            },
            TimedSample {
                at: now - Duration::from_secs(10),
                value: 50.0,
            },
        ]);

        let columns = resample_history(&history, 7, now);
        assert_eq!(
            columns,
            vec![
                None,
                None,
                None,
                Some(25.0),
                Some(25.0),
                Some(50.0),
                Some(50.0),
            ]
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
    fn llm_sample_interval_ema_smooths_refresh_jitter() {
        let mut average = None;
        for sample in [100.0, 130.0, 70.0, 115.0, 85.0] {
            average = smooth_llm_sample_interval(average, sample);
        }
        let average = average.unwrap();
        assert!((95.0..=105.0).contains(&average));
    }

    #[test]
    fn llm_last_uses_smoothed_interval_while_online() {
        let state = UiState {
            llm_history: LlmHistory {
                sample_interval_ema_ms: Some(101.4),
                last_fresh_at: Some(Instant::now()),
                ..LlmHistory::default()
            },
            ..UiState::default()
        };
        let llm = LlmStats {
            connected: true,
            ..LlmStats::default()
        };
        assert_eq!(llm_last_sample_text(&state, &llm), "~101 ms avg");
    }

    #[test]
    fn llm_phase_marks_idle_without_activity() {
        let stats = LlmStats::default();
        let (phase, color) = llm_phase(&stats);
        assert_eq!(phase, "IDLE");
        assert_eq!(color, MUTED);
    }

    #[test]
    fn llm_phase_uses_a_reported_server_phase() {
        use crate::strata::StrataPhase;

        // Prefill is labeled PREFILL even though Strata reports no live prefill
        // rate; generating is GENERATING without needing a live rate.
        let reading = LlmStats {
            strata_phase: Some(StrataPhase::Reading),
            ..LlmStats::default()
        };
        assert_eq!(llm_phase(&reading).0, "PREFILL");

        let generating = LlmStats {
            strata_phase: Some(StrataPhase::Generating),
            ..LlmStats::default()
        };
        assert_eq!(llm_phase(&generating).0, "GENERATING");

        // An idle phase falls through to the existing heuristics (no activity
        // reported -> IDLE), and a programmatic rate still wins when there is
        // no phase.
        let idle = LlmStats {
            strata_phase: Some(StrataPhase::Idle),
            ..LlmStats::default()
        };
        assert_eq!(llm_phase(&idle).0, "IDLE");
        let rate_only = LlmStats {
            generation_tps: Some(50.0),
            ..LlmStats::default()
        };
        assert_eq!(llm_phase(&rate_only).0, "GENERATING");
    }

    #[test]
    fn spec_row_hints_when_enabled_but_counters_unavailable() {
        let llm = LlmStats {
            spec_enabled: true,
            ..LlmStats::default()
        };
        let row = llm_spec_row(&llm, 24, None);
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("no counters"), "row: {text:?}");
        assert!(text.contains("SPEC"));
    }

    #[test]
    fn spec_row_shows_values_when_any_counter_reported() {
        let llm = LlmStats {
            spec_enabled: true,
            spec_drafts_total: Some(3.0),
            spec_draft_tokens: Some(12.0),
            spec_accepted_tokens: Some(9.0),
            ..LlmStats::default()
        };
        let row = llm_spec_row(&llm, 24, Some(75.0));
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("12 draft"), "row: {text:?}");
        assert!(text.contains("9 accepted · 75.0%"), "row: {text:?}");
        assert!(!text.contains("b10700"));
    }

    #[test]
    fn spec_row_off_when_disabled() {
        let llm = LlmStats::default();
        let row = llm_spec_row(&llm, 24, None);
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("OFF"), "row: {text:?}");
        assert!(!text.contains("b10700"));
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

    #[test]
    fn io_row_renders_meter_or_dash_placeholder_at_fixed_width() {
        let base = GpuStats {
            available: true,
            index: 0,
            name: "Test".to_string(),
            ..Default::default()
        };
        let with_link = GpuStats {
            pcie_rx_mb_s: Some(15753.6),
            pcie_tx_mb_s: Some(15753.6),
            pcie_link_speed_gts: Some(16.0),
            pcie_link_width: Some(16),
            ..base.clone()
        };
        let without_link = GpuStats { ..base };

        let row = |gpu: &GpuStats| match gpu::pcie_utilization_pct(
            gpu.pcie_rx_mb_s,
            gpu.pcie_tx_mb_s,
            gpu.pcie_link_speed_gts,
            gpu.pcie_link_width,
        ) {
            Some(pct) => meter_line(
                "I/O",
                pct,
                14,
                CYAN,
                format!("{:>3.0}%", pct),
                vec![
                    fixed_data_pair(
                        "MEMCTRL",
                        optional_number(gpu.memory_utilization, 0, "%"),
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
                ],
            ),
            None => Line::from(vec![
                label_span(" I/O    "),
                Span::styled(
                    format!("{:<width$}", " \u{2014}", width = 14 + 11),
                    Style::default().fg(MUTED),
                ),
                fixed_data_pair(
                    "MEMCTRL",
                    optional_number(gpu.memory_utilization, 0, "%"),
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
        };
        assert_eq!(row(&with_link).width(), row(&without_link).width());
    }

    #[test]
    fn enc_dec_idle_detects_missing_or_sub_percent_utilization() {
        let gpu = |enc: Option<f64>, dec: Option<f64>| GpuStats {
            encoder_utilization: enc,
            decoder_utilization: dec,
            ..GpuStats::default()
        };

        assert!(enc_dec_idle(&gpu(None, None)));
        assert!(enc_dec_idle(&gpu(Some(0.0), Some(0.0))));
        assert!(enc_dec_idle(&gpu(Some(0.5), None)));
        assert!(!enc_dec_idle(&gpu(Some(1.0), None)));
        assert!(!enc_dec_idle(&gpu(None, Some(5.0))));
    }

    fn hybrid_system(perf: usize, eff: usize) -> SystemStats {
        let mut core_kinds = Vec::new();
        let mut physical_core_groups = Vec::new();
        for i in 0..perf {
            core_kinds.push(CpuCoreKind::Performance);
            physical_core_groups.push(CpuPhysicalCore {
                kind: CpuCoreKind::Performance,
                logical_cpus: vec![i],
            });
        }
        for i in 0..eff {
            core_kinds.push(CpuCoreKind::Efficiency);
            physical_core_groups.push(CpuPhysicalCore {
                kind: CpuCoreKind::Efficiency,
                logical_cpus: vec![perf + i],
            });
        }
        SystemStats {
            cpu_topology: CpuTopology {
                core_kinds,
                physical_core_groups,
                ..CpuTopology::default()
            },
            ..SystemStats::default()
        }
    }

    #[test]
    fn small_hybrid_box_keeps_the_full_view_with_the_minibar() {
        // Regression: a 4P+4E Core Ultra (the 288V) allocated
        // `system_panel_height` = 14 → inner 12, which the old hard-coded
        // `inner.height < 15` gate rejected, collapsing the panel to the
        // 4-line compact view and hiding the P/E minibar entirely. The 8P+8E
        // box (12900K) survived because inner 16 >= 15.
        let small = hybrid_system(4, 4);
        assert_eq!(system_panel_height(&small), 14);
        assert!(
            system_full_view(35, 14 - 2),
            "4P+4E inner height 12 must be the full view"
        );

        let wide = hybrid_system(8, 8);
        assert_eq!(system_panel_height(&wide), 18);
        assert!(
            system_full_view(35, 18 - 2),
            "8P+8E inner height 16 must stay the full view"
        );

        // Homogeneous boxes (panel 13 → inner 11) render the full view with
        // the per-core heatmap too.
        let homo = SystemStats::default();
        assert_eq!(system_panel_height(&homo), 13);
        assert!(system_full_view(35, 13 - 2));

        // Below the full-view floors the compact fallback wins: the minibar
        // needs the 35-column floor, and 8 rows cannot hold the fixed rows
        // plus a core row.
        assert!(!system_full_view(34, 20));
        assert!(!system_full_view(60, 8));
    }

    #[test]
    fn rendered_system_panel_shows_the_pe_minibar_on_a_small_hybrid_box() {
        // End-to-end regression for the 288V report: paint the real SYSTEM
        // panel at the size the layout allocates for a 4P+4E box (panel
        // height 14, inner 12) and assert the P/E minibar renders — it used
        // to fall through to the compact 4-line view.
        let system = hybrid_system(4, 4);
        let area = Rect::new(0, 0, 70, system_panel_height(&system));
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_system(frame, area, &system))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("P-CORES") && text.contains("E-CORES"),
            "P/E minibar header must render, got:\n{text}",
        );
    }

    fn render_system_rows(system: &SystemStats, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_system(frame, area, system))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let raw: String = (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                raw.trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn system_panel_renders_ram_unavailable_when_total_is_zero() {
        // Regression for review finding C3: with `memory_total_bytes == 0`
        // the SYSTEM panel must render the RAM meter as unavailable ("—")
        // like VRAM/POWER — never a fabricated 0.0 % / 0.0 GiB.
        let system = SystemStats {
            memory_used_bytes: 16 * 1024 * 1024 * 1024,
            memory_total_bytes: 0,
            ..Default::default()
        };

        // Full view (panel height 13, width 70).
        let rows = render_system_rows(&system, 70, 13);
        let ram_index = rows
            .iter()
            .position(|row| row.trim_start().starts_with("RAM"))
            .expect("RAM meter row must render");
        let ram_row = &rows[ram_index];
        assert!(
            ram_row.contains('—'),
            "RAM meter must render unavailable, got: {ram_row:?}"
        );
        assert!(
            !ram_row.contains("0.0%"),
            "RAM meter must not fabricate a 0.0 % value, got: {ram_row:?}"
        );
        // The used/total GiB pair row directly below the meter is
        // unavailable as well.
        let pair_row = rows
            .get(ram_index + 1)
            .expect("used/total GiB pair row must render");
        assert_eq!(
            pair_row, "—",
            "used/total GiB pair must be unavailable, got: {pair_row:?}"
        );

        // Compact fallback (below the full-view height floor) shares the
        // same validity semantics.
        let rows = render_system_rows(&system, 70, 8);
        let ram_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("RAM"))
            .expect("RAM meter row must render in compact view");
        assert!(
            ram_row.contains('—'),
            "compact RAM meter must render unavailable, got: {ram_row:?}"
        );
        assert!(
            !ram_row.contains("0.0%"),
            "compact RAM meter must not fabricate a 0.0 % value, got: {ram_row:?}"
        );
        let used_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("USED"))
            .expect("USED row must render in compact view");
        assert!(
            used_row.contains('—') && !used_row.contains("GiB"),
            "compact USED row must be unavailable, got: {used_row:?}"
        );
        let total_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("TOTAL"))
            .expect("TOTAL row must render in compact view");
        assert!(
            total_row.contains('—') && !total_row.contains("GiB"),
            "compact TOTAL row must be unavailable, got: {total_row:?}"
        );
    }

    #[test]
    fn system_panel_renders_ram_meter_when_total_is_valid() {
        // Existing behavior must be preserved: a valid total renders the
        // real percentage and GiB pair.
        let system = SystemStats {
            memory_used_bytes: 16 * 1024 * 1024 * 1024,
            memory_total_bytes: 64 * 1024 * 1024 * 1024,
            ..Default::default()
        };

        let rows = render_system_rows(&system, 70, 13);
        let ram_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("RAM"))
            .expect("RAM meter row must render");
        assert!(
            ram_row.contains("25.0%"),
            "RAM meter must show 25.0 %, got: {ram_row:?}"
        );
        assert!(
            !ram_row.contains('—'),
            "RAM meter must not render unavailable for a valid total, got: {ram_row:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("16.0 / 64.0 GiB")),
            "full view must show the used/total GiB pair, got: {rows:?}"
        );
    }

    #[test]
    fn system_panel_renders_load_average_unavailable_without_a_reading() {
        // Regression: an unreadable /proc/loadavg must render `—`, not three
        // fabricated zeros. A real zero stays a number.
        let unknown = SystemStats::default();
        let rows = render_system_rows(&unknown, 70, 13);
        let load_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("LOAD"))
            .expect("LOAD row must render");
        assert!(
            load_row.contains('—'),
            "unknown load must render —: {load_row:?}"
        );
        assert!(
            !load_row.contains("0.00"),
            "unknown load must not fabricate zeros: {load_row:?}"
        );

        let zero = SystemStats {
            load_one: Some(0.0),
            load_five: Some(0.0),
            load_fifteen: Some(0.0),
            ..Default::default()
        };
        let rows = render_system_rows(&zero, 70, 13);
        let load_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("LOAD"))
            .expect("LOAD row must render");
        assert!(
            load_row.contains("0.00 / 0.00 / 0.00"),
            "a real zero load must stay a number: {load_row:?}"
        );

        let known = SystemStats {
            load_one: Some(1.25),
            load_five: Some(0.75),
            load_fifteen: Some(0.5),
            ..Default::default()
        };
        let rows = render_system_rows(&known, 70, 13);
        let load_row = rows
            .iter()
            .find(|row| row.trim_start().starts_with("LOAD"))
            .expect("LOAD row must render");
        assert!(
            load_row.contains("1.25 / 0.75 / 0.50"),
            "known loads must render as read: {load_row:?}"
        );
    }

    #[test]
    fn ram_history_skips_samples_without_total() {
        // The history sparkline must follow the same validity semantics:
        // no total, no sample — never a fabricated zero.
        let mut state = UiState::default();
        let gpu = GpuStats::default();
        let mut system = SystemStats::default();

        state.push_sample(&gpu, &system);
        assert!(state.ram_history.is_empty());

        system.memory_used_bytes = 16 * 1024 * 1024 * 1024;
        system.memory_total_bytes = 64 * 1024 * 1024 * 1024;
        state.push_sample(&gpu, &system);
        assert_eq!(state.ram_history.len(), 1);
        assert!((state.ram_history[0].value - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gpu_in_mapping_matches_only_the_mapped_device() {
        let gpu = GpuStats {
            device: DeviceId::new(Some("0000:01:00.0".into()), None),
            ..Default::default()
        };

        let mapped = MappedGpu {
            device: gpu.device.clone(),
            name: "card0".to_string(),
            vendor: GpuVendor::Nvidia,
            evidence: GpuEvidence::NvmlCompute,
        };
        let other = MappedGpu {
            device: DeviceId::new(Some("0000:02:00.0".into()), None),
            ..mapped.clone()
        };

        assert!(gpu_in_mapping(&gpu, &GpuMapping::Single(mapped.clone())));
        assert!(gpu_in_mapping(
            &gpu,
            &GpuMapping::Multi(vec![other.clone(), mapped])
        ));
        assert!(!gpu_in_mapping(&gpu, &GpuMapping::Single(other)));
        assert!(!gpu_in_mapping(&gpu, &GpuMapping::None));
        assert!(!gpu_in_mapping(&gpu, &GpuMapping::Unknown));

        // A GPU without a stable key never matches — even against a mapping
        // entry that is itself unkeyed.
        let unkeyed = GpuStats::default();
        let unkeyed_map = MappedGpu {
            device: DeviceId::default(),
            name: "card1".to_string(),
            vendor: GpuVendor::Amd,
            evidence: GpuEvidence::RenderNodeFd,
        };
        assert!(!gpu_in_mapping(&unkeyed, &GpuMapping::Single(unkeyed_map)));
    }

    #[test]
    fn gpu_in_mapping_matches_mig_parent_and_child() {
        let parent_uuid = "GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789";
        let parent = GpuStats {
            device: DeviceId::new(Some("0000:41:00.0".into()), Some(parent_uuid.into())),
            ..Default::default()
        };
        let child = GpuStats {
            device: DeviceId::new(None, Some(format!("MIG-{parent_uuid}/1/0"))),
            ..Default::default()
        };
        let parent_mapped = MappedGpu {
            device: parent.device.clone(),
            name: String::new(),
            vendor: GpuVendor::Nvidia,
            evidence: GpuEvidence::NvmlCompute,
        };
        let child_mapped = MappedGpu {
            device: child.device.clone(),
            ..parent_mapped.clone()
        };

        // Server attributed to the physical parent: chip on the child panel.
        assert!(gpu_in_mapping(
            &child,
            &GpuMapping::Single(parent_mapped.clone())
        ));
        // Server attributed to the child: chip on the parent panel.
        assert!(gpu_in_mapping(
            &parent,
            &GpuMapping::Single(child_mapped.clone())
        ));
        // The same child matches directly as well.
        assert!(gpu_in_mapping(
            &child,
            &GpuMapping::Single(child_mapped.clone())
        ));
        // A child of a different physical GPU never matches.
        let foreign = GpuStats {
            device: DeviceId::new(None, Some("MIG-GPU-other/1/0".into())),
            ..Default::default()
        };
        assert!(!gpu_in_mapping(&foreign, &GpuMapping::Single(child_mapped)));
    }

    #[test]
    fn gpu_attribution_label_shows_single_key_and_visible_multi_count() {
        let mapped = MappedGpu {
            device: DeviceId::new(Some("0000:01:00.0".into()), None),
            name: "card0".to_string(),
            vendor: GpuVendor::Nvidia,
            evidence: GpuEvidence::NvmlCompute,
        };
        let other = MappedGpu {
            device: DeviceId::new(Some("0000:02:00.0".into()), None),
            ..mapped.clone()
        };

        assert_eq!(gpu_attribution_label(&GpuMapping::None), None);
        assert_eq!(gpu_attribution_label(&GpuMapping::Unknown), None);
        assert_eq!(
            gpu_attribution_label(&GpuMapping::Single(mapped.clone())),
            Some("0000:01:00.0".to_string())
        );

        // Multi must not silently look like a single GPU: the count is visible.
        let label = gpu_attribution_label(&GpuMapping::Multi(vec![mapped, other])).unwrap();
        assert!(
            label.contains("+1"),
            "multi attribution must show the GPU count"
        );
    }

    #[test]
    fn gpu_panel_title_shows_the_llm_chip_when_mapped() {
        let gpu = GpuStats {
            available: true,
            name: "RTX 4090".to_string(),
            device: DeviceId::new(Some("0000:01:00.0".into()), None),
            ..Default::default()
        };
        let mapped = MappedGpu {
            device: gpu.device.clone(),
            name: "card0".to_string(),
            vendor: GpuVendor::Nvidia,
            evidence: GpuEvidence::NvmlCompute,
        };
        let area = Rect::new(0, 0, 40, 7);

        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_gpu(frame, area, &gpu, &GpuMapping::Single(mapped)))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("LLM"),
            "mapped GPU title must carry the LLM chip, got:\n{text}",
        );

        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_gpu(frame, area, &gpu, &GpuMapping::None))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !text.contains("LLM"),
            "unmapped GPU title must not carry the LLM chip, got:\n{text}",
        );
    }

    fn render_gpu_panel(gpu: &GpuStats, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_gpu(frame, area, gpu, &GpuMapping::None))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn gpu_panel_renders_pwr_unavailable_without_power_telemetry() {
        // Regression: the PWR meter was built unconditionally from
        // `power_pct.unwrap_or(0.0)`, so a driver that exposes no power
        // telemetry (e.g. the Intel xe iGPU) rendered an empty bar with
        // "0%" — indistinguishable from a real 0 W reading. It must render
        // unavailable ("—") like the GPU/VRAM rows do.
        let gpu = GpuStats {
            available: true,
            name: "Intel Arc (Core Ultra 200V iGPU)".to_string(),
            utilization: Some(12.0),
            ..Default::default()
        };
        let text = render_gpu_panel(&gpu, 80, 8);
        assert!(
            text.contains("PWR"),
            "PWR row must still render, got:\n{text}"
        );
        assert!(
            !text.contains("0%"),
            "PWR must not fabricate a 0% reading, got:\n{text}"
        );
        assert!(
            text.matches('—').count() >= 3,
            "PWR/VRAM/DRAW must all render unavailable, got:\n{text}"
        );
    }

    #[test]
    fn gpu_panel_renders_pwr_meter_when_power_is_reported() {
        // Guard the other direction: with real power telemetry the meter
        // still renders a percentage instead of degrading to "—".
        let gpu = GpuStats {
            available: true,
            name: "RTX 4090".to_string(),
            power_w: Some(150.0),
            power_limit_w: Some(450.0),
            ..Default::default()
        };
        let text = render_gpu_panel(&gpu, 80, 8);
        assert!(
            text.contains("PWR") && text.contains("33%"),
            "PWR meter must render the real percentage, got:\n{text}"
        );
    }

    fn render_llm_panel(llm: &LlmStats, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_llm(frame, area, llm, &UiState::default(), &GpuMapping::None))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn llm_panel_ctx_row_names_the_selected_slot_on_multi_slot_servers() {
        let llm = LlmStats {
            connected: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(78800),
            metrics_available: true,
            slots_available: true,
            slot_count: 2,
            busy_slots: 1,
            context_slot_id: Some(1),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            text.contains("S1"),
            "multi-slot panel must tag the selected slot, got:\n{text}"
        );
        assert!(
            text.contains("78,800 / 115,200 tok"),
            "context pair must stay readable next to the tag, got:\n{text}"
        );
    }

    #[test]
    fn llm_panel_single_slot_omits_the_redundant_slot_tag() {
        let llm = LlmStats {
            connected: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(28851),
            metrics_available: true,
            slots_available: true,
            slot_count: 1,
            context_slot_id: Some(0),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            !text.contains("S0"),
            "single-slot panel must not show a redundant S0, got:\n{text}"
        );
        assert!(text.contains("28,851 / 115,200 tok"));
    }

    #[test]
    fn llm_panel_without_slot_id_never_prints_a_fake_slot() {
        let llm = LlmStats {
            connected: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(5000),
            metrics_available: true,
            slots_available: true,
            slot_count: 2,
            busy_slots: 1,
            context_slot_id: None,
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            !text.contains("S0") && !text.contains("S1"),
            "unknown slot identity must not be guessed, got:\n{text}"
        );
        assert!(text.contains("5,000 / 115,200 tok"));
    }

    #[test]
    fn narrow_llm_panel_drops_the_slot_tag_before_the_context_values() {
        let llm = LlmStats {
            connected: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(28851),
            metrics_available: true,
            slots_available: true,
            slot_count: 2,
            busy_slots: 1,
            context_slot_id: Some(0),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 40, 12);
        assert!(
            !text.contains("S0"),
            "below the bar-pin width the tag must be dropped, got:\n{text}"
        );
    }

    #[test]
    fn llm_panel_shows_slots_but_marks_metrics_unavailable() {
        // Reachable server without /metrics: the slot/context state stays
        // visible, the metric rows are unavailable (absent), never fake zeros.
        let llm = LlmStats {
            connected: true,
            metrics_available: false,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(58745),
            slots_available: true,
            slot_count: 2,
            busy_slots: 1,
            context_slot_id: Some(0),
            slot_overview: overview_slots(),
            error: "/metrics disabled; start llama.cpp with --metrics".to_string(),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(text.contains("UNAVAILABLE"), "got:\n{text}");
        assert!(
            text.contains("58,745 / 115,200 tok"),
            "the CTX pair from /slots must stay visible, got:\n{text}"
        );
        assert!(
            text.contains("S0 58.7k*"),
            "the per-slot overview must stay visible, got:\n{text}"
        );
        assert!(
            !text.contains("tok/s"),
            "no metric rate may be fabricated without /metrics, got:\n{text}"
        );
        // The metric-derived rows are omitted entirely, so no total/avg/
        // request cell can show a fabricated value.
        assert!(
            !text.contains("TOTAL") && !text.contains("AVG") && !text.contains("REQUEST"),
            "no metric row may be fabricated without /metrics, got:\n{text}"
        );
    }

    #[test]
    fn unknown_context_occupancy_renders_the_ctx_meter_unavailable() {
        let llm = LlmStats {
            connected: true,
            metrics_available: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: None,
            slots_available: true,
            slot_count: 1,
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            !text.contains("0.0%"),
            "unknown occupancy must not render 0.0%:\n{text}"
        );
        assert!(text.contains("— / 115,200 tok"), "got:\n{text}");
    }

    #[test]
    fn known_zero_context_occupancy_still_renders_zero_percent() {
        let llm = LlmStats {
            connected: true,
            metrics_available: true,
            model: "test-model".to_string(),
            context_size: 4096,
            context_used: Some(0),
            slots_available: true,
            slot_count: 1,
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(text.contains("0.0%"), "a real zero is 0.0%:\n{text}");
        assert!(text.contains("0 / 4,096 tok"), "got:\n{text}");
    }

    #[test]
    fn context_without_capacity_renders_the_meter_unavailable() {
        let llm = LlmStats {
            connected: true,
            metrics_available: true,
            model: "test-model".to_string(),
            context_size: 0,
            context_used: Some(5),
            slots_available: false,
            context_high_watermark: None,
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            !text.contains("0.0%"),
            "no capacity means the percentage is unknown:\n{text}"
        );
    }

    #[test]
    fn absent_metrics_render_as_unavailable_not_zero() {
        // Reachable server, but every /metrics-derived metric is absent: the
        // panel must show `—`, never a fabricated 0 / 0.0 / 0 tok/s.
        let llm = LlmStats {
            connected: true,
            metrics_available: true,
            model: "test-model".to_string(),
            context_size: 4096,
            context_used: Some(100),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 100, 14);
        assert!(!text.contains("0 tok"), "no fabricated total: {text}");
        assert!(!text.contains("0.0 tok/s"), "no fabricated rate: {text}");
        assert!(!text.contains("0.0 s"), "no fabricated time: {text}");
        assert!(
            !text.contains("0/0"),
            "no fabricated request counts: {text}"
        );
        assert!(text.contains('—'), "unavailable values must render —");
    }

    #[test]
    fn incomplete_request_totals_render_unavailable_not_zero() {
        // A busy slot whose prompt total is incomplete (None) renders —, while
        // an independent real zero decode total still renders 0 tok.
        let llm = LlmStats {
            connected: true,
            metrics_available: true,
            model: "test-model".to_string(),
            busy_slots: 1,
            request_prompt_tokens: None,
            request_generated_tokens: Some(0),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 100, 14);
        assert!(text.contains("REQUEST"), "request row must render:\n{text}");
        assert!(
            text.contains("— tok"),
            "incomplete total must be —:\n{text}"
        );
        assert!(
            text.contains("0 tok"),
            "a real zero request count must render 0:\n{text}"
        );
    }

    #[test]
    fn metrics_unavailable_panel_with_slots_keeps_context_unknown() {
        let llm = LlmStats {
            connected: true,
            metrics_available: false,
            model: "test-model".to_string(),
            context_size: 8192,
            context_used: None,
            slots_available: true,
            slot_count: 1,
            error: "/metrics disabled; start llama.cpp with --metrics".to_string(),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(text.contains("METRICS"), "got:\n{text}");
        assert!(!text.contains("0.0%"), "got:\n{text}");
    }

    #[test]
    fn metrics_unavailable_panel_never_claims_the_server_is_offline() {
        // A transient /metrics transport failure while /slots answers must not
        // render the contradictory pair LINK ONLINE + "LLAMA SERVER OFFLINE".
        let llm = LlmStats {
            connected: true,
            metrics_available: false,
            model: "test-model".to_string(),
            error: "cannot reach llama.cpp: connection refused".to_string(),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, 90, 12);
        assert!(text.contains("METRICS UNREACHABLE"), "got:\n{text}");
        assert!(
            !text.contains("SERVER OFFLINE"),
            "a reachable server must not be reported offline, got:\n{text}"
        );
    }

    #[test]
    fn footer_marks_a_reachable_metrics_failure_as_metrics_unreachable() {
        let llm = LlmStats {
            connected: true,
            metrics_available: false,
            error: "cannot reach llama.cpp: connection refused".to_string(),
            ..Default::default()
        };
        let status = footer_status(&llm, &GpuStats::default());
        assert!(status.contains("METRICS UNREACHABLE"), "got: {status}");
        assert!(!status.contains("SERVER OFFLINE"), "got: {status}");
    }

    #[test]
    fn footer_still_reports_an_unreachable_server_as_offline() {
        let llm = LlmStats {
            connected: false,
            metrics_available: false,
            error: "cannot reach llama.cpp: connection refused".to_string(),
            ..Default::default()
        };
        let status = footer_status(&llm, &GpuStats::default());
        assert!(status.contains("SERVER OFFLINE"), "got: {status}");
    }

    fn overview_slots() -> Vec<LlmSlotInfo> {
        vec![
            LlmSlotInfo {
                id: 0,
                busy: true,
                context_used: Some(58745),
                context_size: Some(115200),
            },
            LlmSlotInfo {
                id: 1,
                busy: false,
                context_used: Some(34200),
                context_size: Some(115200),
            },
        ]
    }

    fn overview_llm(overview: Vec<LlmSlotInfo>, selected: Option<u64>) -> LlmStats {
        LlmStats {
            connected: true,
            model: "test-model".to_string(),
            context_size: 115200,
            context_used: Some(58745),
            metrics_available: true,
            slots_available: true,
            slot_count: overview.len() as u64,
            busy_slots: overview.iter().filter(|slot| slot.busy).count() as u64,
            context_slot_id: selected,
            slot_overview: overview,
            ..Default::default()
        }
    }

    #[test]
    fn slot_overview_entries_format_ids_usage_and_selected_marker() {
        let entries = slot_overview_entries(&overview_slots(), Some(0));
        assert_eq!(entries[0].0, "S0 58.7k*");
        assert_eq!(entries[0].1, WHITE, "busy slot is highlighted");
        assert_eq!(entries[1].0, "S1 34.2k");
        assert_eq!(entries[1].1, MUTED, "idle slot is dimmed");

        // The marker follows context_slot_id, never a fixed position.
        let entries = slot_overview_entries(&overview_slots(), Some(1));
        assert_eq!(entries[0].0, "S0 58.7k");
        assert_eq!(entries[1].0, "S1 34.2k*");

        // An unknown selection marks nothing.
        let entries = slot_overview_entries(&overview_slots(), None);
        assert!(entries.iter().all(|(text, _)| !text.ends_with('*')));
    }

    #[test]
    fn slot_overview_entries_show_per_slot_capacity_when_capacities_differ() {
        let mut slots = vec![
            LlmSlotInfo {
                id: 0,
                busy: true,
                context_used: Some(58745),
                context_size: Some(115200),
            },
            LlmSlotInfo {
                id: 3,
                busy: false,
                context_used: Some(34200),
                context_size: Some(70144),
            },
        ];
        let entries = slot_overview_entries(&slots, Some(0));
        assert_eq!(entries[0].0, "S0 58.7/115.2k*");
        assert_eq!(entries[1].0, "S3 34.2/70.1k");

        // A missing capacity is shown as unknown, not hidden or faked.
        slots[1].context_size = None;
        let entries = slot_overview_entries(&slots, Some(0));
        assert_eq!(entries[1].0, "S3 34.2/—");
    }

    #[test]
    fn slot_overview_entries_never_print_a_fake_zero_for_missing_usage() {
        let slots = vec![
            LlmSlotInfo {
                id: 0,
                busy: false,
                context_used: None,
                context_size: Some(115200),
            },
            LlmSlotInfo {
                id: 7,
                busy: false,
                context_used: Some(999),
                context_size: Some(115200),
            },
        ];
        let entries = slot_overview_entries(&slots, Some(0));
        assert_eq!(entries[0].0, "S0 —*");
        assert_eq!(entries[1].0, "S7 999");
    }

    #[test]
    fn slot_overview_line_truncates_many_slots_with_visible_remainder() {
        let slots = (0..5)
            .map(|id| LlmSlotInfo {
                id,
                busy: id % 2 == 0,
                context_used: Some(10000 + id * 1000),
                context_size: Some(115200),
            })
            .collect::<Vec<_>>();
        // Entry widths: S0 10.0k* (9) + S1..S4 (8 each); 12 (label) + 3
        // (gap) + 4 (suffix). Width 55 must drop entries from the end and
        // name the remainder: 3 entries + `+2` = 47 cells fit, 4 entries
        // + `+1` = 58 do not.
        let line = llm_slot_overview_line(&slots, Some(0), 55).expect("row must fit");
        let text: String = line.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("S0 10.0k*"), "got: {text}");
        assert!(
            text.contains("+2"),
            "hidden entries must be named, got: {text}"
        );
        assert!(!text.contains("S4 "), "got: {text}");
        // Deterministic: the same input always yields the same row.
        let again = llm_slot_overview_line(&slots, Some(0), 55).unwrap();
        let again_text: String = again.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, again_text);
    }

    #[test]
    fn slot_overview_line_fits_exactly_at_the_width_boundary() {
        // 4 entries (8 cells each) + `  +1` (4) = 12 + 32 + 9 + 4 = exactly
        // 57 cells: the remainder marker must survive intact at the boundary
        // and the row must fall back one entry below it.
        let slots = (0..5)
            .map(|id| LlmSlotInfo {
                id,
                busy: false,
                context_used: Some(10000 + id * 1000),
                context_size: Some(115200),
            })
            .collect::<Vec<_>>();
        let line = llm_slot_overview_line(&slots, None, 57).expect("exact fit must pass");
        let text: String = line.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("S3 13.0k"), "got: {text}");
        assert!(text.ends_with("  +1"), "got: {text}");
        assert!(!text.contains("S4 "), "got: {text}");
        // One cell less: the suffixed 4-entry row (57) no longer fits, so
        // the row falls back to 3 entries + `  +2` = exactly 46 cells.
        let line = llm_slot_overview_line(&slots, None, 56).unwrap();
        let text: String = line.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("S2 12.0k"), "got: {text}");
        assert!(text.ends_with("  +2"), "got: {text}");
        assert!(!text.contains("S3 "), "got: {text}");
        // And 3 entries + `  +2` = 46 cells still fits at width 46.
        let line = llm_slot_overview_line(&slots, None, 46).unwrap();
        let text: String = line.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.ends_with("  +2"), "got: {text}");
        assert!(!text.contains("S3 "), "got: {text}");
    }

    #[test]
    fn slot_overview_line_omitted_when_nothing_fits() {
        let line = llm_slot_overview_line(&overview_slots(), Some(0), 20);
        assert!(
            line.is_none(),
            "a 20-cell pane cannot hold label + one entry"
        );
        assert!(llm_slot_overview_line(&[], Some(0), 120).is_none());
    }

    #[test]
    fn llm_panel_multi_slot_shows_per_slot_overview_below_the_ctx_row() {
        let llm = overview_llm(overview_slots(), Some(0));
        let text = render_llm_panel(&llm, 90, 12);
        assert!(text.contains("S0 58.7k*"), "got:\n{text}");
        assert!(text.contains("S1 34.2k"), "got:\n{text}");
        // The main CTX row stays authoritative and unchanged.
        assert!(
            text.contains("58,745 / 115,200 tok"),
            "main CTX pair must stay readable, got:\n{text}"
        );
    }

    #[test]
    fn llm_panel_single_slot_never_shows_the_overview() {
        let mut llm = overview_llm(overview_slots(), Some(0));
        llm.slot_count = 1;
        llm.slot_overview.truncate(1);
        let text = render_llm_panel(&llm, 90, 12);
        assert!(
            !text.contains("S0"),
            "single-slot panels must not gain a redundant overview, got:\n{text}"
        );
    }

    #[test]
    fn narrow_llm_panel_omits_the_slot_overview_safely() {
        let llm = overview_llm(overview_slots(), Some(0));
        // Width 20 fits the CTX row but not even one overview entry.
        let text = render_llm_panel(&llm, 20, 12);
        assert!(
            !text.contains("58.7k"),
            "the overview must be dropped before corrupting the panel, got:\n{text}"
        );
    }
}
