from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()

start = text.index('fn draw_processes(')
end = text.index('\nfn sorted_processes(', start)

new_draw = '''fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats], state: &mut UiState) {
    state.process_pane = Some(area);
    let (pinned, sorted) = sorted_processes_with_pin(
        processes,
        state.process_sort_key,
        state.process_sort_desc,
        state.process_selected_pid,
    );
    if state.process_selected_pid.is_some() && pinned.is_none() {
        state.process_selected_pid = None;
    }

    let visible = area.height.saturating_sub(3) as usize;
    let pinned_rows = usize::from(pinned.is_some() && visible > 0);
    let scroll_visible = visible.saturating_sub(pinned_rows);
    let max_start = sorted.len().saturating_sub(scroll_visible);
    let start = state.process_scroll.min(max_start);
    state.process_scroll = start;
    let end = start.saturating_add(scroll_visible).min(sorted.len());

    let sort_name = process_sort_name(state.process_sort_key);
    let sort_arrow = if state.process_sort_desc { "↓" } else { "↑" };
    let total = processes.len();
    let title = if total == 0 {
        format!(" PROCESSES · SORT {sort_name} {sort_arrow} · 0 ")
    } else if let Some(process) = pinned {
        let range = if scroll_visible == 0 || sorted.is_empty() {
            "0".to_string()
        } else {
            format!("{}–{}", start + 1, end)
        };
        format!(
            " PROCESSES · PIN {} · SORT {sort_name} {sort_arrow} · {range}/{total} ",
            process.pid
        )
    } else {
        format!(
            " PROCESSES · SORT {sort_name} {sort_arrow} · {}–{}/{} ",
            start + 1,
            end,
            total
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

    let has_scrollbar = sorted.len() > scroll_visible && scroll_visible > 0;
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

    let mut visible_processes = Vec::with_capacity(visible);
    if let Some(process) = pinned {
        visible_processes.push(process);
    }
    visible_processes.extend(sorted.iter().skip(start).take(scroll_visible).copied());

    state.process_rows = Some(ProcessRowsHit {
        rect: body,
        pids: visible_processes.iter().map(|process| process.pid).collect(),
    });

    let mut lines = Vec::with_capacity(visible);
    for process in visible_processes {
        let is_selected = state.process_selected_pid == Some(process.pid);
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
                body.y.saturating_add(pinned_rows as u16),
                1,
                body.height.saturating_sub(pinned_rows as u16),
            ),
            start,
            scroll_visible,
            sorted.len(),
        );
    }
}
'''

text = text[:start] + new_draw + text[end:]

needle = "fn process_sort_name(key: ProcessSortKey) -> &'static str {"
helper = '''fn sorted_processes_with_pin(
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

'''
assert needle in text
text = text.replace(needle, helper + needle, 1)

insert_before = "    #[test]\n    fn physical_core_average_combines_smt_threads() {"
new_test = '''    #[test]
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

'''
assert insert_before in text
text = text.replace(insert_before, new_test + insert_before, 1)

path.write_text(text)
