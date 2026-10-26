//! `UiState` process-table navigation, selection, search and click handling.
use super::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum ProcessSortKey {
    Pid,
    Program,
    Cpu,
    Memory,
    Threads,
}

#[derive(Copy, Clone, Debug)]
pub(super) struct ProcessHeaderHit {
    pub(super) rect: Rect,
    pub(super) key: ProcessSortKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProcessRowTarget {
    Process(ProcessIdentity),
    Group(String),
}

#[derive(Clone, Debug)]
pub(super) struct ProcessRowsHit {
    pub(super) rect: Rect,
    pub(super) targets: Vec<ProcessRowTarget>,
}

#[derive(Clone, Debug)]
pub(super) struct ProcessGroup<'a> {
    pub(super) program: &'a str,
    pub(super) members: Vec<&'a ProcessStats>,
    pub(super) cpu_pct: f64,
    pub(super) memory_bytes: u64,
    pub(super) gpu_bytes: u64,
    pub(super) threads: Option<usize>,
    pub(super) min_pid: u32,
}

#[derive(Clone, Debug)]
pub(super) enum ProcessDisplayRow<'a> {
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

impl UiState {
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

    pub(crate) fn process_search_query(&self) -> Option<&str> {
        let query = self.process_search_query.trim();
        (!query.is_empty()).then_some(query)
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

    pub(crate) fn clear_process_interaction(&mut self) {
        self.process_header_hits.clear();
        self.process_pane = None;
        self.process_rows = None;
    }
}
