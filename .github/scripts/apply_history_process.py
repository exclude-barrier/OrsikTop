from pathlib import Path
import re


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"missing {label}")
    return text.replace(old, new, 1)

ui_path = Path("src/ui.rs")
ui = ui_path.read_text()

ui = replace_once(
    ui,
    "#[derive(Clone, Debug, Default)]\npub struct SystemStats {",
    "#[derive(Clone, Debug, Default)]\npub struct ProcessStats {\n    pub pid: u32,\n    pub program: String,\n    pub command: String,\n    pub cpu_pct: f64,\n    pub memory_bytes: u64,\n    pub threads: usize,\n}\n\n#[derive(Clone, Debug, Default)]\npub struct SystemStats {",
    "ProcessStats struct insertion",
)
ui = replace_once(
    ui,
    "    pub swap_total_bytes: u64,\n}",
    "    pub swap_total_bytes: u64,\n    pub processes: Vec<ProcessStats>,\n}",
    "SystemStats processes field",
)
ui = replace_once(
    ui,
    "        draw_history(frame, rows[3], state);",
    "        draw_bottom(frame, rows[3], state, &system.processes);",
    "bottom draw call",
)

start = ui.index("fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {")
end = ui.index("fn draw_history_column(", start)
new_history = r'''fn draw_bottom(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    processes: &[ProcessStats],
) {
    // Keep very narrow terminals useful instead of crushing both panes.
    if area.width < 100 {
        draw_history(frame, area, state);
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);

    draw_history(frame, panes[0], state);
    draw_processes(frame, panes[1], processes);
}

fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default()
        .title(" HISTORY · 60 s ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 32 || inner.height < 5 {
        return;
    }

    // A 2x2 grid gives time-series data horizontal resolution while leaving the
    // right side of the dashboard available for the btop-style process table.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    draw_history_column(
        frame,
        top[0],
        "GPU",
        &state.gpu_history,
        ORK_GREEN,
        true,
        true,
    );
    draw_history_column(
        frame,
        top[1],
        "VRAM",
        &state.vram_history,
        CYAN,
        false,
        true,
    );
    draw_history_column(
        frame,
        bottom[0],
        "CPU",
        &state.cpu_history,
        ORK_GREEN,
        true,
        false,
    );
    draw_history_column(
        frame,
        bottom[1],
        "RAM",
        &state.ram_history,
        CYAN,
        false,
        false,
    );
}

'''
ui = ui[:start] + new_history + ui[end:]

old_column_head = '''fn draw_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
) {
    let block = if right_border {
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(INNER_GREEN))
    } else {
        Block::default()
    };
'''
new_column_head = '''fn draw_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
    bottom_border: bool,
) {
    let borders = match (right_border, bottom_border) {
        (true, true) => Borders::RIGHT | Borders::BOTTOM,
        (true, false) => Borders::RIGHT,
        (false, true) => Borders::BOTTOM,
        (false, false) => Borders::NONE,
    };
    let block = Block::default()
        .borders(borders)
        .border_style(Style::default().fg(INNER_GREEN));
'''
ui = replace_once(ui, old_column_head, new_column_head, "history column borders")

process_code = r'''
fn draw_processes(frame: &mut Frame, area: Rect, processes: &[ProcessStats]) {
    let title = format!(" PROCESSES · CPU ↓ · {} ", processes.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 30 || inner.height < 2 {
        return;
    }

    let wide = inner.width >= 78;
    let header = if wide {
        process_header_wide(inner.width as usize)
    } else {
        process_header_compact(inner.width as usize)
    };
    frame.render_widget(
        Paragraph::new(header).style(
            Style::default()
                .fg(WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let visible = inner.height.saturating_sub(1) as usize;
    let mut lines = Vec::with_capacity(visible);
    for process in processes.iter().take(visible) {
        lines.push(if wide {
            process_line_wide(process, inner.width as usize)
        } else {
            process_line_compact(process, inner.width as usize)
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
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
}

fn process_header_compact(width: usize) -> String {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    format!(
        " {:<6}{:<program_width$}{:>6} {:>8} {:>5}",
        "PID", "PROGRAM", "CPU%", "MEM", "THR"
    )
}

fn process_header_wide(width: usize) -> String {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    format!(
        " {:<6}{:<16}{:<command_width$}{:>6} {:>8} {:>5}",
        "PID", "PROGRAM", "COMMAND", "CPU%", "MEM", "THR"
    )
}

fn process_line_compact(process: &ProcessStats, width: usize) -> Line<'static> {
    let fixed = 7 + 7 + 9 + 6;
    let program_width = width.saturating_sub(fixed).max(8);
    let program = fit_cell(&process.program, program_width);
    let cpu = process_cpu_color(process.cpu_pct);
    let mut program_style = Style::default().fg(DIM_GREEN);
    if is_llm_process(&process.program, &process.command) {
        program_style = Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD);
    }

    Line::from(vec![
        Span::styled(format!(" {:<6}", process.pid), Style::default().fg(MUTED)),
        Span::styled(format!("{program:<program_width$}"), program_style),
        Span::styled(format!("{:>6.1}", process.cpu_pct), Style::default().fg(cpu)),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            Style::default().fg(MUTED),
        ),
    ])
}

fn process_line_wide(process: &ProcessStats, width: usize) -> Line<'static> {
    let fixed = 7 + 17 + 7 + 9 + 6;
    let command_width = width.saturating_sub(fixed).max(12);
    let program = fit_cell(&process.program, 16);
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
        Span::styled(format!("{:>6.1}", process.cpu_pct), Style::default().fg(cpu)),
        Span::styled(
            format!(" {:>8}", compact_memory(process.memory_bytes)),
            Style::default().fg(CYAN),
        ),
        Span::styled(
            format!(" {:>5}", process.threads),
            Style::default().fg(MUTED),
        ),
    ])
}

fn fit_cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = text.chars().take(width - 1).collect::<String>();
    out.push('…');
    out
}

fn compact_memory(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1}G", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0}M", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0}K", bytes / KIB)
    } else {
        format!("{}B", bytes as u64)
    }
}

fn process_cpu_color(cpu_pct: f64) -> Color {
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

fn is_llm_process(program: &str, command: &str) -> bool {
    let program = program.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    program.contains("llama")
        || program.contains("orsiktop")
        || command.contains("llama")
        || command.contains("orsiktop")
}

'''
footer_marker = "fn draw_footer(frame: &mut Frame, area: Rect, llm: &LlmStats, gpu: &GpuStats, refresh_ms: u64) {"
if footer_marker not in ui:
    raise SystemExit("missing footer insertion marker")
ui = ui.replace(footer_marker, process_code + footer_marker, 1)

# Replace stale heatmap-specific tests only if they remain; fallback code itself stays.
ui_path.write_text(ui)

app_path = Path("src/app.rs")
app = app_path.read_text()
app = replace_once(
    app,
    "use sysinfo::System;",
    "use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};",
    "sysinfo imports",
)
app = replace_once(
    app,
    "    ui::{self, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS},",
    "    ui::{\n        self, ProcessStats, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,\n    },",
    "ProcessStats import",
)
app = replace_once(
    app,
    "const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);",
    "const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);\nconst PROCESS_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);",
    "process interval",
)
app = replace_once(
    app,
    "        let mut last_system_refresh: Option<Instant> = None;\n        let mut previous_cpu_times = read_cpu_times();",
    "        let mut last_system_refresh: Option<Instant> = None;\n        let mut last_process_refresh: Option<Instant> = None;\n        let mut process_stats = Vec::<ProcessStats>::new();\n        let mut previous_cpu_times = read_cpu_times();",
    "process worker state",
)

loop_marker = "            if last_system_refresh\n                .map(|at| at.elapsed() >= SYSTEM_REFRESH_INTERVAL)\n                .unwrap_or(true)\n            {"
process_refresh = '''            if last_process_refresh
                .map(|at| at.elapsed() >= PROCESS_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::All,
                    true,
                    ProcessRefreshKind::nothing()
                        .with_memory()
                        .with_cpu()
                        .with_exe(UpdateKind::OnlyIfNotSet)
                        .without_tasks(),
                );
                process_stats = collect_process_stats(&system);
                last_process_refresh = Some(Instant::now());
            }

'''
app = replace_once(app, loop_marker, process_refresh + loop_marker, "process refresh block")
app = replace_once(
    app,
    "                    swap_used_bytes: system.used_swap(),\n                    swap_total_bytes: system.total_swap(),\n                };",
    "                    swap_used_bytes: system.used_swap(),\n                    swap_total_bytes: system.total_swap(),\n                    processes: process_stats.clone(),\n                };",
    "SystemStats process snapshot",
)

helper_marker = "#[derive(Copy, Clone, Debug, PartialEq, Eq)]\nstruct CpuTimes {"
helpers = r'''fn collect_process_stats(system: &System) -> Vec<ProcessStats> {
    let mut processes = system
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
        .collect::<Vec<_>>();

    processes.sort_by(|a, b| {
        b.cpu_pct
            .total_cmp(&a.cpu_pct)
            .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    processes.truncate(256);
    processes
}

fn read_process_thread_count(pid: u32) -> Option<usize> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("Threads:")?;
        value.trim().parse().ok()
    })
}

'''
app = replace_once(app, helper_marker, helpers + helper_marker, "process helper insertion")
app_path.write_text(app)
