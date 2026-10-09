//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

pub(super) fn smooth_llm_sample_interval(previous_ms: Option<f64>, current_ms: f64) -> Option<f64> {
    const ALPHA: f64 = 0.2;
    if !current_ms.is_finite() || current_ms <= 0.0 {
        return previous_ms;
    }
    let current_ms = current_ms.clamp(1.0, 60_000.0);
    Some(
        match previous_ms.filter(|value| value.is_finite() && *value > 0.0) {
            Some(previous) => previous * (1.0 - ALPHA) + current_ms * ALPHA,
            None => current_ms,
        },
    )
}

pub(super) fn draw_llm_rate_history(frame: &mut Frame, area: Rect, state: &UiState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_llm_rate_history_column(
        frame,
        cols[0],
        "PREFILL",
        &state.llm_history.prefill,
        CYAN,
        true,
    );
    draw_llm_rate_history_column(
        frame,
        cols[1],
        "DECODE",
        &state.llm_history.decode,
        ORK_GREEN,
        false,
    );
}

pub(super) fn draw_llm_rate_history_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<TimedSample>,
    color: Color,
    right_border: bool,
) {
    let block = Block::default()
        .borders(if right_border {
            Borders::RIGHT
        } else {
            Borders::NONE
        })
        .border_style(Style::default().fg(INNER_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let latest = history.back().map(|sample| sample.value).unwrap_or(0.0);
    if !llm_rate_history_has_activity(history) {
        let header = format!(" {title} idle");
        frame.render_widget(
            Paragraph::new(fit_cell(&header, inner.width as usize))
                .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        return;
    }

    let scale = history_max(history).max(1.0);
    let header = format!(
        " {title} {} tok/s · max {}",
        compact_rate(latest),
        compact_rate(scale)
    );
    frame.render_widget(
        Paragraph::new(fit_cell(&header, inner.width as usize))
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
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

pub(super) fn llm_rate_history_has_activity(history: &VecDeque<TimedSample>) -> bool {
    history.iter().any(|sample| sample.value > 0.05)
}

pub(super) fn draw_history(frame: &mut Frame, area: Rect, state: &UiState) {
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

pub(super) fn draw_history_column(
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
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height < 2 {
        return;
    }

    let latest = history.back().map(|sample| sample.value).unwrap_or(0.0);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {title:<5}"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{latest:>3.0}%"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(trend_lines(
            history,
            rows[1].width as usize,
            rows[1].height as usize,
            color,
        )),
        rows[1],
    );
}

pub(super) fn trend_lines(
    history: &VecDeque<TimedSample>,
    width: usize,
    height: usize,
    color: Color,
) -> Vec<Line<'static>> {
    trend_lines_scaled(history, width, height, color, 100.0)
}

pub(super) fn trend_lines_scaled(
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

pub(super) fn resample_history(
    history: &VecDeque<TimedSample>,
    columns: usize,
    now: Instant,
) -> Vec<Option<f64>> {
    let columns = columns.max(1);
    let window_secs = HISTORY_WINDOW.as_secs_f64();
    let mut points = history
        .iter()
        .filter_map(|sample| {
            let age = now.saturating_duration_since(sample.at);
            (age <= HISTORY_WINDOW)
                .then_some((window_secs - age.as_secs_f64(), sample.value.max(0.0)))
        })
        .peekable();

    let mut result = vec![None; columns];
    let mut current = None;

    for (x, value) in result.iter_mut().enumerate() {
        let target = if columns == 1 {
            window_secs
        } else {
            x as f64 / (columns - 1) as f64 * window_secs
        };

        while let Some(&(offset, sample)) = points.peek() {
            if offset > target {
                break;
            }
            current = Some(sample);
            points.next();
        }

        *value = current;
    }

    result
}

pub(super) fn braille_bit(x: usize, y: usize) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

pub(super) fn braille_char(mask: u8) -> char {
    if mask == 0 {
        ' '
    } else {
        char::from_u32(0x2800 + mask as u32).unwrap_or(' ')
    }
}

pub(super) fn push_history_at(history: &mut VecDeque<TimedSample>, value: f64, now: Instant) {
    history.push_back(TimedSample {
        at: now,
        value: clamp_percent(value),
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

/// Fold one LLM sample into a server's history/connection state. A missing
/// metric is never appended as a fabricated zero; only a present rate enters
/// the graph, keeping unknown distinct from a real `0`.
pub(super) fn observe_llm_into(history: &mut LlmHistory, llm: &LlmStats, now: Instant) {
    if llm.connected && !llm.reconnecting {
        if !history.was_connected {
            history.connected_since = Some(now);
            history.connected_flash_until = Some(now + Duration::from_secs(2));
            history.sample_interval_ema_ms = None;
        } else if let Some(previous) = history.last_fresh_at {
            let interval_ms = now.saturating_duration_since(previous).as_secs_f64() * 1_000.0;
            history.sample_interval_ema_ms =
                smooth_llm_sample_interval(history.sample_interval_ema_ms, interval_ms);
        }
        history.was_connected = true;
        history.last_fresh_at = Some(now);
        if llm.metrics_available {
            if let Some(value) = llm.prompt_tps {
                push_metric_history_at(&mut history.prefill, value, now);
            }
            if let Some(value) = llm.generation_tps {
                push_metric_history_at(&mut history.decode, value, now);
            }
        }
    } else if !llm.connected {
        history.was_connected = false;
        history.connected_since = None;
        history.connected_flash_until = None;
    }
}

pub(super) fn push_metric_history_at(
    history: &mut VecDeque<TimedSample>,
    value: f64,
    now: Instant,
) {
    history.push_back(TimedSample {
        at: now,
        value: if value.is_finite() {
            value.max(0.0)
        } else {
            0.0
        },
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

pub(super) fn history_max(history: &VecDeque<TimedSample>) -> f64 {
    let now = Instant::now();
    history
        .iter()
        .filter(|sample| now.saturating_duration_since(sample.at) <= HISTORY_WINDOW)
        .map(|sample| sample.value)
        .filter(|value| value.is_finite())
        .fold(0.0, f64::max)
}
