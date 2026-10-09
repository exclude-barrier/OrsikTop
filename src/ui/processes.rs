//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

pub(super) fn process_row_target(row: &ProcessDisplayRow<'_>) -> ProcessRowTarget {
    match row {
        ProcessDisplayRow::Process { process, .. } => ProcessRowTarget::Process(process.identity()),
        ProcessDisplayRow::Group { program, .. } => ProcessRowTarget::Group((*program).to_string()),
    }
}

#[cfg(test)]
pub(super) fn grouped_process_rows<'a>(
    processes: &'a [ProcessStats],
    key: ProcessSortKey,
    descending: bool,
    pinned_pid: Option<ProcessIdentity>,
    expanded_groups: &HashSet<String>,
) -> (Option<&'a ProcessStats>, Vec<ProcessDisplayRow<'a>>) {
    grouped_process_rows_filtered(
        processes,
        key,
        descending,
        pinned_pid,
        expanded_groups,
        None,
    )
}

pub(super) fn grouped_process_rows_filtered<'a>(
    processes: &'a [ProcessStats],
    key: ProcessSortKey,
    descending: bool,
    pinned_pid: Option<ProcessIdentity>,
    expanded_groups: &HashSet<String>,
    search_query: Option<&str>,
) -> (Option<&'a ProcessStats>, Vec<ProcessDisplayRow<'a>>) {
    let (mut pinned, mut unpinned) =
        sorted_processes_with_pin(processes, key, descending, pinned_pid);
    if let Some(query) = search_query {
        if pinned.is_some_and(|process| !process_matches_search(process, query)) {
            pinned = None;
        }
        unpinned.retain(|process| process_matches_search(process, query));
    }
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
            gpu_bytes: members.iter().map(|process| process.gpu_bytes()).sum(),
            // Aggregate only when every member's thread count is known: a
            // partial sum would present an unknown as a verified total.
            threads: members
                .iter()
                .map(|process| process.threads)
                .collect::<Option<Vec<_>>>()
                .map(|counts| counts.into_iter().sum()),
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
            gpu_bytes: group.gpu_bytes,
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

pub(super) fn take_pinned_group_rows<'a>(
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

pub(super) fn draw_processes(
    frame: &mut Frame,
    area: Rect,
    processes: &[ProcessStats],
    state: &mut UiState,
) {
    state.process_pane = Some(area);
    let search_query = state.process_search_query().map(str::to_string);
    let (pinned, mut rows) = grouped_process_rows_filtered(
        processes,
        state.process_sort_key,
        state.process_sort_desc,
        state.process_pinned_pid,
        &state.expanded_process_groups,
        search_query.as_deref(),
    );
    let pinned_group_name = state.process_pinned_group.clone();
    let pinned_group_rows = take_pinned_group_rows(&mut rows, pinned_group_name.as_deref());
    state.process_display_total = rows.len();

    let visible = area.height.saturating_sub(3) as usize;
    let pinned_rows =
        usize::from(pinned.is_some() && visible > 0).saturating_add(pinned_group_rows.len());
    let scroll_visible = visible.saturating_sub(pinned_rows);
    state.process_scroll_visible = scroll_visible;
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
    let matched_process_total = search_query
        .as_deref()
        .map(|query| {
            processes
                .iter()
                .filter(|process| process_matches_search(process, query))
                .count()
        })
        .unwrap_or(process_total);
    let process_count = if search_query.is_some() {
        format!("{matched_process_total}/{process_total} PROC")
    } else {
        format!("{process_total} PROC")
    };
    let search_title = if state.process_search_open || search_query.is_some() {
        format!(" · /{}", fit_cell(&state.process_search_query, 18))
    } else {
        String::new()
    };
    let row_total = rows.len();
    let range = if scroll_visible == 0 || rows.is_empty() {
        "0".to_string()
    } else {
        format!("{}–{}", start + 1, end)
    };
    let title = if process_total == 0 {
        format!(" PROCESSES{search_title} · SORT {sort_name} {sort_arrow} · 0 ")
    } else if let Some(process) = pinned {
        format!(
            " PROCESSES{search_title} · PIN {} · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_count} ",
            process.pid
        )
    } else if let Some(program) = state.process_pinned_group.as_deref() {
        format!(
            " PROCESSES{search_title} · PIN GROUP {program} · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_count} "
        )
    } else {
        format!(
            " PROCESSES{search_title} · SORT {sort_name} {sort_arrow} · {range}/{row_total} · {process_count} "
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
                let is_selected = state.process_selected_pid == Some(process.identity())
                    || state.process_pinned_pid == Some(process.identity());
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
                gpu_bytes,
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
                        gpu_bytes,
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
        let message = search_query
            .as_deref()
            .map(|query| format!(" no processes match /{query}"))
            .unwrap_or_else(|| " waiting for process samples…".to_string());
        lines.push(Line::from(Span::styled(
            message,
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

pub(super) fn sorted_processes(
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

pub(super) fn sorted_processes_with_pin(
    processes: &[ProcessStats],
    key: ProcessSortKey,
    descending: bool,
    pinned_pid: Option<ProcessIdentity>,
) -> (Option<&ProcessStats>, Vec<&ProcessStats>) {
    let mut sorted = sorted_processes(processes, key, descending);
    let pinned = pinned_pid.and_then(|identity| {
        let index = sorted
            .iter()
            .position(|process| process.identity() == identity)?;
        Some(sorted.remove(index))
    });
    (pinned, sorted)
}

pub(super) fn process_sort_name(key: ProcessSortKey) -> &'static str {
    match key {
        ProcessSortKey::Pid => "PID",
        ProcessSortKey::Program => "PROGRAM",
        ProcessSortKey::Cpu => "CPU",
        ProcessSortKey::Memory => "MEM",
        ProcessSortKey::Threads => "THR",
    }
}

pub(super) fn process_header_button(
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

pub(super) fn process_header_hits(
    inner: Rect,
    table_width: u16,
    wide: bool,
) -> Vec<ProcessHeaderHit> {
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
        let command_width = width.saturating_sub(7 + 17 + 7 + 9 + 6 + 7).max(12) as u16;
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

pub(super) fn draw_process_scrollbar(
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

pub(super) fn process_header_compact(
    width: usize,
    active: ProcessSortKey,
    descending: bool,
) -> Line<'static> {
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

pub(super) fn process_header_wide(
    width: usize,
    active: ProcessSortKey,
    descending: bool,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6 + 7;
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
        Span::raw(" "),
        Span::styled(
            format!("{:<6}", "GPU"),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
    ])
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process_group_line_compact(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    threads: Option<usize>,
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
            format!(" {:>5}", thread_cell(threads)),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

pub(super) fn process_line_compact(
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
            format!(" {:>5}", thread_cell(process.threads)),
            process_cell_style(MUTED, selected, false),
        ),
    ])
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process_group_line_wide(
    program: &str,
    count: usize,
    cpu_pct: f64,
    memory_bytes: u64,
    gpu_bytes: u64,
    threads: Option<usize>,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6 + 7;
    let command_width = width.saturating_sub(fixed).max(12);
    let count_label = format!("×{count}");
    let arrow = if expanded { "▾" } else { "▸" };
    let program = fit_cell(&format!("{arrow} {program}"), 16);
    let command = fit_cell(&format!("{count} processes"), command_width);
    let cpu = process_cpu_color(cpu_pct);
    let marker = if selected { "›" } else { " " };
    let gpu_text = gpu_cell(gpu_bytes);

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
            format!(" {:>5}", thread_cell(threads)),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!(" {:>6}", gpu_text),
            process_cell_style(CYAN, selected, false),
        ),
    ])
}

pub(super) fn process_line_wide(
    process: &ProcessStats,
    width: usize,
    selected: bool,
    child: bool,
) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6 + 7;
    let command_width = width.saturating_sub(fixed).max(12);
    let program = fit_cell(&process.program, 16);
    let command = fit_cell(&process.command, command_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let program_bold = is_llm_process(&process.program, &process.command);
    let gpu_text = gpu_cell(process.gpu_bytes());
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
            format!(" {:>5}", thread_cell(process.threads)),
            process_cell_style(MUTED, selected, false),
        ),
        Span::styled(
            format!(" {:>6}", gpu_text),
            process_cell_style(CYAN, selected, false),
        ),
    ])
}

pub(super) fn process_cell_style(color: Color, selected: bool, bold: bool) -> Style {
    let mut style = Style::default().fg(color);
    if selected {
        style = style.bg(PROCESS_SELECTED_BG).add_modifier(Modifier::BOLD);
    } else if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

pub(super) fn process_cpu_color(cpu_pct: f64) -> Color {
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

pub(super) fn process_matches_search(process: &ProcessStats, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    process.program.to_ascii_lowercase().contains(&query)
        || process.command.to_ascii_lowercase().contains(&query)
        || process.pid.to_string().contains(&query)
}

pub(super) fn is_llm_process(program: &str, command: &str) -> bool {
    let program = program.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    program.contains("llama")
        || program.contains("orsiktop")
        || command.contains("llama")
        || command.contains("orsiktop")
}
