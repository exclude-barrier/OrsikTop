//! CFA-58 (CFA-39-U1): TestBackend-Baseline des Ist-Zustands der
//! Multi-Server-Auswahl (`draw_server_selector`) für die Bewertungspunkte
//! 3.3 (Messalter / „veraltete Daten") und 3.1 (Anzeige des Server-Limits)
//! aus `docs/UX_EVALUATION.md`.
//!
//! Reine Testarbeit: Diese Tests ändern kein Produktverhalten und bewerten
//! keine UX — sie pinsen ausschließlich das *aktuell tatsächlich gerenderte*
//! Verhalten, damit die Ist-Zustandsaussagen der CFA-39-Bewertung durch
//! echte, deterministische Gates belegt sind.
//!
//! Abgedeckte Nachweise:
//! 1. Ein nicht antwortender/offline Server zeigt sein Messalter als
//!    „… ago" (Spaltenprüfung der Detailzeile), nie einen erfundenen
//!    Zahlenwert.
//! 2. Ein nie antwortender Server erfindet keine Aktualität: kein „0 ms",
//!    kein „S 0/0" — die Alters- und Slot-Zellen bleiben „—".
//! 3. Bei mehr als 16 konfigurierten Endpunkten zeigt der Selektor den
//!    Block „NOT MONITORED · server limit reached (N)" mit exakter Zählung,
//!    listet die nicht überwachten Endpunkte namentlich und der überwachte
//!    Teil (alle 16 Server) bleibt vollständig sichtbar.
//!
//! Bereits bestehende Abdeckung (wird referenziert, nicht dupliziert, alle
//! in `src/ui.rs mod tests`):
//! - `server_selector_shows_measurement_age_and_interval` (Zeile ~2713):
//!   „2.4 s ago" für einen stale Server, „—" für einen nie gemessenen Server.
//! - `server_selector_renders_unavailable_slots_without_a_fake_zero`
//!   (~2409): „S —/—" statt erfundenem „S 0/0" bei verbundenem,
//!   metrics-only Server.
//! - `server_selector_keeps_the_not_monitored_block_visible_when_scrolled`
//!   (~2506) und `server_selector_overflow_continuation_counts_missing_entries_exactly`
//!   (~2964): Overflow-Block bleibt beim Scrollen sichtbar; die
//!   Fortsetzungszählung „… N more not monitored" ist exakt.
//!
//! Deterministisch: reines `ratatui::backend::TestBackend`, keine
//! Netzzugriffe, keine echten Inferenzserver, keine Zeitabhängigkeit
//! (alle Alter werden als feste `age_ms`-Werte in die Fixture geschrieben).

use super::{draw_server_selector, ServerSummary};
use crate::domain::ServerSpec;
use ratatui::{backend::TestBackend, layout::Rect, Terminal};

/// Render den Selektor und liefert den Buffer zeilenweise, damit pro Server
/// die konkrete Detailzeile (Slots, CTX, tok/s, Messalter) geprüft werden
/// kann — nicht nur „irgendwo im Popup".
fn render_selector_rows(servers: &[ServerSummary], selected: &str, area: Rect) -> Vec<String> {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_server_selector(frame, area, servers, selected, 0, &[]))
        .unwrap();
    let width = area.width as usize;
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width)
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
        .collect()
}

/// Render den Selektor mit Overflow-Liste als zusammenhängenden Text.
fn render_selector_text(
    servers: &[ServerSummary],
    selected: &str,
    overflow: &[ServerSpec],
    area: Rect,
) -> String {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_server_selector(frame, area, servers, selected, 0, overflow))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

/// Fixture: Server mit fest vorgegebenem Messalter (`age_ms`), ohne
/// Intervall-EMA, damit deterministisch der Alterszweig
/// („nicht antwortend → Alter ago") getroffen wird.
fn offline_with_age(key: &str, age_ms: u64) -> ServerSummary {
    ServerSummary {
        key: key.to_string(),
        endpoint: format!("http://127.0.0.1:{}", 8100 + key.len()),
        label: key.to_string(),
        connected: false,
        age_ms: Some(age_ms),
        ..Default::default()
    }
}

/// Die Detailzeile des Selektoreintrags mit Label `label` (Zeile nach der
/// Kopfzeile, in der das Label an der Marker-/Label-Spalte steht).
fn detail_row<'a>(rows: &'a [String], label: &str) -> &'a str {
    let pos = rows
        .iter()
        .position(|row| {
            row.find(label).is_some_and(|byte| {
                let start = row[..byte].chars().count();
                let chars: Vec<char> = row.chars().collect();
                start >= 3
                    && matches!(chars[start - 3], '>' | ' ')
                    && matches!(chars[start - 2], '*' | ' ')
                    && chars[start - 1] == ' '
            })
        })
        .unwrap_or_else(|| panic!("kein Selektoreintrag für {label}: {rows:?}"));
    rows.get(pos + 1)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("Detailzeile zu {label} fehlt: {rows:?}"))
}

/// Der Inhalt einer Detailzeile ohne Popup-Border (`│`) und Padding, damit
/// eine Enden-Prüfung die tatsächlich letzte gedruckte Zelle trifft und
/// nicht die Rahmenlinie.
fn detail_tail(row: &str) -> String {
    row.trim_end().trim_end_matches('│').trim_end().to_string()
}

/// Nachweis 1: Ein offline Server (letzte erfolgreiche Messung bekannt,
/// antwortet gerade nicht) zeigt sein Messalter als „… ago" in der
/// Detailzeile — bei Sekunden- und Minutenformat — und nie einen
/// erfundenen Zahlenwert wie „0 ms".
#[test]
fn baseline_offline_server_shows_age_as_ago_never_a_fabricated_value() {
    let area = Rect::new(0, 0, 80, 20);
    let servers = vec![
        offline_with_age("offsec", 45_000), // → „45.0 s ago"
        offline_with_age("offmin", 75_000), // → „1m 15s ago"
    ];
    let rows = render_selector_rows(&servers, "offsec", area);

    let offsec_detail = detail_row(&rows, "offsec");
    assert!(
        offsec_detail.contains("45.0 s ago"),
        "offline-Server muss sein Messalter als '45.0 s ago' zeigen: {rows:?}"
    );
    assert!(
        detail_tail(offsec_detail).ends_with("ago"),
        "die Alterszelle muss mit 'ago' enden (kein erfundener Wert dahinter): {offsec_detail:?}"
    );

    let offmin_detail = detail_row(&rows, "offmin");
    assert!(
        offmin_detail.contains("1m 15s ago"),
        "Minutenformat des Messalters fehlt: {rows:?}"
    );
    assert!(
        detail_tail(offmin_detail).ends_with("ago"),
        "Alterszelle muss mit 'ago' enden: {offmin_detail:?}"
    );

    let text: String = rows.join("\n");
    assert!(
        !text.contains("0 ms"),
        "offline Server darf kein erfundenes '0 ms' zeigen: {text}"
    );
}

/// Nachweis 2: Ein nie antwortender Server (`age_ms = None`, kein `/slots`,
/// keine Telemetrie) erfindet keine Aktualität und keine Kapazität:
/// Alterszelle „—", Slots „—/—", kein „0 ms" und kein „S 0/0" anywhere.
/// (Die Phase-Zelle „—" statt geratenem „IDLE" pinnt bereits
/// `server_selector_shows_activity_phase_per_server` in `src/ui.rs`.)
#[test]
fn baseline_never_answering_server_fabricates_no_freshness_or_counts() {
    let area = Rect::new(0, 0, 80, 20);
    let never = ServerSummary {
        key: "never".to_string(),
        endpoint: "http://10.0.0.99:8080".to_string(),
        label: "never".to_string(),
        connected: false,
        ..Default::default()
    };
    let rows = render_selector_rows(&[never], "never", area);
    let detail = detail_row(&rows, "never");

    assert!(
        detail.contains("S —/—"),
        "unbekannte Slots müssen '—/—' bleiben, nicht '0/0': {rows:?}"
    );
    assert!(
        detail_tail(detail).ends_with('—'),
        "die Alterszelle eines nie antwortenden Servers muss '—' sein: {detail:?}"
    );

    let text: String = rows.join("\n");
    assert!(!text.contains("0 ms"), "erfundenes '0 ms': {text}");
    assert!(!text.contains("S 0/0"), "erfundene Slots 'S 0/0': {text}");
}

/// Nachweis 3: 16 überwachte Server + 4 konfigurierte, aber nicht
/// überwachte Endpunkte (`MAX_MONITORED_SERVERS = 16`): Der Selektor zeigt
/// den Block „NOT MONITORED · server limit reached (4)" mit exakter Zählung,
/// listet jeden nicht überwachten Endpunkt namentlich, und alle 16
/// überwachten Server bleiben vollständig sichtbar.
/// (Dass der Block beim Scrollen angepinnt bleibt und die
/// Fortsetzungszählung exakt ist, pinnt bereits
/// `server_selector_keeps_the_not_monitored_block_visible_when_scrolled`
/// bzw. `server_selector_overflow_continuation_counts_missing_entries_exactly`.)
#[test]
fn baseline_overflow_shows_server_limit_block_with_exact_count_and_keeps_monitored_list() {
    let servers: Vec<ServerSummary> = (0..16)
        .map(|i| ServerSummary {
            key: format!("s{i:02}"),
            endpoint: format!("http://127.0.0.1:{}", 8080 + i),
            label: format!("s{i:02}"),
            connected: true,
            slots_available: true,
            busy_slots: 1,
            slot_count: 4,
            ..Default::default()
        })
        .collect();
    let overflow: Vec<ServerSpec> = (11..15)
        .map(|i| ServerSpec {
            key: format!("o{i}"),
            endpoint: format!("http://10.0.0.{i}:8080"),
            identity: None,
        })
        .collect();

    // 80x44: Popup-Höhe 40, inner 38; Overflow-Block (5 Zeilen) ist
    // angepinnt, die Serverliste (32 Zeilen) passt vollständig in den
    // Viewport — der überwachte Teil bleibt also vollständig sichtbar.
    let area = Rect::new(0, 0, 80, 44);
    let text = render_selector_text(&servers, "s00", &overflow, area);

    assert!(
        text.contains("NOT MONITORED · server limit reached (4)"),
        "Overflow-Header mit exakter Zählung fehlt:\n{text}"
    );
    for i in 11..15 {
        let endpoint = format!("10.0.0.{i}:8080");
        assert!(
            text.contains(&endpoint),
            "nicht überwachter Endpunkt {endpoint} muss namentlich gelistet sein:\n{text}"
        );
    }
    assert_eq!(
        text.matches("not monitored — limit reached").count(),
        4,
        "jeder der 4 Overflow-Endpunkte muss als nicht überwacht markiert sein:\n{text}"
    );
    for i in 0..16 {
        let label = format!("s{i:02}");
        assert!(
            text.contains(&label),
            "überwachter Server {label} muss weiterhin sichtbar sein:\n{text}"
        );
    }
    assert!(
        text.contains("1/16"),
        "Positionsindikator muss die 16 überwachten Server ausweisen:\n{text}"
    );
}
