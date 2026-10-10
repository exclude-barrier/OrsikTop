//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

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
            format!(" {text:<15}"),
            Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |text: &'static str| Span::styled(text, Style::default().fg(WHITE));

    let lines = vec![
        Line::from(Span::styled(
            " KEYBOARD",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("Esc"), desc("Quit")]),
        Line::from(vec![key("h"), desc("Toggle this help")]),
        Line::from(vec![key("q"), desc("Open settings")]),
        Line::from(vec![key("s"), desc("Server selector (multi-server)")]),
        Line::from(vec![key("/"), desc("Search / filter processes")]),
        Line::from(vec![
            key("- / +"),
            desc("Decrease / increase refresh interval"),
        ]),
        Line::from(vec![key("↑ / k"), desc("Select previous visible row")]),
        Line::from(vec![key("↓ / j"), desc("Select next visible row")]),
        Line::from(vec![key("PgUp / PgDn"), desc("Jump 10 processes")]),
        Line::from(vec![key("Home / End"), desc("First / last process")]),
        Line::from(""),
        Line::from(Span::styled(
            " MOUSE",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("Left click"), desc("Select process or group")]),
        Line::from(vec![key("Double click"), desc("Pin process or group")]),
        Line::from(vec![key("Right click"), desc("Expand / collapse group")]),
        Line::from(vec![key("Header click"), desc("Sort process table")]),
        Line::from(vec![key("Mouse wheel"), desc("Scroll process table")]),
        Line::from(vec![key("[-] / [+]"), desc("Change refresh interval")]),
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
            format!("{} ago", format_sample_age(Duration::from_millis(age_ms)))
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
            Span::styled(format!("{label:<16} "), Style::default().fg(WHITE)),
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
    let height = 18.min(area.height.saturating_sub(4));
    if width < 54 || height < 15 {
        draw_settings_too_small(frame, area);
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
        .title(" SETTINGS · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

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
            Span::styled(format!(" {value:<42}"), value_style),
        ])
    };

    // Preview the endpoint that would actually be saved, so an `https` (or
    // custom-scheme) URL is not misrepresented as `http`.
    let preview = state
        .settings_endpoint()
        .unwrap_or_else(|_| "…".to_string());
    let auto = if state.settings_auto_discovery {
        "ON"
    } else {
        "OFF"
    };

    let mut lines = vec![
        Line::from(Span::styled(
            " RUNTIME / CONNECTION",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        field_line(
            SettingsField::Host,
            "LLM Host/IP",
            fit_cell(&state.settings_host, 40),
        ),
        field_line(SettingsField::Port, "LLM Port", state.settings_port.clone()),
        field_line(SettingsField::Gpu, "GPU", state.settings_gpu.clone()),
        field_line(
            SettingsField::Refresh,
            "Refresh",
            format!("{} ms", state.settings_refresh_ms),
        ),
        field_line(
            SettingsField::ProcessRefresh,
            "Process refresh",
            format!("{} ms", state.settings_process_refresh_ms),
        ),
        field_line(
            SettingsField::OfflineGrace,
            "Offline grace",
            format!("{} ms", state.settings_offline_grace_ms),
        ),
        field_line(
            SettingsField::AutoDiscovery,
            "Auto discovery",
            auto.to_string(),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Endpoint          ", Style::default().fg(MUTED)),
            Span::styled(preview, Style::default().fg(CYAN)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Tab / ↑↓ select   Type to edit   Space / ←→ toggle   Enter apply + save",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            " Esc cancel   Auto discovery uses a detected local llama server first",
            Style::default().fg(MUTED),
        )),
    ];

    if let Some(error) = state.settings_error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
