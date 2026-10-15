//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use std::time::Duration;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::widgets::{compact_endpoint, fit_cell, format_sample_age, pad_cell};
use super::{
    SettingsField, UiState, BRIGHT_GREEN, CYAN, DIM_GREEN, MUTED, ORK_GREEN, PROCESS_SELECTED_BG,
    RED, WHITE, YELLOW,
};
use crate::domain::{ActivityPhase, ServerSummary};

pub(super) fn draw_help_popup(frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(6).min(72);
    let height = area.height.saturating_sub(4).min(22);
    if width < 48 || height < 16 {
        // The full popup does not fit. Still draw a reduced help notice when
        // the compact layout advertises `[h]`; otherwise opening help would
        // leave the UI in an invisible, key-swallowing mode.
        draw_help_compact(frame, area);
        return;
    }

    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" HELP · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let key = |text: &'static str| {
        Span::styled(
            format!(" {text:<9}"),
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |text: &'static str| Span::styled(text, Style::default().fg(WHITE));
    let heading = |text: &'static str| {
        Span::styled(text, Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
    };

    // Concise enough to fit the smallest full-help popup (inner height 16 at
    // 72x22): the field legend first, then the keys. The legend is where the
    // CTX-vs-REQUEST distinction and the `—`/freshness rules are explained,
    // because the dense panel and selector cells cannot say it themselves.
    let lines = vec![
        Line::from(heading(" READING THE PANEL")),
        Line::from(vec![
            key("CTX"),
            desc("current context occupancy of the shown slot"),
        ]),
        Line::from(vec![
            key("REQUEST"),
            desc("in-flight request tokens: prompt / generated"),
        ]),
        Line::from(vec![
            key("SLOTS"),
            desc("busy / total slots (S in the selector)"),
        ]),
        Line::from(vec![
            key("PP/TG"),
            desc("prefill / decode rate (LIVE · AVG · TOTAL)"),
        ]),
        Line::from(vec![
            key("CACHE"),
            desc("lifetime cached prompt tokens; no ratio shown"),
        ]),
        Line::from(vec![
            key("STATE"),
            desc("IDLE/PREFILL/GENERATING/… only with evidence"),
        ]),
        Line::from(vec![
            key("SAMPLE"),
            desc("sample interval, or age / STALE when not fresh"),
        ]),
        Line::from(vec![
            key("—"),
            desc("unknown or unavailable — never a zero"),
        ]),
        Line::from(heading(" KEYBOARD")),
        Line::from(Span::styled(
            " Esc quit · h help · q settings · s servers · / search",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " -/+ refresh · ↑/k ↓/j select · PgUp/PgDn jump · Home/End first/last",
            Style::default().fg(WHITE),
        )),
        Line::from(heading(" MOUSE")),
        Line::from(Span::styled(
            " click select · double-click pin · right-click expand group",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " header click sort · wheel scroll · [-]/[+] refresh interval",
            Style::default().fg(WHITE),
        )),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Reduced help shown when the full popup does not fit — e.g. the compact
/// dashboard range, where `[h]` is still advertised. It lists the essential
/// keys so help is never an invisible mode. Nothing is drawn when the area is
/// too small even for this notice.
fn draw_help_compact(frame: &mut Frame, area: Rect) {
    if area.width < 24 || area.height < 3 {
        return;
    }
    let width = area.width.saturating_sub(4).min(56);
    let height = area.height.saturating_sub(2).clamp(3, 8);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" HELP · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(Span::styled(
            " CTX context · REQUEST in-flight request",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " Esc / h  close help",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " s  servers    q  settings",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " - / +  refresh interval",
            Style::default().fg(WHITE),
        )),
        Line::from(Span::styled(
            " /  search    ↑ / ↓  select",
            Style::default().fg(WHITE),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Compact notice shown when the terminal is too small for the settings
/// popup; nothing is rendered when the area cannot fit the notice itself.
pub(super) fn draw_settings_too_small(frame: &mut Frame, area: Rect) {
    if area.width < 12 || area.height < 5 {
        return;
    }
    let width = 48.min(area.width);
    let height = 3.min(area.height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" SETTINGS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Terminal too small for settings (need 60x19)",
            Style::default().fg(MUTED),
        ))),
        inner,
    );
}

/// Rows the overflow block needs: one header plus one row per configured
/// endpoint beyond the monitoring cap. Zero when there is no overflow.
fn overflow_line_count(overflow: &[crate::app::ServerSpec]) -> usize {
    if overflow.is_empty() {
        0
    } else {
        overflow.len() + 1
    }
}

/// Scroll offset (in rows) that keeps `highlight` inside a window of `visible`
/// rows out of `total`, without ever scrolling past the end. The last row is
/// therefore reachable by moving the highlight, and the highlighted row is
/// always inside the window.
fn selector_scroll(highlight: usize, total: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let max_scroll = total - visible;
    highlight
        .saturating_sub(visible.saturating_sub(1))
        .min(max_scroll)
}

/// Color for a per-server activity phase in the selector. Unknown/absent
/// phases are muted; generating, prefill and active phases get the same colors
/// as the selected-server panel.
fn selector_phase_color(phase: Option<ActivityPhase>) -> Color {
    match phase {
        Some(ActivityPhase::Generating) => ORK_GREEN,
        Some(ActivityPhase::Prefill) => CYAN,
        Some(ActivityPhase::Processing | ActivityPhase::Queued | ActivityPhase::Reconnecting) => {
            YELLOW
        }
        _ => MUTED,
    }
}

/// Multi-server selector/overview popup. Lists every known server with its
/// state, activity phase, measurement age, backend, slot counts, context and
/// throughput; the selected server is marked `*` and the highlighted row `>`.
/// The list scrolls so the highlighted row stays visible and the title carries
/// an `N/M` position indicator. Endpoints are redacted before display.
pub(super) fn draw_server_selector(
    frame: &mut Frame,
    area: Rect,
    servers: &[ServerSummary],
    selected_server: &str,
    highlight: usize,
    overflow: &[crate::app::ServerSpec],
) {
    let overflow_rows = overflow_line_count(overflow) as u16;
    let width = area.width.saturating_sub(6).min(78);
    // Two lines per server (a header line and a detail line), so the popup can
    // show every required field without truncating a value.
    let height = (servers.len() as u16 * 2 + overflow_rows + 4)
        .min(area.height.saturating_sub(4))
        .max(5);
    if width < 48 || height < 5 {
        return;
    }
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);

    let highlight = if servers.is_empty() {
        0
    } else {
        highlight.min(servers.len() - 1)
    };
    // Position indicator so a scrolled list still tells the user how many
    // servers exist and which one is highlighted.
    let position = if servers.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", highlight + 1, servers.len())
    };
    let block = Block::default()
        .title(format!(
            " LLM SERVERS {position} · Up/Down · Enter select · Esc close "
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let inner_width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(servers.len() * 2);
    for (index, server) in servers.iter().enumerate() {
        let is_selected = server.key == selected_server;
        let marker = if index == highlight { ">" } else { " " };
        let star = if is_selected { "*" } else { " " };
        let (state, state_color) = if server.connected {
            ("connected", ORK_GREEN)
        } else if server.reconnecting {
            ("reconnecting", YELLOW)
        } else {
            ("offline", MUTED)
        };
        let ctx = match (server.context_used, server.context_size) {
            (Some(used), size) if size > 0 => format!("{used}/{size}"),
            (None, size) if size > 0 => format!("—/{size}"),
            _ => "—".to_string(),
        };
        let tps = server.generation_tps.map_or_else(
            || "— tok/s".to_string(),
            |value| format!("{value:.1} tok/s"),
        );
        // A missing `/slots` endpoint means the busy count is unknown: render
        // `—/—` (or `—/N` when `/props` still knows the total) rather than a
        // fabricated `0/0`. A real empty `/slots` array stays `0/0`, and a
        // valid array shows its actual busy/total.
        let slots = if server.slots_available {
            format!("{}/{}", server.busy_slots, server.slot_count)
        } else if server.props_slot_count > 0 {
            format!("—/{}", server.props_slot_count)
        } else {
            "—/—".to_string()
        };
        // Per-server activity phase. Missing evidence stays `—`; it is never
        // rendered as a guessed `IDLE`.
        let phase = server.phase.map_or("—", ActivityPhase::label);
        // Per-server measurement freshness, mirroring the selected-server
        // panel: a connected server shows its (smoothed) sample interval, a
        // server that is not answering shows how old its last successful
        // measurement is, and a server that never answered shows `—`.
        let sample = if server.connected {
            match (server.sample_interval_ms, server.age_ms) {
                (Some(avg_ms), _) => format!("~{avg_ms:.0} ms avg"),
                (None, Some(age_ms)) => {
                    format!("{} ago", format_sample_age(Duration::from_millis(age_ms)))
                }
                (None, None) => "—".to_string(),
            }
        } else if let Some(age_ms) = server.age_ms {
            // A reconnecting server still shows its held CTX/tok/s values;
            // label them as stale rather than just aged.
            let age = format_sample_age(Duration::from_millis(age_ms));
            if server.reconnecting {
                format!("stale {age} ago")
            } else {
                format!("{age} ago")
            }
        } else {
            "—".to_string()
        };
        let label = fit_cell(&server.label, 16);

        // Line 1: identity, state, activity phase, and — when it fits — the
        // backend and endpoint. Whole cells that do not fit are dropped, so a
        // value is never sliced in half; only the trailing endpoint, which is
        // not a measured value, is trimmed with an explicit `…`.
        let mut header = vec![
            Span::styled(
                format!("{marker}{star} "),
                Style::default().fg(if index == highlight { CYAN } else { MUTED }),
            ),
            Span::styled(
                format!("{} ", pad_cell(&label, 16)),
                Style::default().fg(WHITE),
            ),
            Span::styled(format!("{state:<12} "), Style::default().fg(state_color)),
            Span::styled(
                format!("{phase:<12} "),
                Style::default().fg(selector_phase_color(server.phase)),
            ),
        ];
        let mut used = 3 + 17 + 13 + 13;
        if used + 11 <= inner_width {
            header.push(Span::styled(
                format!("{:<10} ", server.backend.label()),
                Style::default().fg(DIM_GREEN),
            ));
            used += 11;
        }
        if inner_width > used {
            header.push(Span::styled(
                fit_cell(&compact_endpoint(&server.endpoint), inner_width - used),
                Style::default().fg(DIM_GREEN),
            ));
        }
        lines.push(Line::from(header));

        // Line 2: slot counts, CTX, tok/s and measurement freshness. Same rule:
        // drop whole trailing cells rather than truncating a number. The slot
        // and CTX cells are kept first, tok/s next, the measurement age last.
        let mut detail = vec![Span::raw("   ")];
        let mut used = 3;
        if used + 7 <= inner_width {
            detail.push(Span::styled(
                format!("{:<6} ", format!("S {slots}")),
                Style::default().fg(MUTED),
            ));
            used += 7;
        }
        if used + 18 <= inner_width {
            detail.push(Span::styled(
                format!("CTX {ctx:<13} "),
                Style::default().fg(MUTED),
            ));
            used += 18;
        }
        if used + 13 <= inner_width {
            detail.push(Span::styled(
                format!("{tps:<12} "),
                Style::default().fg(MUTED),
            ));
            used += 13;
        }
        if used + 1 + sample.chars().count() <= inner_width {
            detail.push(Span::styled(
                format!(" {sample}"),
                Style::default().fg(MUTED),
            ));
        }
        lines.push(Line::from(detail));
    }
    if servers.is_empty() {
        lines.push(Line::from(Span::styled(
            "no servers known",
            Style::default().fg(MUTED),
        )));
    }
    // Configured endpoints beyond the monitoring cap are shown explicitly so
    // they are never silently dropped. They are not polled; endpoints are
    // redacted (no credentials/query tokens). The block is pinned to the
    // bottom of the popup: it never scrolls out of view with the server list,
    // and when it is too tall to fit, at least its header (which names the
    // count) survives, so the unmonitored extras are always signalled.
    let mut overflow_lines: Vec<Line> = Vec::with_capacity(overflow_line_count(overflow));
    if !overflow.is_empty() {
        overflow_lines.push(Line::from(Span::styled(
            format!(
                "── NOT MONITORED · server limit reached ({}) ──",
                overflow.len()
            ),
            Style::default().fg(YELLOW),
        )));
        for spec in overflow {
            overflow_lines.push(Line::from(vec![
                Span::styled("  ✗ ", Style::default().fg(YELLOW)),
                Span::styled(
                    fit_cell(&compact_endpoint(&spec.endpoint), 40),
                    Style::default().fg(MUTED),
                ),
                Span::styled(
                    "  not monitored — limit reached",
                    Style::default().fg(MUTED),
                ),
            ]));
        }
    }

    let inner_h = inner.height as usize;
    let overflow_total = overflow_lines.len();
    // Give the server list at least one row when servers exist, and cap the
    // pinned block at half the popup so a long overflow list cannot starve the
    // interactive server list it sits under. Only an overflow-only popup may
    // spend its whole height on the block.
    let reserve_cap = if servers.is_empty() {
        inner_h
    } else {
        inner_h.saturating_sub(1).min((inner_h / 2).max(1))
    };
    let overflow_shown = overflow_total.min(reserve_cap);
    let viewport = inner_h - overflow_shown;

    if viewport > 0 {
        // Each server occupies two lines, so scroll in line units and keep both
        // lines of the highlighted entry inside the viewport. The scroll window
        // must span an even number of lines: a two-line entry then always lands
        // on an even pair boundary, so its header and detail stay together. An
        // odd viewport therefore computes the offset against `viewport - 1`;
        // the extra rendered row simply shows the next entry's header. Rounding
        // the offset down to an even line then never moves the window past the
        // highlighted entry's detail (which an odd window did, making the last
        // server's detail unreachable).
        let last_line = highlight * 2 + 1;
        let window = if viewport >= 2 {
            viewport - (viewport % 2)
        } else {
            viewport
        };
        let scroll = selector_scroll(last_line, lines.len(), window);
        let scroll = (scroll - (scroll % 2)) as u16;
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll, 0)),
            Rect::new(inner.x, inner.y, inner.width, viewport as u16),
        );
    }

    if overflow_shown > 0 {
        let hidden = overflow_total - overflow_shown;
        if hidden > 0 && overflow_shown >= 2 {
            // Replace the last visible row with an explicit continuation so a
            // clipped overflow block is never silently cut. The marker occupies
            // one of the `overflow_shown` rows, so one more entry is hidden
            // than the raw difference: hidden endpoints = hidden + 1.
            overflow_lines.truncate(overflow_shown);
            if let Some(last) = overflow_lines.last_mut() {
                *last = Line::from(Span::styled(
                    format!("  … {} more not monitored", hidden + 1),
                    Style::default().fg(YELLOW),
                ));
            }
        } else {
            // `overflow_shown == 1` keeps the header, which already names the
            // total count of unmonitored endpoints.
            overflow_lines.truncate(overflow_shown);
        }
        frame.render_widget(
            Paragraph::new(overflow_lines),
            Rect::new(
                inner.x,
                inner.y + viewport as u16,
                inner.width,
                overflow_shown as u16,
            ),
        );
    }
}

pub(super) fn draw_settings_popup(frame: &mut Frame, area: Rect, state: &UiState) {
    let width = area.width.saturating_sub(6).min(74);
    let max_height = area.height.saturating_sub(4);
    if width < 54 || max_height < 15 {
        draw_settings_too_small(frame, area);
        return;
    }
    // Rows available inside the border.
    let inner_rows = usize::from(max_height - 2);
    let label_width = usize::from(width).saturating_sub(20);

    let field_line = |field: SettingsField, label: &str, value: String| {
        let selected = state.settings_field == field;
        let value_style = if selected {
            Style::default()
                .fg(BRIGHT_GREEN)
                .bg(PROCESS_SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(WHITE)
        };
        Line::from(vec![
            Span::styled(
                format!(" {label:<18}"),
                Style::default().fg(if selected { CYAN } else { MUTED }),
            ),
            Span::styled(format!(" {}", pad_cell(&value, 42)), value_style),
        ])
    };

    // Preview the endpoint that would actually be saved, so an `https` (or
    // custom-scheme) URL is not misrepresented as `http`. The preview is
    // display-only and goes through the redactor: userinfo, path, query and
    // fragment must never be painted (the saved value is left untouched). A
    // scheme-less legacy endpoint is shown in the compact `host:port` form
    // when it carries nothing confidential, otherwise as the placeholder.
    let primary = state.settings_endpoint().ok();
    let preview = primary.as_deref().map_or_else(
        || "…".to_string(),
        |endpoint| {
            let safe = crate::redact::safe_endpoint(endpoint);
            if safe == crate::redact::UNPARSEABLE_ENDPOINT {
                compact_endpoint(endpoint)
            } else {
                safe
            }
        },
    );
    let auto = if state.settings_auto_discovery {
        "ON"
    } else {
        "OFF"
    };

    let extras = state.settings_server_entries();
    let monitoring = state.settings_server_monitoring();
    let overflow_count = monitoring.overflow;
    // Endpoint labels shrink with the popup so the status column stays visible.
    let entry_width = usize::from(width).saturating_sub(2 + 3 + 15).min(40);
    let focused = state.settings_server_list_focused();

    // Every row carries a drop priority: 0 is always kept; when the terminal
    // is too short, rows are dropped from the highest priority down, so the
    // fields, the server window, the active draft and any error stay visible.
    const KEEP: u8 = 0;
    const PRIMARY_ROW: u8 = 1;
    const SPACER: u8 = 2;
    const FOOTER: u8 = 3;
    let mut head: Vec<(Line, u8)> = vec![
        (
            Line::from(Span::styled(
                " RUNTIME / CONNECTION",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            KEEP,
        ),
        (
            field_line(
                SettingsField::Host,
                "LLM Host/IP",
                fit_cell(&state.settings_host, 40),
            ),
            KEEP,
        ),
        (
            field_line(SettingsField::Port, "LLM Port", state.settings_port.clone()),
            KEEP,
        ),
        (
            field_line(SettingsField::Gpu, "GPU", state.settings_gpu.clone()),
            KEEP,
        ),
        (
            field_line(
                SettingsField::Refresh,
                "Refresh",
                format!("{} ms", state.settings_refresh_ms),
            ),
            KEEP,
        ),
        (
            field_line(
                SettingsField::ProcessRefresh,
                "Process refresh",
                format!("{} ms", state.settings_process_refresh_ms),
            ),
            KEEP,
        ),
        (
            field_line(
                SettingsField::OfflineGrace,
                "Offline grace",
                format!("{} ms", state.settings_offline_grace_ms),
            ),
            KEEP,
        ),
        (
            field_line(
                SettingsField::AutoDiscovery,
                "Auto discovery",
                auto.to_string(),
            ),
            KEEP,
        ),
        (Line::from(""), SPACER),
        (
            Line::from(vec![
                Span::styled(" Endpoint          ", Style::default().fg(MUTED)),
                Span::styled(fit_cell(&preview, label_width), Style::default().fg(CYAN)),
            ]),
            KEEP,
        ),
        (Line::from(""), SPACER),
        (
            Line::from(vec![
                Span::styled(
                    " SERVER LIST",
                    Style::default()
                        .fg(if focused { BRIGHT_GREEN } else { CYAN })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} additional · monitored {}/{}",
                        extras.len(),
                        monitoring.monitored,
                        crate::domain::MAX_MONITORED_SERVERS
                    ),
                    Style::default().fg(MUTED),
                ),
            ]),
            KEEP,
        ),
    ];
    // Primary endpoint row (read-only; edited by the host/port fields above).
    if primary.is_some() {
        head.push((
            Line::from(vec![
                Span::styled("   ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{:<entry_width$}", fit_cell(&preview, entry_width)),
                    Style::default().fg(WHITE),
                ),
                Span::styled("  primary", Style::default().fg(DIM_GREEN)),
            ]),
            PRIMARY_ROW,
        ));
    }

    let mut tail: Vec<(Line, u8)> = Vec::new();
    if overflow_count > 0 {
        tail.push((
            Line::from(Span::styled(
                format!(
                    " {overflow_count} beyond the {}-server limit: kept, not monitored",
                    crate::domain::MAX_MONITORED_SERVERS
                ),
                Style::default().fg(YELLOW),
            )),
            KEEP,
        ));
    }
    // The active draft (add / edit) is a text field. Secrets in it are masked
    // and a long draft shows its end, where the cursor is.
    if let Some(draft) = state.settings_server_draft() {
        let verb = if state.settings_server_edit_index.is_some() {
            " edit "
        } else {
            " add  "
        };
        let draft_width = usize::from(width).saturating_sub(10);
        tail.push((
            Line::from(vec![
                Span::styled(verb, Style::default().fg(ORK_GREEN)),
                Span::styled(
                    fit_tail(
                        &format!("{}_", crate::server_editor::mask_draft(draft)),
                        draft_width,
                    ),
                    Style::default()
                        .fg(BRIGHT_GREEN)
                        .bg(PROCESS_SELECTED_BG)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            KEEP,
        ));
        tail.push((
            Line::from(Span::styled(
                " Enter keep entry   Esc discard entry",
                Style::default().fg(MUTED),
            )),
            FOOTER,
        ));
    } else if focused {
        tail.push((
            Line::from(Span::styled(
                " a add   e edit   d delete   ↑↓ move   Enter save",
                Style::default().fg(MUTED),
            )),
            KEEP,
        ));
    }
    // One error row: a save failure wins over a list hint.
    if let Some(error) = state
        .settings_error
        .as_deref()
        .or(state.settings_server_error())
    {
        tail.push((
            Line::from(Span::styled(
                format!(" {error}"),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )),
            KEEP,
        ));
    }
    // The dialog-wide keys do not apply while a draft is typed (Enter and Esc
    // then act on the entry), so their footer is shown only outside a draft.
    if state.settings_server_draft().is_none() {
        tail.push((Line::from(""), FOOTER));
        tail.push((
            Line::from(Span::styled(
                " Tab / ↑↓ select   Type to edit   Space / ←→ toggle   Enter apply + save",
                Style::default().fg(MUTED),
            )),
            FOOTER,
        ));
        tail.push((
            Line::from(Span::styled(
                " Esc cancel   Auto discovery uses a detected local llama server first",
                Style::default().fg(MUTED),
            )),
            FOOTER,
        ));
    }

    // Server window: everything when it fits, otherwise at least a few rows
    // (droppable rows give way first) with an explicit "… more" marker.
    let total = extras.len();
    let others = head.len() + tail.len();
    let mandatory_others = head
        .iter()
        .chain(tail.iter())
        .filter(|(_, priority)| *priority == KEEP)
        .count();
    let mut window: Vec<(Line, u8)> = Vec::new();
    if total == 0 {
        window.push((
            Line::from(Span::styled(
                "   (none yet; press a here to add one)",
                Style::default().fg(MUTED),
            )),
            KEEP,
        ));
    } else {
        let budget = if total + others <= inner_rows {
            total
        } else {
            let generous = inner_rows.saturating_sub(others);
            let minimum = inner_rows
                .saturating_sub(mandatory_others)
                .min(total.min(4));
            // While a draft is open the draft line is the focus, so on the
            // smallest terminals the list may shrink to nothing for it.
            let floor = usize::from(state.settings_server_draft().is_none());
            generous.max(minimum).max(floor)
        };
        // A truncated window ends in a "… N more" row, unless only a single
        // row fits: then the highlighted entry wins and the header's entry
        // count tells that more exist.
        let more_row = total > budget && budget >= 2;
        let visible = if more_row {
            budget - 1
        } else {
            total.min(budget)
        };
        let highlight = state.settings_server_index.min(total - 1);
        let start = selector_scroll(highlight, total, visible);
        for (index, entry) in extras.iter().enumerate().skip(start).take(visible) {
            let selected = focused && index == highlight;
            let (status, status_color) = if monitoring.flags.get(index).copied().unwrap_or(false) {
                ("monitored", DIM_GREEN)
            } else {
                ("not monitored", YELLOW)
            };
            window.push((
                Line::from(vec![
                    Span::styled(
                        if selected { " > " } else { "   " },
                        Style::default().fg(if selected { CYAN } else { MUTED }),
                    ),
                    Span::styled(
                        format!(
                            "{:<entry_width$}",
                            fit_cell(&crate::server_editor::display_label(entry), entry_width)
                        ),
                        if selected {
                            Style::default()
                                .fg(BRIGHT_GREEN)
                                .bg(PROCESS_SELECTED_BG)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(WHITE)
                        },
                    ),
                    Span::styled(format!("  {status}"), Style::default().fg(status_color)),
                ]),
                KEEP,
            ));
        }
        if more_row {
            window.push((
                Line::from(Span::styled(
                    format!("   … {} more", total - visible),
                    Style::default().fg(MUTED),
                )),
                KEEP,
            ));
        }
    }

    let mut rows: Vec<(Line, u8)> = head;
    rows.extend(window);
    rows.extend(tail);
    for priority in [FOOTER, SPACER, PRIMARY_ROW] {
        while rows.len() > inner_rows {
            let Some(position) = rows.iter().rposition(|(_, p)| *p == priority) else {
                break;
            };
            rows.remove(position);
        }
    }
    let lines: Vec<Line> = rows.into_iter().map(|(line, _)| line).collect();

    let height = (lines.len() as u16 + 2).min(max_height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" SETTINGS · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Base URL of an endpoint, tolerating the legacy scheme-less form.
///
/// A value that parses with a host (`https://…`) is returned unchanged so
/// scheme, path, query and userinfo are preserved on save. A scheme-less
/// legacy endpoint (`host:port`, optionally `user:pass@host:port`) is parsed
/// as `http://…` so the dialog can still rewrite host and port while keeping
/// userinfo and path, instead of failing on a cannot-be-a-base URL. Returns
/// `None` when no host can be recovered, so the caller can fall back to the
/// historical `http://host:port` builder.
pub(super) fn parse_endpoint_base(server: &str) -> Option<reqwest::Url> {
    let trimmed = server.trim();
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        if url.has_host() {
            return Some(url);
        }
    }
    reqwest::Url::parse(&format!("http://{trimmed}"))
        .ok()
        .filter(reqwest::Url::has_host)
}

/// Like [`fit_cell`], but keeps the END of `text` (prefixing `…`), for a text
/// field whose cursor sits at the end.
fn fit_tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::from("…");
    out.extend(text.chars().skip(count - (width - 1)));
    out
}

pub(super) fn endpoint_parts(server: &str) -> (String, u16) {
    // Prefer the URL parser: it is scheme-aware (https without an explicit
    // port shows 443, not a made-up 8080) and keeps IPv6 hosts readable.
    if let Ok(url) = reqwest::Url::parse(server.trim()) {
        if let Some(host) = url.host_str() {
            return (
                host.to_string(),
                url.port_or_known_default().unwrap_or(8080),
            );
        }
    }

    let compact = server
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');

    // Legacy scheme-less `host:port` (optionally `user:pass@host:port`): parse
    // it as http so userinfo is not swallowed into the host field and the port
    // stays the explicit legacy default of 8080 (never the http default 80).
    if let Ok(url) = reqwest::Url::parse(&format!("http://{compact}")) {
        if let Some(host) = url.host_str() {
            return (host.to_string(), url.port().unwrap_or(8080));
        }
    }

    // The field is editable, so mask at the source: what the URL parser
    // could not handle must still never put userinfo, a path, a query or
    // a fragment into the host field.
    let compact = compact.rsplit_once('@').map_or(compact, |(_, host)| host);
    let compact = compact
        .find(['/', '?', '#'])
        .map_or(compact, |end| &compact[..end]);

    if let Some(rest) = compact.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(8080);
            return (host.to_string(), port);
        }
    }

    if let Some((host, port)) = compact.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (compact.to_string(), 8080)
}

pub(super) fn parse_setting_u64(
    value: &str,
    label: &str,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a number"))?;
    if !(min..=max).contains(&parsed) {
        return Err(format!("{label} must be between {min} and {max}"));
    }
    Ok(parsed)
}

/// The implicit port of a well-known URL scheme, used to decide whether a
/// port must be written out or can stay implicit in the saved endpoint.
pub(super) fn default_port_for_scheme(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

pub(super) fn build_endpoint(host: &str, port: &str) -> Result<String, String> {
    let host = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        return Err("Host / IP must not be empty".to_string());
    }
    if host.contains('/') || host.chars().any(char::is_whitespace) {
        return Err("Host / IP contains invalid characters".to_string());
    }
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "Port must be between 1 and 65535".to_string())?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("http://{host}:{port}"))
}
