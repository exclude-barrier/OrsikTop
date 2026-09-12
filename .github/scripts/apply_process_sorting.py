from pathlib import Path

ui = Path('src/ui.rs')
text = ui.read_text()

old = '''pub struct UiState {
    gpu_history: VecDeque<TimedSample>,
    vram_history: VecDeque<TimedSample>,
    cpu_history: VecDeque<TimedSample>,
    ram_history: VecDeque<TimedSample>,
    process_selected: usize,
}
'''
new = '''#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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

#[derive(Copy, Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    start: usize,
}

pub struct UiState {
    gpu_history: VecDeque<TimedSample>,
    vram_history: VecDeque<TimedSample>,
    cpu_history: VecDeque<TimedSample>,
    ram_history: VecDeque<TimedSample>,
    process_selected: usize,
    process_sort_key: ProcessSortKey,
    process_sort_desc: bool,
    process_header_hits: Vec<ProcessHeaderHit>,
    process_pane: Option<Rect>,
    process_rows: Option<ProcessRowsHit>,
}
'''
assert old in text
text = text.replace(old, new)

old = '''            ram_history: VecDeque::with_capacity(600),
            process_selected: 0,
        }
'''
new = '''            ram_history: VecDeque::with_capacity(600),
            process_selected: 0,
            process_sort_key: ProcessSortKey::Cpu,
            process_sort_desc: true,
            process_header_hits: Vec::new(),
            process_pane: None,
            process_rows: None,
        }
'''
assert old in text
text = text.replace(old, new)

old = '''    pub fn clamp_process_selection(&mut self, total: usize) {
        self.process_selected = self.process_selected.min(total.saturating_sub(1));
    }
}
'''
new = '''    pub fn clamp_process_selection(&mut self, total: usize) {
        self.process_selected = self.process_selected.min(total.saturating_sub(1));
    }

    pub fn process_pane_contains(&self, x: u16, y: u16) -> bool {
        self.process_pane
            .is_some_and(|pane| rect_contains(pane, x, y))
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
        self.process_selected = 0;
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

    fn clear_process_interaction(&mut self) {
        self.process_header_hits.clear();
        self.process_pane = None;
        self.process_rows = None;
    }
}
'''
assert old in text
text = text.replace(old, new)

text = text.replace('''    state: &UiState,
    server: &str,
''', '''    state: &mut UiState,
    server: &str,
''', 1)

old = '''    let area = frame.area();

    frame.render_widget(Block::default().style(Style::default().bg(BG_BLACK)), area);
'''
new = '''    let area = frame.area();
    state.clear_process_interaction();

    frame.render_widget(Block::default().style(Style::default().bg(BG_BLACK)), area);
'''
assert old in text
text = text.replace(old, new, 1)

old = '''fn draw_bottom(frame: &mut Frame, area: Rect, state: &UiState, processes: &[ProcessStats]) {
'''
new = '''fn draw_bottom(frame: &mut Frame, area: Rect, state: &mut UiState, processes: &[ProcessStats]) {
'''
assert old in text
text = text.replace(old, new)

old = '''    draw_history(frame, panes[0], state);
    draw_processes(frame, panes[1], processes, state.process_selected);
}
'''
new = '''    draw_history(frame, panes[0], state);
    draw_processes(frame, panes[1], processes, state);
}
'''
assert old in text
text = text.replace(old, new)

start = text.index('fn draw_processes(')
end = text.index('\nfn draw_process_scrollbar(', start)
replacement = r'''fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats], state: &mut UiState) {
    state.process_pane = Some(area);
    let sorted = sorted_processes(processes, state.process_sort_key, state.process_sort_desc);
    let visible = area.height.saturating_sub(3) as usize;
    let selected = state.process_selected.min(sorted.len().saturating_sub(1));
    let max_start = sorted.len().saturating_sub(visible);
    let start = if visible == 0 {
        0
    } else {
        selected.saturating_sub(visible / 2).min(max_start)
    };
    let end = start.saturating_add(visible).min(sorted.len());
    let sort_name = process_sort_name(state.process_sort_key);
    let sort_arrow = if state.process_sort_desc { "↓" } else { "↑" };
    let title = if sorted.is_empty() {
        format!(" PROCESSES · {sort_name} {sort_arrow} · 0 ")
    } else {
        format!(
            " PROCESSES · {sort_name} {sort_arrow} · {}–{}/{} ",
            start + 1,
            end,
            sorted.len()
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

    let has_scrollbar = sorted.len() > visible && visible > 0;
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
        Paragraph::new(header).style(Style::default().fg(WHITE).add_modifier(Modifier::BOLD)),
        Rect::new(inner.x, inner.y, table_width, 1),
    );
    state.process_header_hits = process_header_hits(inner, table_width, wide);

    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        table_width,
        inner.height.saturating_sub(1),
    );
    state.process_rows = Some(ProcessRowsHit { rect: body, start });

    let mut lines = Vec::with_capacity(visible);
    for (index, process) in sorted.iter().enumerate().skip(start).take(visible) {
        let is_selected = index == selected;
        lines.push(if wide {
            process_line_wide(process, table_width as usize, is_selected)
        } else {
            process_line_compact(process, table_width as usize, is_selected)
        });
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
                inner.y.saturating_add(1),
                1,
                inner.height.saturating_sub(1),
            ),
            start,
            visible,
            sorted.len(),
        );
    }
}

fn sorted_processes<'a>(
    processes: &'a [ProcessStats],
    key: ProcessSortKey,
    descending: bool,
) -> Vec<&'a ProcessStats> {
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

fn process_sort_name(key: ProcessSortKey) -> &'static str {
    match key {
        ProcessSortKey::Pid => "PID",
        ProcessSortKey::Program => "PROGRAM",
        ProcessSortKey::Cpu => "CPU",
        ProcessSortKey::Memory => "MEM",
        ProcessSortKey::Threads => "THR",
    }
}

fn process_header_label(
    label: &str,
    key: ProcessSortKey,
    active: ProcessSortKey,
    descending: bool,
) -> String {
    if key == active {
        format!("{label}{}", if descending { "↓" } else { "↑" })
    } else {
        label.to_string()
    }
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
'''
text = text[:start] + replacement + text[end:]

old_start = text.index('fn process_header_compact(')
old_end = text.index('\nfn process_line_compact(', old_start)
headers = r'''fn process_header_compact(
    width: usize,
    active: ProcessSortKey,
    descending: bool,
) -> String {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    let pid = process_header_label("PID", ProcessSortKey::Pid, active, descending);
    let program = process_header_label("PROGRAM", ProcessSortKey::Program, active, descending);
    let cpu = process_header_label("CPU%", ProcessSortKey::Cpu, active, descending);
    let memory = process_header_label("MEM", ProcessSortKey::Memory, active, descending);
    let threads = process_header_label("THR", ProcessSortKey::Threads, active, descending);
    format!(
        " {pid:<6}{program:<program_width$}{cpu:>6} {memory:>8} {threads:>5}"
    )
}

fn process_header_wide(
    width: usize,
    active: ProcessSortKey,
    descending: bool,
) -> String {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    let pid = process_header_label("PID", ProcessSortKey::Pid, active, descending);
    let program = process_header_label("PROGRAM", ProcessSortKey::Program, active, descending);
    let cpu = process_header_label("CPU%", ProcessSortKey::Cpu, active, descending);
    let memory = process_header_label("MEM", ProcessSortKey::Memory, active, descending);
    let threads = process_header_label("THR", ProcessSortKey::Threads, active, descending);
    format!(
        " {pid:<6}{program:<16}{:<command_width$}{cpu:>6} {memory:>8} {threads:>5}",
        "COMMAND"
    )
}
'''
text = text[:old_start] + headers + text[old_end:]

text = text.replace(
    'let help = format!(" [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  {refresh_ms} ms  ");',
    'let help = format!(" [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click] sort  {refresh_ms} ms  ");'
)

ui.write_text(text)

app = Path('src/app.rs')
text = app.read_text()
text = text.replace('''                &ui_state,
                server,
''', '''                &mut ui_state,
                server,
''', 1)

old = '''                    MouseEventKind::Down(MouseButton::Left) => {
                        let (width, _) = crossterm::terminal::size()?;
                        let header = Rect::new(0, 0, width, 3);
                        if let Some(controls) = ui::refresh_controls(header) {
                            if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, false, &refresh_shared);
                            } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                                change_refresh(&mut refresh_ms, true, &refresh_shared);
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        ui_state.move_process_selection(-3, snapshot.system.processes.len());
                    }
                    MouseEventKind::ScrollDown => {
                        ui_state.move_process_selection(3, snapshot.system.processes.len());
                    }
'''
new = '''                    MouseEventKind::Down(MouseButton::Left) => {
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
                    MouseEventKind::ScrollUp => {
                        if ui_state.process_pane_contains(mouse.column, mouse.row) {
                            ui_state.move_process_selection(-3, snapshot.system.processes.len());
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if ui_state.process_pane_contains(mouse.column, mouse.row) {
                            ui_state.move_process_selection(3, snapshot.system.processes.len());
                        }
                    }
'''
assert old in text
text = text.replace(old, new)

start = text.index('fn collect_process_stats(system: &System) -> Vec<ProcessStats> {')
end = text.index('\nfn read_process_thread_count', start)
replacement = r'''fn collect_process_stats(system: &System) -> Vec<ProcessStats> {
    system
        .processes()
        .iter()
        .map(|(pid, process)| {
            let program = process.name().to_string_lossy().into_owned();
            let command = process
                .exe()
                .map(|path| path.display().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| program.clone());
            let pid_u32 = pid.as_u32();
            ProcessStats {
                pid: pid_u32,
                program,
                command,
                cpu_pct: process.cpu_usage() as f64,
                memory_bytes: process.memory(),
                threads: read_process_thread_count(pid_u32).unwrap_or(1),
            }
        })
        .collect()
}
'''
text = text[:start] + replacement + text[end:]
app.write_text(text)
