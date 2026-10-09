//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

pub(super) fn draw_llm(
    frame: &mut Frame,
    area: Rect,
    llm: &LlmStats,
    state: &UiState,
    gpu_map: &GpuMapping,
) {
    let block = Block::default()
        .title(" LLM INFERENCE ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    if !llm.connected {
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
                    value_span(&llm_last_sample_text(state, llm), MUTED),
                ]),
                Line::from(vec![label_span(" UPTIME      "), value_span("—", MUTED)]),
            ]),
            inner,
        );
        return;
    }

    // Reachable server without /metrics: keep the /slots-derived state (slot
    // counts, CTX, per-slot overview) visible while every metric-derived
    // value is shown as unavailable rather than a fabricated zero.
    if !llm.metrics_available {
        draw_llm_metrics_unavailable(frame, inner, llm, state);
        return;
    }

    // The CTX row shows current occupancy only. Without `/slots` the occupancy
    // is unknown (`— / capacity`); the `/metrics` high-water mark is a
    // historical lifetime peak and must never stand in as the current value.
    // It stays available, explicitly labelled, in `orsiktop diag`.
    let context_used = llm.context_used;
    let context_pct = match (context_used, llm.context_size) {
        (Some(used), size) if size > 0 => (used as f64 / size as f64 * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    };
    let context_bar = inner.width.saturating_sub(46).max(8) as usize;
    // Multi-slot servers show which slot supplies the displayed CTX pair,
    // using the slot's own /slots `id`. Hidden for a single slot
    // (unambiguous) and when the selected slot reports no id (never a fake
    // number). Below the width where the CTX bar stops being pinned (54 =
    // 46 + 8) the tag is dropped before the context values so the row
    // cannot overflow further.
    let context_slot_tag = (llm.slot_count > 1)
        .then_some(llm.context_slot_id)
        .flatten()
        .filter(|_| inner.width >= 54)
        .map(|id| format!("S{id}"));
    let mut context_suffix = Vec::new();
    if let Some(tag) = &context_slot_tag {
        context_suffix.push(Span::styled(format!(" {tag}"), Style::default().fg(CYAN)));
    }
    context_suffix.push(Span::styled(
        match (context_used, llm.context_size) {
            (Some(used), size) if size > 0 => {
                format!(" {} / {} tok", grouped_u64(used), grouped_u64(size))
            }
            (None, size) if size > 0 => format!(" — / {} tok", grouped_u64(size)),
            _ => " waiting for context".to_string(),
        },
        Style::default().fg(MUTED),
    ));
    // An unknown occupancy or capacity renders the meter as unavailable (`—`),
    // never as a fabricated `0.0%`; a known zero stays a real `0.0%`.
    let context_line = if context_used.is_some() && llm.context_size > 0 {
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            context_suffix,
        )
    } else {
        unavailable_meter_line("CTX", context_bar, context_suffix)
    };
    let slots = if llm.slots_available {
        format!("{}/{}", llm.busy_slots, llm.slot_count)
    } else if llm.props_slot_count > 0 {
        format!("—/{}", llm.props_slot_count)
    } else {
        "—".to_string()
    };
    let (mtp, mtp_color) = if llm.spec_is_mtp {
        (
            llm.spec_n_max
                .filter(|value| *value > 0)
                .map(|value| format!("MTP{value}"))
                .unwrap_or_else(|| "MTP".to_string()),
            CYAN,
        )
    } else if llm.spec_enabled {
        ("SPEC".to_string(), CYAN)
    } else {
        ("MTP OFF".to_string(), MUTED)
    };
    let (acc, acc_color) = match llm.spec_acceptance_pct {
        Some(value) => (format!("{value:.0}%"), ORK_GREEN),
        None => ("—".to_string(), MUTED),
    };
    let (phase, phase_color) = llm_phase(llm);

    let live_pp = llm.prompt_tps.map_or_else(
        || "— tok/s".to_string(),
        |value| format!("{value:.1} tok/s"),
    );
    let live_tg = llm.generation_tps.map_or_else(
        || "— tok/s".to_string(),
        |value| format!("{value:.1} tok/s"),
    );
    let request_pp = match (llm.busy_slots > 0, llm.request_prompt_tokens) {
        (true, Some(tokens)) => format!("{} tok", grouped_u64(tokens)),
        _ => "— tok".to_string(),
    };
    let request_tg = match (llm.busy_slots > 0, llm.request_generated_tokens) {
        (true, Some(tokens)) => format!("{} tok", grouped_u64(tokens)),
        _ => "— tok".to_string(),
    };
    let total_pp = llm.prompt_total.map_or_else(
        || "— tok".to_string(),
        |value| format!("{} tok", grouped_f64(value)),
    );
    let total_tg = llm.generated_total.map_or_else(
        || "— tok".to_string(),
        |value| format!("{} tok", grouped_f64(value)),
    );
    let cache = llm
        .prompt_cached_total
        .map(grouped_f64)
        .unwrap_or_else(|| "—".to_string());
    let cache_color = if llm.prompt_cached_total.is_some() {
        CYAN
    } else {
        MUTED
    };
    let cache_share = llm
        .prompt_cached_total
        .zip(llm.prompt_total)
        .and_then(|(cached, total)| {
            (total > 0.0).then_some((cached / total * 100.0).clamp(0.0, 100.0))
        });
    let spec_total_acceptance = llm
        .spec_draft_tokens
        .zip(llm.spec_accepted_tokens)
        .and_then(|(draft, accepted)| {
            (draft > 0.0).then_some((accepted / draft * 100.0).clamp(0.0, 100.0))
        });
    let req = match (llm.active_requests, llm.deferred_requests) {
        (Some(active), Some(deferred)) => format!("{active:.0}/{deferred:.0}"),
        _ => "—".to_string(),
    };
    let req_color = if llm.active_requests.is_some() && llm.deferred_requests.is_some() {
        WHITE
    } else {
        MUTED
    };

    let pp_active = llm.prompt_tps.is_some_and(|value| value > 0.05);
    let tg_active = llm.generation_tps.is_some_and(|value| value > 0.05);
    let pp_header_color = if pp_active { CYAN } else { MUTED };
    let tg_header_color = if tg_active { ORK_GREEN } else { MUTED };
    let pp_live_color = if pp_active { CYAN } else { MUTED };
    let tg_live_color = if tg_active { ORK_GREEN } else { MUTED };
    let request_color = if llm.busy_slots > 0 { WHITE } else { MUTED };

    let metric_width = ((inner.width as usize).saturating_sub(12) / 2).clamp(16, 30);
    let mut state_line = vec![
        label_span(" STATE      "),
        value_span(phase, phase_color),
        llm_sep(),
        label_span("SLOTS "),
        value_span(&slots, CYAN),
        llm_sep(),
        label_span("REQ "),
        value_span(&req, req_color),
        llm_sep(),
        value_span(&mtp, mtp_color),
        llm_sep(),
        label_span("ACC "),
        value_span(&acc, acc_color),
    ];
    if inner.width >= 82 {
        state_line.push(llm_sep());
        state_line.push(label_span("CACHE "));
        state_line.push(value_span(&cache, cache_color));
    }
    if inner.width < 66 {
        state_line = vec![
            label_span(" STATE      "),
            value_span(phase, phase_color),
            Span::raw("  "),
            label_span("SLOTS "),
            value_span(&slots, CYAN),
            Span::raw("  "),
            value_span(&mtp, mtp_color),
        ];
    }
    if inner.width >= 103 {
        state_line.push(llm_sep());
        state_line.push(label_span("GPU "));
        // The selected server's verified local GPU association: its stable key
        // (BDF/UUID) when a single device matched, a `+N` suffix when several
        // are attributed, otherwise "—" (not determined — distinct from
        // CPU-only or 0% utilization).
        match gpu_attribution_label(gpu_map) {
            Some(label) => state_line.push(value_span(&label, CYAN)),
            None => state_line.push(value_span("—", MUTED)),
        }
    }

    let total_line = vec![
        label_span(" TOTAL      "),
        llm_metric_cell(&total_pp, metric_width, WHITE, false),
        llm_metric_cell(&total_tg, metric_width, WHITE, false),
    ];

    let mut lines = vec![
        Line::from(vec![
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
            label_span("UPTIME "),
            value_span(&llm_uptime_text(state), MUTED),
            llm_sep(),
            label_span("POLL "),
            value_span(&llm_last_sample_text(state, llm), MUTED),
        ]),
        Line::from(state_line),
        Line::from(vec![
            label_span("            "),
            llm_metric_cell("PREFILL / PP", metric_width, pp_header_color, pp_active),
            llm_metric_cell("DECODE / TG", metric_width, tg_header_color, tg_active),
        ]),
        Line::from(vec![
            label_span(" LIVE       "),
            llm_metric_cell(&live_pp, metric_width, pp_live_color, pp_active),
            llm_metric_cell(&live_tg, metric_width, tg_live_color, tg_active),
        ]),
        Line::from(vec![
            label_span(" AVG (LIFE) "),
            llm_metric_cell(
                &llm.prompt_avg_tps.map_or_else(
                    || "— tok/s".to_string(),
                    |value| format!("{value:.1} tok/s"),
                ),
                metric_width,
                MUTED,
                false,
            ),
            llm_metric_cell(
                &llm.generation_avg_tps.map_or_else(
                    || "— tok/s".to_string(),
                    |value| format!("{value:.1} tok/s"),
                ),
                metric_width,
                MUTED,
                false,
            ),
        ]),
        Line::from(vec![
            label_span(" REQUEST    "),
            llm_metric_cell(&request_pp, metric_width, request_color, false),
            llm_metric_cell(&request_tg, metric_width, request_color, false),
        ]),
        Line::from(total_line),
        context_line,
    ];

    // Compact per-slot context overview, directly below the CTX row it
    // supplements. Multi-slot servers only; on narrow widths the whole row is
    // dropped before any core metric is.
    if llm.slots_available && llm.slot_count > 1 {
        if let Some(overview) =
            llm_slot_overview_line(&llm.slot_overview, llm.context_slot_id, inner.width)
        {
            lines.push(overview);
        }
    }

    // The LLM pane is usually taller than its core metric set because it shares a row
    // with the system pane. Use that spare vertical space for useful cumulative detail.
    if inner.height >= 10 {
        let cache_tokens = llm
            .prompt_cached_total
            .map(|value| format!("{} tok", grouped_f64(value)))
            .unwrap_or_else(|| "—".to_string());
        let cache_ratio = cache_share
            .map(|value| format!("{value:.1}% of PP"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(vec![
            label_span(" CACHE      "),
            llm_metric_cell(&cache_tokens, metric_width, cache_color, false),
            llm_metric_cell(&cache_ratio, metric_width, MUTED, false),
        ]));
    }

    if inner.height >= 11 {
        lines.push(llm_spec_row(llm, metric_width, spec_total_acceptance));
    }

    if inner.height >= 12 {
        lines.push(Line::from(vec![
            label_span(" TIME       "),
            llm_metric_cell(
                &llm.prompt_seconds_total
                    .map_or_else(|| "— s".to_string(), |value| format!("{value:.1} s")),
                metric_width,
                MUTED,
                false,
            ),
            llm_metric_cell(
                &llm.generation_seconds_total
                    .map_or_else(|| "— s".to_string(), |value| format!("{value:.1} s")),
                metric_width,
                MUTED,
                false,
            ),
        ]));
    }

    lines.truncate(inner.height as usize);
    let text_height = lines.len().min(inner.height as usize) as u16;
    if text_height > 0 {
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(inner.x, inner.y, inner.width, text_height),
        );
    }

    let graph_height = inner.height.saturating_sub(text_height);
    if graph_height >= 3 && inner.width >= 40 {
        draw_llm_rate_history(
            frame,
            Rect::new(
                inner.x,
                inner.y.saturating_add(text_height),
                inner.width,
                graph_height,
            ),
            state,
        );
    }
}

/// Panel rendered when the server is reachable (`connected`) but `/metrics`
/// is unavailable or disabled. The `/slots`-derived state — slot counts, the
/// selected CTX pair and the per-slot overview — stays visible; every
/// metric-derived value is omitted rather than shown as a fabricated zero,
/// and the reason is stated explicitly.
pub(super) fn draw_llm_metrics_unavailable(
    frame: &mut Frame,
    inner: Rect,
    llm: &LlmStats,
    state: &UiState,
) {
    // Current occupancy only: the high-water mark is historical and is not the
    // current context value (see `draw_llm`).
    let context_used = llm.context_used;
    let context_pct = match (context_used, llm.context_size) {
        (Some(used), size) if size > 0 => (used as f64 / size as f64 * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    };
    let context_bar = inner.width.saturating_sub(46).max(8) as usize;
    let context_slot_tag = (llm.slot_count > 1)
        .then_some(llm.context_slot_id)
        .flatten()
        .filter(|_| inner.width >= 54)
        .map(|id| format!("S{id}"));
    let mut context_suffix = Vec::new();
    if let Some(tag) = &context_slot_tag {
        context_suffix.push(Span::styled(format!(" {tag}"), Style::default().fg(CYAN)));
    }
    context_suffix.push(Span::styled(
        match (context_used, llm.context_size) {
            (Some(used), size) if size > 0 => {
                format!(" {} / {} tok", grouped_u64(used), grouped_u64(size))
            }
            (None, size) if size > 0 => format!(" — / {} tok", grouped_u64(size)),
            _ => " waiting for context".to_string(),
        },
        Style::default().fg(MUTED),
    ));
    // An unknown occupancy or capacity renders the meter as unavailable (`—`),
    // never as a fabricated `0.0%`; a known zero stays a real `0.0%`.
    let context_line = if context_used.is_some() && llm.context_size > 0 {
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            context_suffix,
        )
    } else {
        unavailable_meter_line("CTX", context_bar, context_suffix)
    };
    let slots = if llm.slots_available {
        format!("{}/{}", llm.busy_slots, llm.slot_count)
    } else if llm.props_slot_count > 0 {
        format!("—/{}", llm.props_slot_count)
    } else {
        "—".to_string()
    };
    let (link, link_color) = llm_link_status(state, llm);

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL      "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" LINK       "),
            value_span(link, link_color),
            llm_sep(),
            label_span("UPTIME "),
            value_span(&llm_uptime_text(state), MUTED),
            llm_sep(),
            label_span("POLL "),
            value_span(&llm_last_sample_text(state, llm), MUTED),
        ]),
        Line::from(vec![
            label_span(" METRICS    "),
            value_span("UNAVAILABLE", YELLOW),
            llm_sep(),
            label_span("SLOTS "),
            value_span(&slots, CYAN),
        ]),
        Line::from(vec![
            label_span(" LLAMA      "),
            Span::styled(
                metrics_unavailable_reason(&llm.error),
                Style::default().fg(MUTED),
            ),
        ]),
        context_line,
    ];

    if llm.slots_available && llm.slot_count > 1 {
        if let Some(overview) =
            llm_slot_overview_line(&llm.slot_overview, llm.context_slot_id, inner.width)
        {
            lines.push(overview);
        }
    }

    lines.truncate(inner.height as usize);
    if !lines.is_empty() {
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

pub(super) fn llm_link_status(state: &UiState, llm: &LlmStats) -> (&'static str, Color) {
    if llm.reconnecting {
        ("RECONNECTING", YELLOW)
    } else if !llm.connected {
        ("OFFLINE", RED)
    } else if state
        .llm_history
        .connected_flash_until
        .is_some_and(|until| Instant::now() <= until)
    {
        ("CONNECTED", BRIGHT_GREEN)
    } else {
        ("ONLINE", ORK_GREEN)
    }
}

pub(super) fn llm_last_sample_text(state: &UiState, llm: &LlmStats) -> String {
    if llm.connected && !llm.reconnecting {
        if let Some(avg_ms) = state.llm_history.sample_interval_ema_ms {
            return format!("~{avg_ms:.0} ms avg");
        }
    }

    state
        .llm_history
        .last_fresh_at
        .map(|at| {
            format!(
                "{} ago",
                format_sample_age(Instant::now().saturating_duration_since(at))
            )
        })
        .unwrap_or_else(|| "—".to_string())
}

pub(super) fn llm_uptime_text(state: &UiState) -> String {
    state
        .llm_history
        .connected_since
        .map(|at| format_uptime(Instant::now().saturating_duration_since(at)))
        .unwrap_or_else(|| "—".to_string())
}

pub(super) fn llm_spec_row(
    llm: &LlmStats,
    metric_width: usize,
    spec_total_acceptance: Option<f64>,
) -> Line<'static> {
    if !llm.spec_enabled {
        return Line::from(vec![label_span(" SPEC       "), value_span("OFF", MUTED)]);
    }
    if llm.spec_drafts_total.is_none()
        && llm.spec_draft_tokens.is_none()
        && llm.spec_accepted_tokens.is_none()
    {
        // The server reports speculative decoding as enabled but exposes no
        // draft/accept counters (older builds, or an idle server). Say so
        // neutrally instead of asserting a server version or a broken read.
        return Line::from(vec![
            label_span(" SPEC TOK   "),
            llm_metric_cell("no counters", metric_width, MUTED, false),
            llm_metric_cell("—", metric_width, MUTED, false),
        ]);
    }
    let draft = llm.spec_draft_tokens.map_or_else(
        || "— draft".to_string(),
        |value| format!("{} draft", grouped_f64(value)),
    );
    let accepted = match (spec_total_acceptance, llm.spec_accepted_tokens) {
        (Some(rate), Some(accepted)) => {
            format!("{} accepted · {rate:.1}%", grouped_f64(accepted))
        }
        (None, Some(accepted)) => format!("{} accepted", grouped_f64(accepted)),
        (_, None) => "— accepted".to_string(),
    };
    Line::from(vec![
        label_span(" SPEC TOK   "),
        llm_metric_cell(&draft, metric_width, CYAN, false),
        llm_metric_cell(&accepted, metric_width, ORK_GREEN, false),
    ])
}

pub(super) fn llm_phase(llm: &LlmStats) -> (&'static str, Color) {
    // A server-reported phase (Strata) is authoritative for the request phase:
    // PREFILL while reading the prompt, and GENERATING even when the windowed
    // rate momentarily reads zero. An absent/unknown phase falls through to the
    // existing activity heuristics, so llama.cpp is unchanged.
    let strata_phase = llm.strata_phase;
    let generating = strata_phase == Some(crate::strata::StrataPhase::Generating)
        || llm.generation_tps.is_some_and(|value| value > 0.05);
    let reading = strata_phase == Some(crate::strata::StrataPhase::Reading)
        || llm.prompt_tps.is_some_and(|value| value > 0.05);
    if llm.reconnecting {
        ("RECONNECTING", YELLOW)
    } else if generating {
        ("GENERATING", ORK_GREEN)
    } else if reading {
        ("PREFILL", CYAN)
    } else if llm.busy_slots > 0 || llm.active_requests.is_some_and(|value| value > 0.0) {
        ("PROCESSING", YELLOW)
    } else if llm.deferred_requests.is_some_and(|value| value > 0.0) {
        ("QUEUED", YELLOW)
    } else {
        ("IDLE", MUTED)
    }
}

/// One entry per slot for the per-slot context overview. The slot's own
/// `/slots` id leads (`S7`), followed by its current occupancy. When every
/// visible slot reports the same known capacity the entry stays compact
/// (`S0 58.7k`); when capacities differ the entry shows
/// `used/capacity` per slot (`S0 58.7/115.2k`) — a differing or unknown
/// capacity is never hidden. A missing occupancy prints `—`, never a fake
/// zero. The slot that supplies the main CTX row is marked with a trailing
/// `*` (never by color alone); busy entries are highlighted, idle ones dimmed.
pub(super) fn slot_overview_entries(
    slots: &[LlmSlotInfo],
    selected: Option<u64>,
) -> Vec<(String, Color)> {
    let unified_capacity = slots
        .iter()
        .map(|slot| slot.context_size)
        .collect::<Option<Vec<_>>>()
        .is_some_and(|capacities| capacities.iter().all(|cap| *cap == capacities[0]));

    slots
        .iter()
        .map(|slot| {
            let mut text = format!("S{}", slot.id);
            match slot.context_used {
                Some(used) => {
                    let used = compact_tokens(used);
                    text.push(' ');
                    if unified_capacity && slot.context_size.is_some() {
                        text.push_str(&used);
                    } else {
                        // Mixed/unknown capacities: `used` loses its own `k`
                        // so the pair reads `58.7/115.2k` (one trailing unit).
                        let used_number = used.trim_end_matches('k');
                        let capacity = slot
                            .context_size
                            .map(compact_tokens)
                            .unwrap_or_else(|| "—".to_string());
                        text.push_str(&format!("{used_number}/{capacity}"));
                    }
                }
                None => text.push_str(" —"),
            }
            if selected == Some(slot.id) {
                text.push('*');
            }
            (text, if slot.busy { WHITE } else { MUTED })
        })
        .collect()
}

/// The width-aware `SLOTS` overview line for multi-slot servers: a padded
/// label, one `S<n> value` entry per slot with 3-space gaps, and a `  +n`
/// suffix when not every entry fits. Entries are dropped from the end
/// (highest slot IDs first) deterministically. Returns `None` when even a
/// single entry cannot fit — the row is omitted entirely rather than
/// overflowing the panel.
pub(super) fn llm_slot_overview_line(
    slots: &[LlmSlotInfo],
    selected: Option<u64>,
    width: u16,
) -> Option<Line<'static>> {
    let entries = slot_overview_entries(slots, selected);
    if entries.is_empty() {
        return None;
    }

    const LABEL_LEN: usize = 12; // " SLOTS      "
    const ENTRY_GAP: usize = 3;
    // `  +n` is two spaces + `+` + the digit count.
    let suffix_len = |hidden: usize| {
        if hidden > 0 {
            3 + hidden.to_string().len()
        } else {
            0
        }
    };
    // Mirrors the rendering below exactly: label, one 3-space gap between
    // entries (none after the padded label), and the remainder suffix.
    let fits = |kept: usize| {
        let hidden = entries.len() - kept;
        // Measure display cells, not UTF-8 bytes: `—` is three bytes but one
        // cell, so a byte count would hide slots that actually fit.
        let text: usize = entries[..kept]
            .iter()
            .map(|(text, _)| text.chars().count())
            .sum();
        LABEL_LEN + text + ENTRY_GAP * (kept - 1) + suffix_len(hidden) <= width as usize
    };

    let mut kept = entries.len();
    while kept > 0 && !fits(kept) {
        kept -= 1;
    }
    if kept == 0 {
        return None;
    }
    let hidden = entries.len() - kept;

    let mut spans = vec![label_span(" SLOTS      ")];
    for (index, (text, color)) in entries[..kept].iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(value_span(text, *color));
    }
    if hidden > 0 {
        spans.push(value_span(&format!("  +{hidden}"), MUTED));
    }
    Some(Line::from(spans))
}

pub(super) fn context_color(percent: f64) -> Color {
    match percent {
        v if v >= 97.0 => RED,
        v if v >= 90.0 => ORANGE,
        v if v >= 80.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

pub(super) fn llm_metrics_disabled(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("501") || lower.contains("--metrics") || lower.contains("metrics endpoint")
}

/// The reason line for the reachable-server-without-`/metrics` panel. Unlike
/// `friendly_llm_error`, it never claims the server is offline: this panel is
/// only drawn while another endpoint (`/slots`) is answering.
pub(super) fn metrics_unavailable_reason(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if llm_metrics_disabled(error) {
        "LLAMA METRICS OFF · restart server with --metrics".to_string()
    } else if lower.contains("cannot reach") {
        "LLAMA METRICS UNREACHABLE · /slots still live".to_string()
    } else {
        format!("LLAMA METRICS · {error}")
    }
}

pub(super) fn friendly_llm_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if llm_metrics_disabled(error) {
        "LLAMA METRICS OFF · restart server with --metrics".to_string()
    } else if lower.contains("cannot reach") || lower.contains("connection") {
        "LLAMA SERVER OFFLINE".to_string()
    } else {
        format!("LLAMA · {error}")
    }
}
