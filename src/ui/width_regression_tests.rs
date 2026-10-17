//! Regressionstests für die Anzeigebreiten-Korrektur aus CFA-90:
//! `display_width`/`fit_cell`/`pad_cell` (src/ui/widgets.rs) messen in
//! Terminal-Spalten statt Zeichen, damit breite (CJK) Zeichen, kombinierende
//! Akzente und schmale Fenster die festen Tabellenzellen nicht mehr
//! verschieben oder halbieren.
//!
//! Deterministisch: reine `TestBackend`- und reine String-Tests, keine
//! echten Server, keine Zeitabhängigkeit. An die bestehenden
//! CTX-Regressionstests (`layout_regression_tests.rs`) wird nur
//! angeschlossen (langer CJK-Modellname), nicht dupliziert.
//!
//! Eichung der Messbasis (test-desktop, rustc 1.100.0-nightly feaadeeac
//! 2026-09-19, ratatui 0.30.2 → ratatui-core 0.1.2 `Span::raw().width()` →
//! `UnicodeWidthStr::width`, unicode-width 0.2.0), gemessen per
//! eichendem Testlauf (Snapshot 4c7a72f5d4844e6eca269009b93655a71a74c3f6a56b92e43b34c6391aa6ac02):
//! `日本語`=6, `パ`=2, `日\u{0301}`=2, `e\u{0301}`=1, `e\u{0301}fg`=3,
//! `…`=1, `—`=1, `日本語ss`=8.
//! Semantik damit wie im CFA-90-Modell angenommen: breite Zeichen zählen 2,
//! kombinierende Zeichen zählen 0 (Träger + Akzent = 1 Spalte), die Ellipsis
//! zählt 1. Die Invariante von `fit_cell` ist `display_width(out) <= width`
//! (die Kante kann frei bleiben, wenn ein breites Zeichen nicht ganz passt),
//! nicht `== width`; exakte Breite erzeugt erst `pad_cell`.

use super::processes::{process_line_compact, process_line_wide};
use super::system::system_panel_title;
use super::widgets::{display_width, fit_cell, pad_cell, truncate_title};
use super::{draw_llm, GpuMapping, LlmStats, UiState};
use crate::cpu::CpuTopology;
use crate::domain::ProcessStats;
use ratatui::{backend::TestBackend, layout::Rect, text::Line, Terminal};

/// Anzeigebreite aller Spans einer Zeile in Terminal-Spalten.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| span.width()).sum()
}

/// Spaltenposition, an der der Span mit der ersten exakten Teilzeichenkette
/// `needle` in der Zeile beginnt (Summe der Anzeigebreiten der Vorspans).
fn span_column(line: &Line<'_>, needle: &str) -> usize {
    let mut col = 0usize;
    for span in &line.spans {
        if span.content.contains(needle) {
            return col;
        }
        col += span.width();
    }
    panic!(
        "Span mit {needle:?} nicht gefunden in: {}",
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("|")
    );
}

/// Zeile als reine Anzeigezeichenkette (ohne Stile), für Buffer-Vergleiche.
fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Renderter LLM-Panel als zeilenweiser Text-Snapshot (Borders inklusive),
/// wie in `layout_regression_tests` — hier für den langen CJK-Modellnamen.
fn render_llm_panel_rows(llm: &LlmStats, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_llm(frame, area, llm, &UiState::default(), &GpuMapping::None))
        .unwrap();
    let content = terminal.backend().buffer().content();
    (0..height)
        .map(|row| {
            content[(row as usize * width as usize)..((row + 1) as usize * width as usize)]
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect()
        })
        .collect()
}

/// Verbundener Server mit Metriken; Modellname frei wählbar, CTX-Paar fest.
fn llm_with_model(model: &str) -> LlmStats {
    LlmStats {
        connected: true,
        metrics_available: true,
        model: model.to_string(),
        context_size: 1_048_576,
        context_used: Some(78_800),
        slots_available: true,
        slot_count: 1,
        busy_slots: 1,
        ..Default::default()
    }
}

fn process(program: &str, command: &str) -> ProcessStats {
    ProcessStats {
        pid: 4711,
        program: program.to_string(),
        command: command.to_string(),
        cpu_pct: 12.5,
        memory_bytes: 268_435_456,
        threads: Some(7),
        start_time: 0,
        gpu: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 1. display_width: breite Zeichen zählen 2, kombinierende Zeichen 0
//    (Träger + Akzent zusammen = 1 Spalte)
// ---------------------------------------------------------------------------

#[test]
fn display_width_counts_cjk_as_two_and_combining_marks_as_zero() {
    assert_eq!(display_width("日本語"), 6);
    assert_eq!(display_width("パ"), 2);
    // e + U+0301 (kombinierender Akut): ein Anzeigeträger, ein Spalte.
    assert_eq!(display_width("e\u{0301}"), 1);
    assert_eq!(display_width("é"), 1);
    assert_eq!(display_width("München"), 7);
    // Breites Zeichen plus kombinierender Akzent auf dem breiten Träger.
    assert_eq!(display_width("日\u{0301}"), 2);
    assert_eq!(display_width(""), 0);
    assert_eq!(display_width("ascii"), 5);
}

// ---------------------------------------------------------------------------
// 2. fit_cell / pad_cell: Ergebnisanzeigebreite <= Zielbreite
//    (pad_cell: exakt == Zielbreite), kein halbiertes breites Zeichen,
//    Ellipsis nur bei echter Kürzung
// ---------------------------------------------------------------------------

#[test]
fn fit_cell_never_splits_a_wide_char_and_fits_the_target_width() {
    // Ziel 5: das dritte breite Zeichen (Spalten 5–6) würde die Kante
    // überschneiden und wird ganz verworfen statt halbiert.
    let out = fit_cell("日本語", 5);
    assert_eq!(out, "日本…");
    assert_eq!(display_width(&out), 5);

    // Ziel 4: ein breites Zeichen passt, das zweite nicht → "日…" (Breite 3,
    // Kante bleibt frei, weil das … an Spalte 3 sitzt und 語 (Spalten 3–4)
    // sonst halbiert würde).
    let out = fit_cell("日本語", 4);
    assert_eq!(out, "日…");
    assert_eq!(display_width(&out), 3); // <= 4, Kante frei gelassen

    // Ziel 1: nur die Ellipsis.
    assert_eq!(fit_cell("日本語", 1), "…");
    assert_eq!(fit_cell("日本語", 0), "");

    // Breit + ASCII-Mix: `日本語ss` misst exakt 8 → passt ohne Kürzung.
    assert_eq!(display_width("日本語ss"), 8);
    assert_eq!(fit_cell("日本語ss", 8), "日本語ss");
    // Ziel 7: Budget 6 → 日本語 passt ganz (6 Spalten); das erste `s` würde
    // die Kante überschreiten und entfällt komplett → "日本語…" (Breite 7).
    let out = fit_cell("日本語ss", 7);
    assert_eq!(out, "日本語…");
    // <= 7, kein halbiertes Zeichen
    assert_eq!(display_width(&out), 7);

    // Ziel 6: Budget 5 → 日本 (4); das dritte CJK (Spalten 5–6) würde die
    // Ellipsis-Kante halbieren und entfällt. Der Loop bricht am ersten
    // nicht passenden Zeichen — die nachfolgenden ASCII-`s` werden nicht
    // nachgezogen → "日本…" (Breite 5, Kante bleibt frei).
    let out = fit_cell("日本語ss", 6);
    assert_eq!(out, "日本…");
    assert_eq!(display_width(&out), 5); // <= 6, kein halbiertes 語
}

#[test]
fn fit_cell_appends_ellipsis_only_when_truncating() {
    // Exakt passend: keine Ellipsis, Text unverändert.
    assert_eq!(fit_cell("日本語", 6), "日本語");
    assert_eq!(fit_cell("short", 16), "short");
    assert_eq!(fit_cell("", 4), "");
    // Kürzung: genau ein … am Ende.
    let out = fit_cell("sehr langer Programmname", 10);
    assert_eq!(display_width(&out), 10);
    assert!(out.ends_with('…'));
    assert_eq!(out.chars().filter(|c| *c == '…').count(), 1);
}

#[test]
fn pad_cell_result_has_exactly_the_target_display_width() {
    for (text, width) in [
        ("日本語", 16usize),
        ("python3", 16),
        ("e\u{0301}gtab", 8),
        ("日", 3),
        ("", 5),
        ("ascii", 5), // exakt → kein Padding
    ] {
        let out = pad_cell(text, width);
        assert_eq!(
            display_width(&out),
            width,
            "pad_cell({text:?}, {width}) ergibt {out:?} mit Breite {}",
            display_width(&out),
        );
    }
    // CJK-Label und ASCII-Label belegen nach pad_cell exakt dieselbe Breite
    // (Serverauswahl/Settings: die Folge-Spalten rücken nicht).
    assert_eq!(
        display_width(&pad_cell("日本語サーバー", 16)),
        display_width(&pad_cell("orsik-llama", 16)),
    );
}

#[test]
fn pad_cell_keeps_combining_marks_attached_and_pads_by_columns() {
    // "e" + U+0301 bleibt eine Spalte; das Padding zählt Spalten, nicht Chars.
    let out = pad_cell("e\u{0301}fg", 4);
    assert_eq!(out, "e\u{0301}fg ");
    assert_eq!(display_width(&out), 4);
    // Kürzung darf einen Akzent nicht von seinem Träger abtrennen:
    // Budget 1 → e+Akzent (1 Spalte) bleibt zusammen, danach …; Breite exakt 2.
    let out = fit_cell("e\u{0301}fg", 2);
    assert_eq!(out, "e\u{0301}…");
    assert_eq!(display_width(&out), 2);
}

#[test]
fn truncate_title_uses_display_width_not_char_count() {
    // 8 CJK-Zeichen = 16 Spalten > 10 → wird gekürzt (chars().count() == 8
    // wäre unter einer Zeichen-Grenze von 10 geblieben und nicht gekürzt).
    let out = truncate_title("日本語プログラム", 10);
    // Budget 9: 日本語 (6) + プ (2) = 8; das nächste CJK (Spalten 9–10)
    // würde die Ellipsis-Kante überschneiden → Kante bleibt bei 9 frei.
    assert_eq!(out, "日本語プ…");
    assert!(display_width(&out) <= 10);
    assert!(out.ends_with('…'));
    // Passender Titel bleibt unverändert.
    assert_eq!(truncate_title("日本語", 6), "日本語");
}

#[test]
fn system_panel_title_stays_within_panel_width_with_cjk_model() {
    let topology = CpuTopology {
        model: "日本語モデル".to_string(),
        logical_cpus: 16,
        physical_cores: Some(8),
        ..Default::default()
    };
    // Volle Titelbreite: " SYSTEM · 日本語モデル · 8C/16T " = 32 Spalten
    // (1 + 6 + 3 + 12 + 3 + 6 + 1; CJK zählt 2 je Zeichen).
    let wide = system_panel_title(&topology, 40);
    assert_eq!(wide, " SYSTEM · 日本語モデル · 8C/16T ");
    assert_eq!(display_width(&wide), 32);

    // Schmales Fenster (Panel 14 → max_len 12): truncate_title mit Budget 11
    // → " SYSTEM · …" (11 Spalten). Sogar 日 (Spalten 11–12) würde die
    // Ellipsis-Kante überschreiten und entfällt komplett; die Kante bleibt
    // eine Spalte frei, weil kein breites Zeichen halbiert wird.
    let narrow = system_panel_title(&topology, 14);
    assert_eq!(narrow, " SYSTEM · …");
    assert_eq!(display_width(&narrow), 11); // <= 12, Kante wegen breitem 日 frei
    assert!(narrow.ends_with('…'));
    assert!(!narrow.contains("日本")); // beide CJK würden die Kante überschneiden
}

// ---------------------------------------------------------------------------
// 3. Prozesszeilen: CJK-Programmname verschiebt CPU/MEM/THR/GPU nicht
// ---------------------------------------------------------------------------

#[test]
fn process_line_wide_cjk_program_keeps_metric_columns() {
    // 9 CJK-Zeichen = 18 Spalten > 16-Spalten-Zelle → muss gekürzt werden.
    let cjk = process_line_wide(
        &process("日本語プログラム名", "-m llama"),
        100,
        false,
        false,
    );
    let ascii = process_line_wide(&process("python3", "-m http.server"), 100, false, false);

    // pid-Zelle (7) + Programmzelle (16, per pad_cell fix) + Commandzelle
    // (width - fixed = 100 - 53 = 47): CPU beginnt in beiden Zeilen an
    // Spalte 7 + 16 + 47 = 70, unabhängig davon, ob CJK oder ASCII.
    for line in [&cjk, &ascii] {
        assert_eq!(line.spans[0].width(), 7);
        assert_eq!(line.spans[1].width(), 16);
        assert_eq!(line.spans[2].width(), 47); // Commandzelle (pad_cell-fix)
        assert_eq!(span_column(line, "12.5"), 70); // CPU nach 7 + 16 + 47
    }
    // CJK-Programm (9 Zeichen = 18 Spalten) wird für die Ellipsis auf 15
    // Spalten Budget gekürzt: 7 ganze CJK-Zeichen (14 Spalten) + … = 15,
    // nie ein halbiertes Zeichen; pad_cell füllt danach auf exakt 16 Spalten
    // auf — die Zelle endet deshalb mit Leerzeichen hinter der Ellipsis.
    assert_eq!(line_text(&cjk).chars().next().unwrap(), ' ');
    let program_cell = &cjk.spans[1].content;
    assert!(program_cell.starts_with("日本語プログラ"));
    assert!(program_cell.trim_end().ends_with('…'));

    // CPU/MEM/THR/GPU beginnen in beiden Zeilen an derselben Spalte.
    assert_eq!(span_column(&cjk, "256M"), span_column(&ascii, "256M"));
    assert_eq!(span_column(&cjk, "7"), span_column(&ascii, "7"));
    assert_eq!(span_column(&cjk, "—"), span_column(&ascii, "—"));
    assert_eq!(line_width(&cjk), line_width(&ascii));

    // Komplette Zeile passt in die Tabellenbreite, Ellipsis nur im Programm.
    assert!(line_width(&cjk) <= 100);
    let text = line_text(&cjk);
    assert!(
        text.contains("日本語プログラ…"),
        "Ist-Ware der Zeile: {text:?} / Programmzelle: {:?} / Breite {}",
        cjk.spans[1].content,
        line_width(&cjk),
    );
    assert!(text.contains("-m llama"));
}

#[test]
fn process_line_compact_cjk_program_keeps_metric_columns() {
    let cjk = process_line_compact(&process("日本語プログラム", "-m llama"), 80, false, false);
    let ascii = process_line_compact(&process("python3", "-m http.server"), 80, false, false);

    // Compact: fixed = 7+7+9+6 → Programmzelle 51 Spalten; pid(7) davor.
    for line in [&cjk, &ascii] {
        assert_eq!(line.spans[0].width(), 7);
        assert_eq!(line.spans[1].width(), 51);
        assert_eq!(span_column(line, "12.5"), 58); // pid(7) + programm(51)
    }
    assert_eq!(span_column(&cjk, "256M"), span_column(&ascii, "256M"));
    assert_eq!(span_column(&cjk, "7"), span_column(&ascii, "7"));
    assert_eq!(line_width(&cjk), line_width(&ascii));
    assert!(line_width(&cjk) <= 80);
}

#[test]
fn process_line_wide_truncates_long_cjk_command_without_shifting_columns() {
    // Sehr langer CJK-Command: Zelle wird auf ihre Spaltenbreite gekürzt,
    // die Metrikspalten bleiben an derselben Position wie bei ASCII.
    let long = process_line_wide(
        &process("llama", "日本語の非常に長いコマンドライン引数の例"),
        100,
        false,
        false,
    );
    let ascii = process_line_wide(&process("llama", "plain"), 100, false, false);
    assert_eq!(span_column(&long, "12.5"), span_column(&ascii, "12.5"));
    assert_eq!(span_column(&long, "256M"), span_column(&ascii, "256M"));
    assert_eq!(line_width(&long), line_width(&ascii));
    // Die Command-Zelle ist in beiden Zeilen pad_cell-fix (47 Spalten);
    // der CJK-Command (42 Spalten) passt und wird nie über die Zelle hinaus
    // gezeichnet — die Metrikspalten bleiben dadurch verschoben-frei.
    assert_eq!(long.spans[2].width(), ascii.spans[2].width());
    assert!(line_text(&long).contains("日本語の非常に長いコマンドライン引数の例"));
}

// ---------------------------------------------------------------------------
// 4. Schmales Fenster + langer CJK-Modellname: CTX-Zahlen vollständig
//    (anknüpfend an layout_regression_tests, dort ohne CJK-Namen)
// ---------------------------------------------------------------------------

#[test]
fn narrow_panel_ctx_pair_stays_complete_with_long_cjk_model_name() {
    // Panel 46 → innere Breite 44 (wie in layout_regression_tests), aber mit
    // 24-spaltigem CJK-Modellnamen: die MODEL-Zeile darf die CTX-Zahlen
    // nicht aus dem Panel drängen.
    let llm = llm_with_model("日本語モデル名前テスト");
    let rows = render_llm_panel_rows(&llm, 46, 12);
    let text = rows.join("\n");
    assert!(
        text.contains("78,800 / 1,048,576 tok"),
        "die CTX-Zahlen müssen bei langem CJK-Namen vollständig bleiben, got:\n{text}"
    );
    let ctx = rows
        .iter()
        .filter(|row| row.contains("CTX"))
        .map(|row| {
            row.chars()
                .skip(1)
                .take(row.chars().count().saturating_sub(2))
                .collect::<String>()
        })
        .map(|inner| inner.trim_end().to_string())
        .next()
        .unwrap_or_else(|| panic!("keine CTX-Zeile gerendert:\n{text}"));
    assert!(
        ctx.ends_with("78,800 / 1,048,576 tok"),
        "die CTX-Zeile muss mit dem vollständigen Paar enden, got: {ctx:?}"
    );
    // Der Modellname selbst wird nicht halbiert: alle 12 CJK-Zeichen bleiben
    // im Panel sichtbar. (Der TestBackend-Buffer setzt breite Zeichen als
    // ein Symbol pro Zelle mit leerer Folgezelle, daher Vergleich ohne
    // Whitespace.)
    let joined_rows = rows.join("");
    let stripped: String = joined_rows.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        stripped.contains("日本語モデル名前テスト"),
        "CJK-Modellname muss vollständig sichtbar bleiben, got:\n{text}"
    );
}

#[test]
fn narrow_panel_cjk_model_name_is_never_split_across_rows() {
    // Noch schmaler (Panel 40 → inner 38) mit langem CJK-Namen: die CTX-Zeile
    // bleibt die letzte Kernzeile und das Paar vollständig. Degradationsregel
    // (wie layout_regression_tests::overflowing_ctx_row_drops_the_unit_before_any_digit):
    // erst die Einheit " tok" entfällt, dann kein einziger Digit — das Paar
    // endet vollständig am Panelrand.
    let llm = llm_with_model("日本語モデル名前テスト");
    let rows = render_llm_panel_rows(&llm, 40, 12);
    let text = rows.join("\n");
    let ctx = rows
        .iter()
        .find(|row| row.contains("CTX"))
        .unwrap_or_else(|| panic!("keine CTX-Zeile gerendert:\n{text}"));
    assert!(
        ctx.ends_with("78,800 / 1,048,576│"),
        "CTX-Paar muss auch bei Panel 40 vollständig bleiben, got: {ctx:?}"
    );
    assert!(
        !ctx.contains("tok"),
        "die Einheit darf vollständig entfallen, got: {ctx:?}"
    );
    // Kein CJK-Zeichen des Modellnamens wird durch eine Zeilengrenze
    // halbiert: die Modellzeile enthält nur ganze Zeichen (Buffer liefert
    // pro Zelle genau ein Symbol; ein halbiertes Zeichen wäre ein Lücken-
    // oder Ersatzzeichen).
    let model_row = rows
        .iter()
        .find(|row| row.contains("MODEL"))
        .expect("MODEL-Zeile fehlt");
    assert!(
        !model_row.contains('\u{FFFD}') && !model_row.contains('?'),
        "MODEL-Zeile zeigt Ersatzzeichen (halbiertes Zeichen?): {model_row:?}"
    );
    // Der vollständige Name bleibt im Panel erhalten (Buffer: ein Symbol pro
    // Zelle, Folgezelle leer → Vergleich ohne Whitespace).
    let stripped: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        stripped.contains("日本語モデル名前テスト"),
        "CJK-Modellname muss vollständig sichtbar bleiben, got:\n{text}"
    );
}
