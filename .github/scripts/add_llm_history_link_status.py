from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"anchor not found: {label}")
    return text.replace(old, new, 1)

# llama.rs: mark held/transient samples as reconnecting.
path = Path("src/llama.rs")
text = path.read_text()
text = replace_once(
    text,
    "    pub connected: bool,\n    pub model: String,\n",
    "    pub connected: bool,\n    pub reconnecting: bool,\n    pub model: String,\n",
    "llm reconnecting field",
)
path.write_text(text)

# app.rs: feed LLM samples into UI state, reset state on endpoint changes,
# and preserve the last good sample while explicitly marking reconnect attempts.
path = Path("src/app.rs")
text = path.read_text()
text = replace_once(
    text,
    "        while let Ok(next) = llm_rx.try_recv() {\n            snapshot.llm = next;\n        }\n",
    "        while let Ok(next) = llm_rx.try_recv() {\n            ui_state.observe_llm_sample(&next);\n            snapshot.llm = next;\n        }\n",
    "observe llm sample",
)
text = replace_once(
    text,
    "                                    server = endpoint.clone();\n                                    snapshot.llm = LlmStats::default();\n                                    let _ = server_tx.send(endpoint);\n",
    "                                    server = endpoint.clone();\n                                    snapshot.llm = LlmStats::default();\n                                    ui_state.reset_llm_connection_state();\n                                    let _ = server_tx.send(endpoint);\n",
    "reset llm state on endpoint change",
)
text = replace_once(
    text,
    "fn stabilize_llm_sample(\n    stats: LlmStats,\n",
    "fn stabilize_llm_sample(\n    mut stats: LlmStats,\n",
    "mutable stabilized stats",
)
text = replace_once(
    text,
    "    if stats.connected {\n        *last_good = Some((stats.clone(), now));\n        return stats;\n    }\n",
    "    if stats.connected {\n        stats.reconnecting = false;\n        *last_good = Some((stats.clone(), now));\n        return stats;\n    }\n",
    "fresh sample reconnect flag",
)
text = replace_once(
    text,
    "                let mut held = previous.clone();\n                held.error.clear();\n                return held;\n",
    "                let mut held = previous.clone();\n                held.error.clear();\n                held.reconnecting = true;\n                return held;\n",
    "held sample reconnect flag",
)
text = replace_once(
    text,
    "\n    stats\n}\n\nfn sleep_until_next_cycle",
    "\n    stats.reconnecting = !hard_failure;\n    stats\n}\n\nfn sleep_until_next_cycle",
    "offline reconnect flag",
)
# Add stabilization coverage before the existing CPU parser test.
text = replace_once(
    text,
    "    #[test]\n    fn parses_linux_cpu_times_and_iowait_delta() {\n",
    "    #[test]\n    fn transient_llm_failure_keeps_last_sample_and_marks_reconnecting() {\n        let now = Instant::now();\n        let mut last_good = None;\n        let good = LlmStats {\n            connected: true,\n            prompt_tps: 123.0,\n            ..LlmStats::default()\n        };\n        let fresh = stabilize_llm_sample(good, &mut last_good, now);\n        assert!(fresh.connected);\n        assert!(!fresh.reconnecting);\n\n        let failed = LlmStats {\n            error: \"cannot reach llama.cpp: timeout\".to_string(),\n            ..LlmStats::default()\n        };\n        let held = stabilize_llm_sample(\n            failed,\n            &mut last_good,\n            now + Duration::from_millis(500),\n        );\n        assert!(held.connected);\n        assert!(held.reconnecting);\n        assert_eq!(held.prompt_tps, 123.0);\n    }\n\n    #[test]\n    fn parses_linux_cpu_times_and_iowait_delta() {\n",
    "stabilize reconnecting test",
)
path.write_text(text)

# ui.rs: track fresh LLM samples, add connection metadata, and render two 60-second
# throughput graphs in the spare LLM panel space.
path = Path("src/ui.rs")
text = path.read_text()
text = replace_once(
    text,
    "struct TimedSample {\n    at: Instant,\n    value: u64,\n}\n",
    "struct TimedSample {\n    at: Instant,\n    value: f64,\n}\n",
    "floating history values",
)
text = replace_once(
    text,
    "    cpu_history: VecDeque<TimedSample>,\n    ram_history: VecDeque<TimedSample>,\n",
    "    cpu_history: VecDeque<TimedSample>,\n    ram_history: VecDeque<TimedSample>,\n    llm_prefill_history: VecDeque<TimedSample>,\n    llm_decode_history: VecDeque<TimedSample>,\n    llm_last_fresh_at: Option<Instant>,\n    llm_connected_since: Option<Instant>,\n    llm_connected_flash_until: Option<Instant>,\n    llm_was_connected: bool,\n",
    "llm history state fields",
)
text = replace_once(
    text,
    "            cpu_history: VecDeque::with_capacity(600),\n            ram_history: VecDeque::with_capacity(600),\n",
    "            cpu_history: VecDeque::with_capacity(600),\n            ram_history: VecDeque::with_capacity(600),\n            llm_prefill_history: VecDeque::with_capacity(600),\n            llm_decode_history: VecDeque::with_capacity(600),\n            llm_last_fresh_at: None,\n            llm_connected_since: None,\n            llm_connected_flash_until: None,\n            llm_was_connected: false,\n",
    "llm history state defaults",
)
push_sample_anchor = """    pub fn push_sample(&mut self, gpu: &GpuStats, system: &SystemStats) {
        let now = Instant::now();
        let vram = percent(gpu.memory_used_mib, gpu.memory_total_mib);
        let ram = percent(
            system.memory_used_bytes as f64,
            system.memory_total_bytes as f64,
        );

        push_history_at(&mut self.gpu_history, gpu.utilization, now);
        push_history_at(&mut self.vram_history, vram, now);
        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        push_history_at(&mut self.ram_history, ram, now);
    }
"""
push_sample_new = push_sample_anchor + """
    pub fn observe_llm_sample(&mut self, llm: &LlmStats) {
        let now = Instant::now();
        if llm.connected && !llm.reconnecting {
            if !self.llm_was_connected {
                self.llm_connected_since = Some(now);
                self.llm_connected_flash_until = Some(now + Duration::from_secs(2));
            }
            self.llm_was_connected = true;
            self.llm_last_fresh_at = Some(now);
            push_metric_history_at(&mut self.llm_prefill_history, llm.prompt_tps, now);
            push_metric_history_at(&mut self.llm_decode_history, llm.generation_tps, now);
        } else if !llm.connected {
            self.llm_was_connected = false;
            self.llm_connected_since = None;
            self.llm_connected_flash_until = None;
        }
    }

    pub fn reset_llm_connection_state(&mut self) {
        self.llm_prefill_history.clear();
        self.llm_decode_history.clear();
        self.llm_last_fresh_at = None;
        self.llm_connected_since = None;
        self.llm_connected_flash_until = None;
        self.llm_was_connected = false;
    }
"""
text = replace_once(text, push_sample_anchor, push_sample_new, "llm sample observer")
text = replace_once(
    text,
    "    draw_header(frame, rows[0], llm, server, refresh_ms);\n",
    "    draw_header(frame, rows[0], llm, state, server, refresh_ms);\n",
    "header receives ui state",
)
text = replace_once(
    text,
    "    draw_llm_and_system(frame, rows[2], system, llm);\n",
    "    draw_llm_and_system(frame, rows[2], system, llm, state);\n",
    "llm panel receives ui state",
)
text = replace_once(
    text,
    "fn draw_header(frame: &mut Frame, area: Rect, llm: &LlmStats, server: &str, refresh_ms: u64) {\n",
    "fn draw_header(\n    frame: &mut Frame,\n    area: Rect,\n    llm: &LlmStats,\n    state: &UiState,\n    server: &str,\n    refresh_ms: u64,\n) {\n",
    "header signature",
)
text = replace_once(
    text,
    "        let status = if llm.connected { \"ONLINE\" } else { \"OFFLINE\" };\n        let status_style = if llm.connected {\n            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD)\n        } else {\n            Style::default().fg(RED).add_modifier(Modifier::BOLD)\n        };\n",
    "        let (status, status_color) = llm_link_status(state, llm);\n        let status_style = Style::default()\n            .fg(status_color)\n            .add_modifier(Modifier::BOLD);\n",
    "header link status",
)
text = replace_once(
    text,
    "fn draw_llm_and_system(frame: &mut Frame, area: Rect, system: &SystemStats, llm: &LlmStats) {\n",
    "fn draw_llm_and_system(\n    frame: &mut Frame,\n    area: Rect,\n    system: &SystemStats,\n    llm: &LlmStats,\n    state: &UiState,\n) {\n",
    "llm system signature",
)
text = replace_once(
    text,
    "    draw_llm(frame, cols[0], llm);\n",
    "    draw_llm(frame, cols[0], llm, state);\n",
    "draw llm state arg",
)
text = replace_once(
    text,
    "fn draw_llm(frame: &mut Frame, area: Rect, llm: &LlmStats) {\n",
    "fn draw_llm(frame: &mut Frame, area: Rect, llm: &LlmStats, state: &UiState) {\n",
    "draw llm signature",
)
# Replace offline rendering with explicit reconnect/freshness information.
offline_old = """    if !llm.connected {
        let metrics_off = llm_metrics_disabled(&llm.error);
        let (status, color, message) = if metrics_off {
            ("METRICS OFF", YELLOW, "restart server with --metrics")
        } else {
            ("SERVER OFFLINE", RED, "connection lost · retrying")
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![label_span(" STATUS   "), value_span(status, color)]),
                Line::from(vec![
                    label_span(" LLAMA    "),
                    Span::styled(message, Style::default().fg(MUTED)),
                ]),
            ]),
            inner,
        );
        return;
    }
"""
offline_new = """    if !llm.connected {
        let metrics_off = llm_metrics_disabled(&llm.error);
        let (status, color, message) = if metrics_off {
            ("METRICS OFF", YELLOW, "restart server with --metrics")
        } else if llm.reconnecting {
            ("RECONNECTING", YELLOW, "connection lost · retrying")
        } else {
            ("SERVER OFFLINE", RED, "connection lost")
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![label_span(" STATUS      "), value_span(status, color)]),
                Line::from(vec![
                    label_span(" LLAMA       "),
                    Span::styled(message, Style::default().fg(MUTED)),
                ]),
                Line::from(vec![
                    label_span(" LAST SAMPLE "),
                    value_span(&llm_last_sample_text(state), MUTED),
                ]),
                Line::from(vec![label_span(" UPTIME      "), value_span("—", MUTED)]),
            ]),
            inner,
        );
        return;
    }
"""
text = replace_once(text, offline_old, offline_new, "offline llm status")
# Insert the link/freshness line after MODEL.
model_line = """        Line::from(vec![
            label_span(" MODEL      "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(state),
"""
link_line = """        Line::from(vec![
            label_span(" MODEL      "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" LINK       "),
            value_span(llm_link_status(state, llm).0, llm_link_status(state, llm).1),
            llm_sep(),
            label_span("LAST "),
            value_span(&llm_last_sample_text(state), MUTED),
            llm_sep(),
            label_span("UPTIME "),
            value_span(&llm_uptime_text(state), MUTED),
        ]),
        Line::from(state),
"""
text = replace_once(text, model_line, link_line, "llm link row")
# Render text first, then use remaining rows for side-by-side prefill/decode graphs.
text = replace_once(
    text,
    "    lines.truncate(inner.height as usize);\n    frame.render_widget(Paragraph::new(lines), inner);\n}\n\nfn llm_metric_cell",
    "    lines.truncate(inner.height as usize);\n    let text_height = lines.len().min(inner.height as usize) as u16;\n    if text_height > 0 {\n        frame.render_widget(\n            Paragraph::new(lines),\n            Rect::new(inner.x, inner.y, inner.width, text_height),\n        );\n    }\n\n    let graph_height = inner.height.saturating_sub(text_height);\n    if graph_height >= 3 && inner.width >= 40 {\n        draw_llm_rate_history(\n            frame,\n            Rect::new(\n                inner.x,\n                inner.y.saturating_add(text_height),\n                inner.width,\n                graph_height,\n            ),\n            state,\n        );\n    }\n}\n\nfn llm_metric_cell",
    "llm graph render area",
)
# Make reconnecting override stale processing/generating phase.
text = replace_once(
    text,
    "fn llm_phase(llm: &LlmStats) -> (&'static str, Color) {\n    if llm.generation_tps > 0.05 {\n",
    "fn llm_phase(llm: &LlmStats) -> (&'static str, Color) {\n    if llm.reconnecting {\n        (\"RECONNECTING\", YELLOW)\n    } else if llm.generation_tps > 0.05 {\n",
    "llm phase reconnecting",
)
# Insert status and LLM graph helpers before llm_metric_cell.
helper_anchor = "fn llm_metric_cell(text: &str, width: usize, color: Color, bold: bool) -> Span<'static> {\n"
helpers = r'''fn llm_link_status(state: &UiState, llm: &LlmStats) -> (&'static str, Color) {
    if llm.reconnecting {
        ("RECONNECTING", YELLOW)
    } else if !llm.connected {
        ("OFFLINE", RED)
    } else if state
        .llm_connected_flash_until
        .is_some_and(|until| Instant::now() <= until)
    {
        ("CONNECTED", BRIGHT_GREEN)
    } else {
        ("ONLINE", ORK_GREEN)
    }
}

fn llm_last_sample_text(state: &UiState) -> String {
    state
        .llm_last_fresh_at
        .map(|at| format_sample_age(Instant::now().saturating_duration_since(at)))
        .unwrap_or_else(|| "—".to_string())
}

fn llm_uptime_text(state: &UiState) -> String {
    state
        .llm_connected_since
        .map(|at| format_uptime(Instant::now().saturating_duration_since(at)))
        .unwrap_or_else(|| "—".to_string())
}

fn format_sample_age(age: Duration) -> String {
    let millis = age.as_millis();
    if millis < 1_000 {
        return format!("{millis} ms");
    }
    let seconds = age.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1} s");
    }
    let total = age.as_secs();
    format!("{}m {:02}s", total / 60, total % 60)
}

fn format_uptime(uptime: Duration) -> String {
    let total = uptime.as_secs();
    let days = total / 86_400;
    let hours = (total / 3_600) % 24;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn draw_llm_rate_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_llm_rate_history_column(
        frame,
        cols[0],
        "PREFILL",
        &state.llm_prefill_history,
        CYAN,
        true,
    );
    draw_llm_rate_history_column(
        frame,
        cols[1],
        "DECODE",
        &state.llm_decode_history,
        ORK_GREEN,
        false,
    );
}

fn draw_llm_rate_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
) {
    let block = Block::default()
        .borders(if right_border { Borders::RIGHT } else { Borders::NONE })
        .border_style(Style::default().fg(INNER_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let latest = history.back().map(|sample| sample.value).unwrap_or(0.0);
    let scale = history_max(history).max(1.0);
    let header = format!(
        " {title} {} tok/s · max {}",
        compact_rate(latest),
        compact_rate(scale)
    );
    frame.render_widget(
        Paragraph::new(fit_cell(&header, inner.width as usize)).style(
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let graph = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(trend_lines_scaled(
            history,
            graph.width as usize,
            graph.height as usize,
            color,
            scale,
        )),
        graph,
    );
}

fn compact_rate(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        "0".to_string()
    } else if value >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

'''
text = replace_once(text, helper_anchor, helpers + helper_anchor, "llm status/graph helpers")
# Generalize history rendering from percentages to arbitrary positive metrics.
old_trend = r'''fn trend_lines(
    history: &VecDeque<TimedSample>,
    width: usize,
    height: usize,
    color: Color,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let now = Instant::now();
    let columns = resample_history(history, width * 2, now);
    let sub_height = height * 4;

    (0..height)
        .map(|cell_y| {
            let mut row = String::with_capacity(width);

            for cell_x in 0..width {
                let mut mask = 0u8;

                for dot_x in 0..2 {
                    let sample = columns[cell_x * 2 + dot_x];
                    let Some(sample) = sample else {
                        continue;
                    };

                    let mut filled = ((sample as f64 / 100.0) * sub_height as f64).round() as usize;
                    if sample > 0 && filled == 0 {
                        filled = 1;
                    }
                    filled = filled.min(sub_height);
                    let start = sub_height.saturating_sub(filled);

                    for dot_y in 0..4 {
                        let sub_y = cell_y * 4 + dot_y;
                        if sub_y >= start {
                            mask |= braille_bit(dot_x, dot_y);
                        }
                    }
                }

                row.push(braille_char(mask));
            }

            Line::from(Span::styled(row, Style::default().fg(color)))
        })
        .collect()
}
'''
new_trend = r'''fn trend_lines(
    history: &VecDeque<TimedSample>,
    width: usize,
    height: usize,
    color: Color,
) -> Vec<Line<'static>> {
    trend_lines_scaled(history, width, height, color, 100.0)
}

fn trend_lines_scaled(
    history: &VecDeque<TimedSample>,
    width: usize,
    height: usize,
    color: Color,
    scale_max: f64,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let height = height.max(1);
    let scale_max = scale_max.max(f64::EPSILON);
    let now = Instant::now();
    let columns = resample_history(history, width * 2, now);
    let sub_height = height * 4;

    (0..height)
        .map(|cell_y| {
            let mut row = String::with_capacity(width);

            for cell_x in 0..width {
                let mut mask = 0u8;

                for dot_x in 0..2 {
                    let sample = columns[cell_x * 2 + dot_x];
                    let Some(sample) = sample else {
                        continue;
                    };

                    let normalized = (sample / scale_max).clamp(0.0, 1.0);
                    let mut filled = (normalized * sub_height as f64).round() as usize;
                    if sample > 0.0 && filled == 0 {
                        filled = 1;
                    }
                    filled = filled.min(sub_height);
                    let start = sub_height.saturating_sub(filled);

                    for dot_y in 0..4 {
                        let sub_y = cell_y * 4 + dot_y;
                        if sub_y >= start {
                            mask |= braille_bit(dot_x, dot_y);
                        }
                    }
                }

                row.push(braille_char(mask));
            }

            Line::from(Span::styled(row, Style::default().fg(color)))
        })
        .collect()
}
'''
text = replace_once(text, old_trend, new_trend, "scaled trend lines")
text = replace_once(
    text,
    "fn resample_history(\n    history: &VecDeque<TimedSample>,\n    columns: usize,\n    now: Instant,\n) -> Vec<Option<u64>> {\n",
    "fn resample_history(\n    history: &VecDeque<TimedSample>,\n    columns: usize,\n    now: Instant,\n) -> Vec<Option<f64>> {\n",
    "float resample return",
)
text = replace_once(
    text,
    "                .then_some((window_secs - age.as_secs_f64(), sample.value.min(100)))\n",
    "                .then_some((window_secs - age.as_secs_f64(), sample.value.max(0.0)))\n",
    "raw metric resampling",
)
text = replace_once(
    text,
    "        value: clamp_percent(value) as u64,\n",
    "        value: clamp_percent(value),\n",
    "float percent history",
)
# Add metric history push/max helpers after percentage push_history_at.
metric_anchor = """fn percent(value: f64, total: f64) -> f64 {
"""
metric_helpers = r'''fn push_metric_history_at(history: &mut VecDeque<TimedSample>, value: f64, now: Instant) {
    history.push_back(TimedSample {
        at: now,
        value: if value.is_finite() { value.max(0.0) } else { 0.0 },
    });

    while history
        .front()
        .is_some_and(|sample| now.saturating_duration_since(sample.at) > HISTORY_WINDOW)
    {
        history.pop_front();
    }
    while history.len() > HISTORY_MAX_SAMPLES {
        history.pop_front();
    }
}

fn history_max(history: &VecDeque<TimedSample>) -> f64 {
    history
        .iter()
        .map(|sample| sample.value)
        .filter(|value| value.is_finite())
        .fold(0.0, f64::max)
}

'''
text = replace_once(text, metric_anchor, metric_helpers + metric_anchor, "metric history helpers")
# Update float-based tests and add LLM history/status coverage.
text = text.replace("assert_eq!(history.back().unwrap().value, 20);", "assert_eq!(history.back().unwrap().value, 20.0);")
text = text.replace("                value: 25,\n", "                value: 25.0,\n")
text = text.replace("                value: 100,\n", "                value: 100.0,\n")
text = text.replace("                value: 50,\n", "                value: 50.0,\n")
text = text.replace(
    "            vec![None, None, None, Some(25), Some(25), Some(50), Some(50)]\n",
    "            vec![\n                None,\n                None,\n                None,\n                Some(25.0),\n                Some(25.0),\n                Some(50.0),\n                Some(50.0),\n            ]\n",
)
text = replace_once(
    text,
    "    #[test]\n    fn endpoint_is_compact() {\n",
    "    #[test]\n    fn llm_history_ignores_held_reconnect_samples() {\n        let mut state = UiState::default();\n        state.observe_llm_sample(&LlmStats {\n            connected: true,\n            prompt_tps: 1200.0,\n            generation_tps: 75.0,\n            ..LlmStats::default()\n        });\n        assert_eq!(state.llm_prefill_history.len(), 1);\n        assert_eq!(state.llm_decode_history.len(), 1);\n        assert!(state.llm_last_fresh_at.is_some());\n        assert!(state.llm_connected_since.is_some());\n\n        state.observe_llm_sample(&LlmStats {\n            connected: true,\n            reconnecting: true,\n            prompt_tps: 9999.0,\n            generation_tps: 9999.0,\n            ..LlmStats::default()\n        });\n        assert_eq!(state.llm_prefill_history.len(), 1);\n        assert_eq!(state.llm_decode_history.len(), 1);\n    }\n\n    #[test]\n    fn endpoint_is_compact() {\n",
    "llm history test",
)
path.write_text(text)
