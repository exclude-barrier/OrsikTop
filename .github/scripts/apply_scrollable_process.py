from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing {label}")
    return text.replace(old, new, 1)

ui_path = Path("src/ui.rs")
ui = ui_path.read_text()

ui = replace_once(
    ui,
    "const WHITE: Color = Color::Rgb(225, 225, 225);",
    "const WHITE: Color = Color::Rgb(225, 225, 225);\nconst PROCESS_SELECTED_BG: Color = Color::Rgb(24, 54, 24);",
    "selected background color",
)

ui = replace_once(
    ui,
    "    ram_history: VecDeque<TimedSample>,\n}",
    "    ram_history: VecDeque<TimedSample>,\n    process_selected: usize,\n}",
    "UiState process selection field",
)
ui = replace_once(
    ui,
    "            ram_history: VecDeque::with_capacity(600),\n        }",
    "            ram_history: VecDeque::with_capacity(600),\n            process_selected: 0,\n        }",
    "UiState process selection default",
)

old_impl_tail = '''        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        push_history_at(&mut self.ram_history, ram, now);
    }
}'''
new_impl_tail = '''        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        push_history_at(&mut self.ram_history, ram, now);
    }

    pub fn move_process_selection(&mut self, delta: isize, total: usize) {
        if total == 0 {
            self.process_selected = 0;
            return;
        }
        let max = total - 1;
        self.process_selected = if delta.is_negative() {
            self.process_selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.process_selected
                .saturating_add(delta as usize)
                .min(max)
        };
    }

    pub fn process_home(&mut self) {
        self.process_selected = 0;
    }

    pub fn process_end(&mut self, total: usize) {
        self.process_selected = total.saturating_sub(1);
    }

    pub fn clamp_process_selection(&mut self, total: usize) {
        self.process_selected = self.process_selected.min(total.saturating_sub(1));
    }
}'''
ui = replace_once(ui, old_impl_tail, new_impl_tail, "UiState process navigation methods")

ui = replace_once(
    ui,
    ".constraints([Constraint::Percentage(58), Constraint::Percentage(42)])",
    ".constraints([Constraint::Percentage(52), Constraint::Percentage(48)])",
    "bottom pane ratio",
)
ui = replace_once(
    ui,
    "    draw_processes(frame, panes[1], processes);",
    "    draw_processes(frame, panes[1], processes, state.process_selected);",
    "process selection draw call",
)

start = ui.index("fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats]) {")
end = ui.index("fn process_header_compact", start)
new_draw = r'''fn draw_processes(
    frame: &mut Frame,
    area: Rect,
    processes: &[ProcessStats],
    selected: usize,
) {
    let visible = area.height.saturating_sub(3) as usize;
    let selected = selected.min(processes.len().saturating_sub(1));
    let max_start = processes.len().saturating_sub(visible);
    let start = if visible == 0 {
        0
    } else {
        selected.saturating_sub(visible / 2).min(max_start)
    };
    let end = start.saturating_add(visible).min(processes.len());
    let title = if processes.is_empty() {
        " PROCESSES · CPU ↓ · 0 ".to_string()
    } else {
        format!(
            " PROCESSES · CPU ↓ · {}–{}/{} ",
            start + 1,
            end,
            processes.len()
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

    let has_scrollbar = processes.len() > visible && visible > 0;
    let table_width = inner.width.saturating_sub(u16::from(has_scrollbar));
    let wide = table_width >= 96;
    let header = if wide {
        process_header_wide(table_width as usize)
    } else {
        process_header_compact(table_width as usize)
    };
    frame.render_widget(
        Paragraph::new(header).style(Style::default().fg(WHITE).add_modifier(Modifier::BOLD)),
        Rect::new(inner.x, inner.y, table_width, 1),
    );

    let mut lines = Vec::with_capacity(visible);
    for (index, process) in processes
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
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

    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            table_width,
            inner.height.saturating_sub(1),
        ),
    );

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
            processes.len(),
        );
    }
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
    let thumb_start = ((start as f64 / max_start as f64)
        * track.saturating_sub(thumb_len) as f64)
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

'''
ui = ui[:start] + new_draw + ui[end:]

ui = replace_once(
    ui,
    "fn process_line_compact(process: &ProcessStats, width: usize) -> Line<'static> {",
    "fn process_line_compact(\n    process: &ProcessStats,\n    width: usize,\n    selected: bool,\n) -> Line<'static> {",
    "compact process row signature",
)
ui = replace_once(
    ui,
    "fn process_line_wide(process: &ProcessStats, width: usize) -> Line<'static> {",
    "fn process_line_wide(\n    process: &ProcessStats,\n    width: usize,\n    selected: bool,\n) -> Line<'static> {",
    "wide process row signature",
)

# Replace compact row implementation styling block up to its closing Line expression.
old_compact = '''    let program = fit_cell(&process.program, program_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let mut program_style = Style::default().fg(DIM_GREEN);
    if is_llm_process(&process.program, &process.command) {
        program_style = Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD);
    }

    Line::from(vec![
        Span::styled(format!(" {:<6}", process.pid), Style::default().fg(MUTED)),
        Span::styled(format!("{program:<program_width$}"), program_style),
        Span::styled(
            format!("{:>6.1}", process.cpu_pct),
            Style::default().fg(cpu),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            Style::default().fg(MUTED),
        ),
    ])'''
new_compact = '''    let program = fit_cell(&process.program, program_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let program_bold = is_llm_process(&process.program, &process.command);
    let marker = if selected { "›" } else { " " };

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
    ])'''
ui = replace_once(ui, old_compact, new_compact, "compact selected row styling")

old_wide = '''    let program = fit_cell(&process.program, 16);
    let command = fit_cell(&process.command, command_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let mut program_style = Style::default().fg(DIM_GREEN);
    if is_llm_process(&process.program, &process.command) {
        program_style = Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD);
    }

    Line::from(vec![
        Span::styled(format!(" {:<6}", process.pid), Style::default().fg(MUTED)),
        Span::styled(format!("{program:<16}"), program_style),
        Span::styled(
            format!("{command:<command_width$}"),
            Style::default().fg(MUTED),
        ),
        Span::styled(
            format!("{:>6.1}", process.cpu_pct),
            Style::default().fg(cpu),
        ),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            Style::default().fg(MUTED),
        ),
    ])'''
new_wide = '''    let program = fit_cell(&process.program, 16);
    let command = fit_cell(&process.command, command_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let program_bold = is_llm_process(&process.program, &process.command);
    let marker = if selected { "›" } else { " " };

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
    ])'''
ui = replace_once(ui, old_wide, new_wide, "wide selected row styling")

insert_marker = "fn fit_cell(text: &str, width: usize) -> String {"
style_helper = '''fn process_cell_style(color: Color, selected: bool, bold: bool) -> Style {
    let mut style = Style::default().fg(color);
    if selected {
        style = style.bg(PROCESS_SELECTED_BG).add_modifier(Modifier::BOLD);
    } else if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

'''
if insert_marker not in ui:
    raise SystemExit("missing process style helper marker")
ui = ui.replace(insert_marker, style_helper + insert_marker, 1)

ui = replace_once(
    ui,
    '    let help = format!(" [q] quit   [-]/[+] refresh   {refresh_ms} ms   ");',
    '    let help = format!(" [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  {refresh_ms} ms  ");',
    "footer process navigation hint",
)

ui_path.write_text(ui)

app_path = Path("src/app.rs")
app = app_path.read_text()

app = replace_once(
    app,
    "        while let Ok(next) = fast_rx.try_recv() {\n            ui_state.push_sample(&next.gpu, &next.system);",
    "        while let Ok(next) = fast_rx.try_recv() {\n            ui_state.clamp_process_selection(next.system.processes.len());\n            ui_state.push_sample(&next.gpu, &next.system);",
    "process selection clamp on refresh",
)

old_keys = '''                    KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => {
                        change_refresh(&mut refresh_ms, true, &refresh_shared);
                    }
                    _ => {}'''
new_keys = '''                    KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => {
                        change_refresh(&mut refresh_ms, true, &refresh_shared);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui_state.move_process_selection(-1, snapshot.system.processes.len());
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        ui_state.move_process_selection(1, snapshot.system.processes.len());
                    }
                    KeyCode::PageUp => {
                        ui_state.move_process_selection(-10, snapshot.system.processes.len());
                    }
                    KeyCode::PageDown => {
                        ui_state.move_process_selection(10, snapshot.system.processes.len());
                    }
                    KeyCode::Home => ui_state.process_home(),
                    KeyCode::End => ui_state.process_end(snapshot.system.processes.len()),
                    _ => {}'''
app = replace_once(app, old_keys, new_keys, "process navigation keys")

old_mouse = '''                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    let (width, _) = crossterm::terminal::size()?;
                    let header = Rect::new(0, 0, width, 3);
                    if let Some(controls) = ui::refresh_controls(header) {
                        if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                            change_refresh(&mut refresh_ms, false, &refresh_shared);
                        } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                            change_refresh(&mut refresh_ms, true, &refresh_shared);
                        }
                    }
                }'''
new_mouse = '''                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
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
                    _ => {}
                },'''
app = replace_once(app, old_mouse, new_mouse, "mouse process scrolling")

app_path.write_text(app)
