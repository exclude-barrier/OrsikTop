from pathlib import Path


def replace_between(text: str, start: str, end: str, replacement: str) -> str:
    start_i = text.index(start)
    end_i = text.index(end, start_i)
    return text[:start_i] + replacement + text[end_i:]


ui_path = Path("src/ui.rs")
ui = ui_path.read_text()

ui = ui.replace(
    "    collections::VecDeque,\n",
    "    collections::{HashMap, HashSet, VecDeque},\n",
    1,
)

ui = ui.replace(
    '''#[derive(Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    pids: Vec<u32>,
}
''',
    '''#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessRowTarget {
    Process(u32),
    Group(String),
}

#[derive(Clone, Debug)]
struct ProcessRowsHit {
    rect: Rect,
    targets: Vec<ProcessRowTarget>,
}
''',
    1,
)

ui = ui.replace(
    '''    last_process_click: Option<(u32, Instant)>,
    process_scroll: usize,
''',
    '''    last_process_click: Option<(u32, Instant)>,
    expanded_process_groups: HashSet<String>,
    process_scroll: usize,
    process_display_total: usize,
''',
    1,
)

ui = ui.replace(
    '''            last_process_click: None,
            process_scroll: 0,
''',
    '''            last_process_click: None,
            expanded_process_groups: HashSet::new(),
            process_scroll: 0,
            process_display_total: 0,
''',
    1,
)

ui = ui.replace(
    '''        if self
            .process_pinned_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_pinned_pid = None;
        }
        self.process_scroll = self.process_scroll.min(processes.len().saturating_sub(1));
''',
    '''        if self
            .process_pinned_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_pinned_pid = None;
        }
        let programs = processes
            .iter()
            .map(|process| process.program.as_str())
            .collect::<HashSet<_>>();
        self.expanded_process_groups
            .retain(|program| programs.contains(program.as_str()));
        self.process_scroll = self.process_scroll.min(self.process_display_total.saturating_sub(1));
''',
    1,
)

ui = replace_between(
    ui,
    "    pub fn scroll_processes(",
    "    pub fn clear_process_selection(",
    '''    pub fn scroll_processes(&mut self, delta: isize) {
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

''',
)

ui = replace_between(
    ui,
    "    pub fn click_process_row(",
    "    fn clear_process_interaction(",
    '''    pub fn click_process_row(&mut self, x: u16, y: u16) -> bool {
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
        let ProcessRowTarget::Process(pid) = target else {
            return false;
        };

        let now = Instant::now();
        let is_double_click = self.last_process_click.is_some_and(|(last_pid, at)| {
            last_pid == pid && now.saturating_duration_since(at) <= PROCESS_DOUBLE_CLICK_WINDOW
        });

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

''',
)

new_process_section = r'''#[derive(Clone, Debug)]
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
            memory_bytes: members
                .iter()
                .map(|process| process.memory_bytes)
                .sum(),
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

fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats], state: &mut UiState) {
    state.process_pane = Some(area);
    let (pinned, rows) = grouped_process_rows(
        processes,
        state.process_sort_key,
        state.process_sort_desc,
        state.process_pinned_pid,
        &state.expanded_process_groups,
    );
    if state.process_pinned_pid.is_some() && pinned.is_none() {
        state.process_pinned_pid = None;
    }
    state.process_display_total = rows.len();

    let visible = area.height.saturating_sub(3) as usize;
    let pinned_rows = usize::from(pinned.is_some() && visible > 0);
    let scroll_visible = visible.saturating_sub(pinned_rows);
    let max_start = rows.len().saturating_sub(scroll_visible);
    let start = state.process_scroll.min(max_start);
    state.process_scroll = start;
    let end = start.saturating_add(scroll_visible).min(rows.len());

    let sort_name = process_sort_name(state.process_sort_key);
    let sort_arrow = if state.process_sort_desc { "↓" } else { "↑" };
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
            } => lines.push(if wide {
                process_group_line_wide(
                    program,
                    count,
                    cpu_pct,
                    memory_bytes,
                    threads,
                    expanded,
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
                    table_width as usize,
                )
            }),
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

'''
ui = replace_between(ui, "fn draw_processes(", "fn sorted_processes(", new_process_section)

new_compact = r'''fn process_group_line_compact(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: usize,
    expanded: bool,
    width: usize,
) -> Line<'static> {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    let count_label = format!("×{count}");
    let arrow = if expanded { "▾" } else { "▸" };
    let program = fit_cell(&format!("{arrow} {program}"), program_width);
    let cpu = process_cpu_color(cpu_pct);

    Line::from(vec![
        Span::styled(format!(" {:<6}", count_label), Style::default().fg(CYAN)),
        Span::styled(
            format!("{program:<program_width$}"),
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:>6.1}", cpu_pct), Style::default().fg(cpu)),
        Span::styled(
            format!(" {:>8}", compact_memory(memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(format!(" {:>5}", threads), Style::default().fg(MUTED)),
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

'''
ui = replace_between(ui, "fn process_line_compact(", "fn process_line_wide(", new_compact)

new_wide = r'''fn process_group_line_wide(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: usize,
    expanded: bool,
    width: usize,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    let count_label = format!("×{count}");
    let arrow = if expanded { "▾" } else { "▸" };
    let program = fit_cell(&format!("{arrow} {program}"), 16);
    let command = fit_cell(&format!("{count} processes"), command_width);
    let cpu = process_cpu_color(cpu_pct);

    Line::from(vec![
        Span::styled(format!(" {:<6}", count_label), Style::default().fg(CYAN)),
        Span::styled(
            format!("{program:<16}"),
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{command:<command_width$}"),
            Style::default().fg(MUTED),
        ),
        Span::styled(format!("{:>6.1}", cpu_pct), Style::default().fg(cpu)),
        Span::styled(
            format!(" {:>8}", compact_memory(memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(format!(" {:>5}", threads), Style::default().fg(MUTED)),
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

'''
ui = replace_between(ui, "fn process_line_wide(", "fn process_cell_style(", new_wide)

ui = ui.replace(
    '        " [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click] select  [2x] pin  [header] sort  {refresh_ms} ms  "\n',
    '        " [q] quit  [-]/[+] refresh  [click] select  [2x] pin  [RMB group] expand  [header] sort  {refresh_ms} ms  "\n',
    1,
)

# Add focused grouping tests before the existing pin/sort regression test.
test_marker = "    #[test]\n    fn pinned_process_is_removed_from_sorted_stream() {"
assert test_marker in ui
new_tests = r'''    #[test]
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
        let (_, rows) = grouped_process_rows(
            &processes,
            ProcessSortKey::Cpu,
            true,
            None,
            &expanded,
        );

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
        let (_, rows) = grouped_process_rows(
            &processes,
            ProcessSortKey::Cpu,
            true,
            None,
            &expanded,
        );

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

'''
ui = ui.replace(test_marker, new_tests + test_marker, 1)

# Existing click tests now populate row targets rather than raw PID vectors.
ui = ui.replace("            pids: vec![10, 20],", "            targets: vec![ProcessRowTarget::Process(10), ProcessRowTarget::Process(20)],")

ui_path.write_text(ui)

app_path = Path("src/app.rs")
app = app_path.read_text()
app = app.replace(
    '''                    MouseEventKind::ScrollUp
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.scroll_processes(-3, snapshot.system.processes.len());
                    }
                    MouseEventKind::ScrollDown
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.scroll_processes(3, snapshot.system.processes.len());
                    }
''',
    '''                    MouseEventKind::Down(MouseButton::Right) => {
                        ui_state.right_click_process_group(mouse.column, mouse.row);
                    }
                    MouseEventKind::ScrollUp
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.scroll_processes(-3);
                    }
                    MouseEventKind::ScrollDown
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.scroll_processes(3);
                    }
''',
    1,
)
app_path.write_text(app)
