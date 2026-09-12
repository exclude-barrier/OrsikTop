from pathlib import Path

ui = Path('src/ui.rs')
text = ui.read_text()

text = text.replace(
'''#[derive(Copy, Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    start: usize,
}
''',
'''#[derive(Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    pids: Vec<u32>,
}
''',
1,
)

text = text.replace(
'''    process_selected: usize,
    process_sort_key: ProcessSortKey,
''',
'''    process_selected_pid: Option<u32>,
    process_scroll: usize,
    process_sort_key: ProcessSortKey,
''',
1,
)

text = text.replace(
'''            process_selected: 0,
            process_sort_key: ProcessSortKey::Cpu,
''',
'''            process_selected_pid: None,
            process_scroll: 0,
            process_sort_key: ProcessSortKey::Cpu,
''',
1,
)

start = text.index('    pub fn move_process_selection(')
end = text.index('    pub fn process_pane_contains', start)
replacement = '''    pub fn move_process_selection(&mut self, delta: isize, processes: &[ProcessStats]) {
        let sorted = sorted_processes(processes, self.process_sort_key, self.process_sort_desc);
        if sorted.is_empty() {
            self.process_selected_pid = None;
            self.process_scroll = 0;
            return;
        }

        let current = self
            .process_selected_pid
            .and_then(|pid| sorted.iter().position(|process| process.pid == pid))
            .unwrap_or_else(|| self.process_scroll.min(sorted.len() - 1));
        let target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(sorted.len() - 1)
        };
        self.process_selected_pid = Some(sorted[target].pid);
    }

    pub fn process_home(&mut self, processes: &[ProcessStats]) {
        let sorted = sorted_processes(processes, self.process_sort_key, self.process_sort_desc);
        self.process_selected_pid = sorted.first().map(|process| process.pid);
        self.process_scroll = 0;
    }

    pub fn process_end(&mut self, processes: &[ProcessStats]) {
        let sorted = sorted_processes(processes, self.process_sort_key, self.process_sort_desc);
        self.process_selected_pid = sorted.last().map(|process| process.pid);
        self.process_scroll = sorted.len().saturating_sub(1);
    }

    pub fn clamp_process_selection(&mut self, processes: &[ProcessStats]) {
        if self
            .process_selected_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_selected_pid = None;
        }
        self.process_scroll = self.process_scroll.min(processes.len().saturating_sub(1));
    }

    pub fn scroll_processes(&mut self, delta: isize, total: usize) {
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
    }

'''
text = text[:start] + replacement + text[end:]

text = text.replace(
'''        self.process_selected = 0;
        true
    }

    pub fn click_process_row(&mut self, x: u16, y: u16, total: usize) -> bool {
        let Some(rows) = self.process_rows else {
            return false;
        };
        if !rect_contains(rows.rect, x, y) {
            return false;
        }
        let index = rows.start + y.saturating_sub(rows.rect.y) as usize;
        if index >= total {
            return false;
        }
        self.process_selected = index;
        true
    }
''',
'''        if self.process_selected_pid.is_none() {
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
        let Some(pid) = rows.pids.get(row).copied() else {
            return false;
        };
        self.process_selected_pid = Some(pid);
        true
    }
''',
1,
)

old = '''    let sorted = sorted_processes(processes, state.process_sort_key, state.process_sort_desc);
    let visible = area.height.saturating_sub(3) as usize;
    let selected = state.process_selected.min(sorted.len().saturating_sub(1));
    let max_start = sorted.len().saturating_sub(visible);
    let start = if visible == 0 {
        0
    } else {
        selected.saturating_sub(visible / 2).min(max_start)
    };
'''
new = '''    let sorted = sorted_processes(processes, state.process_sort_key, state.process_sort_desc);
    let visible = area.height.saturating_sub(3) as usize;
    let mut selected_index = state
        .process_selected_pid
        .and_then(|pid| sorted.iter().position(|process| process.pid == pid));
    if state.process_selected_pid.is_some() && selected_index.is_none() {
        state.process_selected_pid = None;
        selected_index = None;
    }
    let max_start = sorted.len().saturating_sub(visible);
    let mut start = state.process_scroll.min(max_start);
    if visible > 0 {
        if let Some(selected) = selected_index {
            if selected < start {
                start = selected;
            } else if selected >= start.saturating_add(visible) {
                start = selected.saturating_add(1).saturating_sub(visible);
            }
        }
    } else {
        start = 0;
    }
    state.process_scroll = start;
'''
assert old in text
text = text.replace(old, new, 1)

old = '''    state.process_rows = Some(ProcessRowsHit { rect: body, start });

    let mut lines = Vec::with_capacity(visible);
    for (index, process) in sorted.iter().enumerate().skip(start).take(visible) {
        let is_selected = index == selected;
'''
new = '''    state.process_rows = Some(ProcessRowsHit {
        rect: body,
        pids: sorted
            .iter()
            .skip(start)
            .take(visible)
            .map(|process| process.pid)
            .collect(),
    });

    let mut lines = Vec::with_capacity(visible);
    for process in sorted.iter().skip(start).take(visible) {
        let is_selected = state.process_selected_pid == Some(process.pid);
'''
assert old in text
text = text.replace(old, new, 1)

ui.write_text(text)

app = Path('src/app.rs')
text = app.read_text()
text = text.replace(
'            ui_state.clamp_process_selection(next.system.processes.len());',
'            ui_state.clamp_process_selection(&next.system.processes);',
1,
)
text = text.replace(
'                        ui_state.move_process_selection(-1, snapshot.system.processes.len());',
'                        ui_state.move_process_selection(-1, &snapshot.system.processes);',
1,
)
text = text.replace(
'                        ui_state.move_process_selection(1, snapshot.system.processes.len());',
'                        ui_state.move_process_selection(1, &snapshot.system.processes);',
1,
)
text = text.replace(
'                        ui_state.move_process_selection(-10, snapshot.system.processes.len());',
'                        ui_state.move_process_selection(-10, &snapshot.system.processes);',
1,
)
text = text.replace(
'                        ui_state.move_process_selection(10, snapshot.system.processes.len());',
'                        ui_state.move_process_selection(10, &snapshot.system.processes);',
1,
)
text = text.replace(
'                    KeyCode::Home => ui_state.process_home(),\n                    KeyCode::End => ui_state.process_end(snapshot.system.processes.len()),',
'                    KeyCode::Home => ui_state.process_home(&snapshot.system.processes),\n                    KeyCode::End => ui_state.process_end(&snapshot.system.processes),',
1,
)

old = '''                    MouseEventKind::Down(MouseButton::Left) => {
                        let (width, _) = crossterm::terminal::size()?;
                        let header = Rect::new(0, 0, width, 3);
                        let mut handled = false;
                        if let Some(controls) = ui::refresh_controls(header) {
                            if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, false, &refresh_shared);
                                handled = true;
                            } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, true, &refresh_shared);
                                handled = true;
                            }
                        }
                        if !handled && ui_state.click_process_sort(mouse.column, mouse.row) {
                            handled = true;
                        }
                        if !handled {
                            ui_state.click_process_row(
                                mouse.column,
                                mouse.row,
                                snapshot.system.processes.len(),
                            );
                        }
                    }
'''
new = '''                    MouseEventKind::Down(MouseButton::Left) => {
                        if ui_state.click_process_row(mouse.column, mouse.row) {
                            continue;
                        }

                        // Any left-click away from a process row releases the pinned process.
                        ui_state.clear_process_selection();

                        let (width, _) = crossterm::terminal::size()?;
                        let header = Rect::new(0, 0, width, 3);
                        let mut handled = false;
                        if let Some(controls) = ui::refresh_controls(header) {
                            if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, false, &refresh_shared);
                                handled = true;
                            } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, true, &refresh_shared);
                                handled = true;
                            }
                        }
                        if !handled {
                            ui_state.click_process_sort(mouse.column, mouse.row);
                        }
                    }
'''
assert old in text
text = text.replace(old, new, 1)

text = text.replace(
'                        ui_state.move_process_selection(-3, snapshot.system.processes.len());',
'                        ui_state.scroll_processes(-3, snapshot.system.processes.len());',
1,
)
text = text.replace(
'                        ui_state.move_process_selection(3, snapshot.system.processes.len());',
'                        ui_state.scroll_processes(3, snapshot.system.processes.len());',
1,
)

app.write_text(text)
