# UX-Bewertung OrsikTop — EPIC 5.2 (CFA-39)

**Projekt:** exclude-barrier/OrsikTop
**Ticket:** CFA-39 (Eltern: CFA-27, EPIC 5 — TUI & Multi-Server UX, Aufgabe 5.2)
**Datum:** 11.10.2026
**Branch / Stand:** `feature/cfa27-integration` @ `ea109d6785557dd216381445d64c95cf57eec22b` (Version 0.3.2)
**Charakter:** reine Bewertung. **Keine automatische Umsetzung** der Vorschläge. Es wurden keine Produktverhaltensänderungen vorgenommen.

## 1. Auftrag

Bewertet werden sechs vorgeschlagene UX-Verbesserungen hinsichtlich Nutzen, Aufwand und Risiko; daraus werden **maximal drei** konkrete Empfehlungen für die nächste Entwicklungsrunde abgeleitet.

Kandidaten aus der Ticket-Beschreibung:

1. Direkte Bearbeitung zusätzlicher Server in Settings
2. Klarere Server-Detailansicht
3. Sichtbarer Hinweis auf veraltete Daten
4. Bessere Tastaturbedienung
5. Verständlichere Statusmeldungen
6. Kompakte Ansicht für kleine Terminals

## 2. Vorgehen und Evidenz

Die Bewertung stützt sich ausschließlich auf den tatsächlichen Quellcode und die vorhandenen deterministischen Tests (TestBackend), nicht auf Annahmen. Verwendete Quellen:

- `src/ui/dialogs.rs` — Hilfe-Popup, Server-Selektor, Settings-Popup
- `src/ui.rs` — Layout-Floors, Compact-Layout, Kopfzeile/Fußzeile, Footer-Status, `UiState::server_sample_age_ms` (`src/ui.rs:302`, `impl UiState` ab `src/ui.rs:248`)
- `src/ui/llm.rs` — Detailpanel des ausgewählten Servers, `friendly_llm_error`/`metrics_unavailable_reason`
- `src/ui/widgets.rs` — `format_sample_age` (Formatierung des Messalters)
- `src/app.rs` — Offline-Grace, Konfigurationszustand, Aufruf von `UiState::server_sample_age_ms` (`src/app.rs:652`)
- `src/config.rs` — Persistenz und Schema der Endpunkte (`server=`, `servers=`)
- `README.md` — dokumentierte Steuerung und Settings
- `src/ui/layout_regression_tests.rs` — bestehende TestBackend-Regressionstests (CFA-38)

Bereits abgeschlossene Multi-Server-/TUI-Arbeiten (CFA-22, CFA-23, CFA-38) wurden als gegeben vorausgesetzt und **nicht neu bewertet**.

## 3. Bewertung je Kandidat

Bewertungsmaßstab Nutzen: **hoch / mittel / niedrig**. Aufwand: **S** (≤ 1 Datei, geringe Logik), **M** (mehrere Funktionen/Tests), **L** (mehrere Module, neue Logik + Persistenz).

### 3.1 Direkte Bearbeitung zusätzlicher Server in Settings — Nutzen hoch, Aufwand L, Risiko mittel

Ist-Zustand: Das Settings-Popup (`draw_settings_popup`, `src/ui/dialogs.rs`) rendert genau sieben editierbare Felder: LLM Host/IP, LLM Port, GPU, Refresh, Process refresh, Offline grace, Auto discovery. Zusätzliche Endpunkte werden beim Öffnen zwar in `settings_servers` geladen (`src/ui.rs`, Felder `settings_servers`), aber **weder angezeigt noch editiert** und bleiben über einen Settings-Speichervorgang nur „verbatim" erhalten. Zusätzliche Server lassen sich heute nur von Hand in der Konfigurationsdatei (Schlüssel `servers=`) pflegen (siehe `README.md`, Abschnitt „Monitoring multiple servers"), anschließend ist ein Neustart nötig.

Nutzen: hoch. Multi-Server-Monitoring ist ein Kernfeature; ohne UI dafür ist die Konfiguration umständlich und fehleranfällig (Tippfehler, doppelte Endpunkte, Limit 16).

Aufwand: L. Es braucht ein interaktives Listenfeld (hinzufügen/ändern/entfernen), Validierung über die vorhandene `build_endpoint`-Logik, Normalisierung/Deduplizierung wie in `config.rs`, die Wechselwirkung mit dem 16er-Limit (Anzeige „NOT MONITORED — limit reached") und die Redaction von Zugangsdaten vor Anzeige.

Risiko: mittel. Fehlerhafte Normalisierung könnte Endpunkte zusammenlegen, die getrennt bleiben müssen (`127.0.0.1` vs. `localhost`), und Schreibfehler die gespeicherte Konfiguration beschädigen. Erfordert neue Tests inkl. Round-Trip über die Persistenz.

### 3.2 Klarere Server-Detailansicht — Nutzen mittel, Aufwand M, Risiko niedrig–mittel

Ist-Zustand: Der Server-Selektor (`draw_server_selector`, `src/ui/dialogs.rs`) zeigt pro Server zwei Zeilen mit Zustand, Aktivitätsphase, Backend, Endpunkt, Slot-Zählern, CTX, tok/s und Messalter. Die Zellen sind stark abgekürzt (`S`, `CTX`, `tok/s`, `avg`, `ago`) und es gibt keine Legende; im Hilfe-Popup werden nur Tasten, nicht die Datenfelder erklärt. Die Detailansicht des ausgewählten Servers liegt in `src/ui/llm.rs`.

Nutzen: mittel. Kürzere Einarbeitung, weniger Fehldeutungen (z. B. Slot-Feld vs. CTX-Feld). Der bestehende Informationsgehalt und die „Unbekannt ≠ Null"-Darstellung bleiben unverändert.

Aufwand: M. Vor allem Text-/Layoutarbeit plus Anpassung/Erweiterung der TestBackend-Erwartungen; die Spaltenbreitenlogik ist bereits vorhanden.

Risiko: niedrig–mittel. Layoutregression bei schmalen Terminals; die vorhandenen `layout_regression_tests` und CFA-38-Arbeiten dürfen nicht brechen.

### 3.3 Sichtbarer Hinweis auf veraltete Daten — Nutzen hoch, Aufwand S, Risiko niedrig

Ist-Zustand: Das Messalter wird bereits berechnet und angezeigt: `UiState::server_sample_age_ms` (`src/ui.rs:302`, aufgerufen aus `src/app.rs:652`) und im Selektor/Detailpanel als `"… ago"` (`format_sample_age`, `src/ui/widgets.rs:4`). Der Hinweis steht jedoch nur in gedämpfter Farbe (`MUTED`) und ohne Schwellwert-Hervorhebung; es gibt keinen expliziten „veraltet"-Zustand, obwohl „Offline grace" konfigurierbar ist. Das steht im Kontrast zum Projektprinzip „Abwesenheit darstellen, nicht Null" — vorhandene, aber alte Werte sollten erkennbar veraltet sein.

Nutzen: hoch. Direktes Vertrauen in die Messwerte: man sieht sofort, ob eine Zahl alt ist, statt sie für aktuell zu halten. Geringer Umfang, hoher Hebel.

Aufwand: S. Ein Schwellwert (z. B. auf Basis von `offline_grace_ms` oder einem Mehrfachen des Sample-Intervalls) plus Farbe/Präfix (`~`/`STALE`/gelb) in Selektor und Detailpanel; keine neue Datenquelle nötig.

Risiko: niedrig. Muss nur die bestehende „nie einen Nullwert erfinden"-Regel einhalten und die Farbzuordnung konsistent halten.

### 3.4 Bessere Tastaturbedienung — Nutzen mittel, Aufwand S–M, Risiko niedrig

Ist-Zustand: bereits gut ausgebaut: `↑/↓`, `j/k`, `PgUp/PgDn`, `Home/End`, `/`, `s`, `q`, `h`, `-/+`, `Esc`; dokumentiert im Hilfe-Popup (`draw_help_popup`) und in `README.md` (Abschnitt „Controls"). Lücken bestehen dort, wo es noch keine Funktion gibt (Server in Settings verwalten, 3.1) sowie bei konsistentem Verhalten von `Esc` (global beendet, in Overlays schließt es nur den Dialog — derzeit korrekt, aber nicht überall erklärt).

Nutzen: mittel. Verbesserungen sind nur punktuell (z. B. Taste zum Hinzufügen/Entfernen eines Servers, `?` als Hilfe-Alias).

Aufwand: S–M. Hängt stark an 3.1.

Risiko: niedrig.

### 3.5 Verständlichere Statusmeldungen — Nutzen mittel, Aufwand S, Risiko niedrig

Ist-Zustand: `footer_status` (`src/ui.rs`) zeigt bei Fehlern eine freundliche Ursache aus `friendly_llm_error` (`src/ui/llm.rs:800`) und `metrics_unavailable_reason` (`src/ui/llm.rs:789`) bzw. sonst `READY · MORE POWER, HAPPIER ORKS`. Bei zu kleinem Terminal erscheint `OrsikTop needs at least 54x16 terminal cells`; Settings-Fehler werden rot im Popup ausgegeben. Einige Meldungen sind knapp/scherzhaft und nennen den nächsten Handlungsschritt nicht immer.

Nutzen: mittel. Klarere Handlungsanweisung („Server 127.0.0.1:8081 offline — Verbindung prüfen") reduziert Supportfragen.

Aufwand: S. Reine Textarbeit an wenigen `format!`-Stellen; Nachweise über bestehende Render-Tests.

Risiko: niedrig. Achtung: bestehende Tests, die auf exakte Status-Strings prüfen, müssten angepasst werden.

### 3.6 Kompakte Ansicht für kleine Terminals — Nutzen niedrig, Aufwand S, Risiko niedrig

Ist-Zustand: bereits weitgehend umgesetzt. Harte Untergrenze 54×16 (`COMPACT_MIN_*`), volles Layout ab 72×22 (`FULL_MIN_*`), dazwischen `draw_compact`; Hilfe und Settings haben eigene Kompakt-/Zu-klein-Varianten (`draw_help_compact`, `draw_settings_too_small`). CFA-38 hat die CTX-Darstellung in schmalen Panels abgesichert.

Nutzen: niedrig. Der Bereich ist abgedeckt; verbleibende Wünsche (z. B. Legende im kompakten Layout) überschneiden sich mit 3.2.

Aufwand: S. Risiko: niedrig.

## 4. Priorisierung

Priorisierungsmaßstab, ausdrücklich: **erwarteter Gesamtnutzen zuerst**; erst bei gleichem Nutzengrad entscheiden der geringere Aufwand, danach das geringere Risiko. Die Nutzengrade sind innerhalb einer Stufe nicht durchweg identisch und werden deshalb in der Erläuterung benannt.

| Rang | Kandidat | Nutzen | Aufwand | Risiko |
| --- | --- | --- | --- | --- |
| 1 | Zusätzliche Server in Settings (3.1) | hoch (breiteste Wirkung) | L | mittel |
| 2 | Hinweis auf veraltete Daten (3.3) | hoch | S | niedrig |
| 3 | Klarere Server-Detailansicht (3.2) | mittel (Kernbedienpfad) | M | niedrig–mittel |
| 4 | Verständlichere Statusmeldungen (3.5) | mittel | S | niedrig |
| 5 | Bessere Tastaturbedienung (3.4) | mittel (an E1 gekoppelt) | S–M | niedrig |
| 6 | Kompakte Ansicht (3.6) | niedrig | S | niedrig |

Erläuterung: 3.1 steht trotz hohem Aufwand vorn, weil es den größten Gesamtnutzen liefert (Multi-Server-Konfiguration ohne Dateibearbeitung). 3.3 folgt mit hohem Nutzen bei kleinstem Aufwand und Risiko. Innerhalb der Stufe „mittel" liegt 3.2 vor 3.5 und 3.4, weil es denselben Kernbedienpfad wie 3.1 abrundet, während 3.5 nur einzelne Meldungen betrifft und 3.4 zusätzlich an E1 gekoppelt ist (für Server-Verwaltung existiert noch keine Tastaturbedienung). Bei gleich bewertetem Nutzen entscheidet der geringere Aufwand, was 3.5 vor 3.4 einordnet. 3.6 ist im Wesentlichen bereits erledigt (CFA-38).

## 5. Empfehlungen für die nächste Entwicklungsrunde (max. drei)

**E1 — Zusätzliche Server direkt in Settings verwalten (3.1, Nutzen hoch / Aufwand L / Risiko mittel).**
Liste der konfigurierten Zusatz-Endpunkte im Settings-Popup anzeigen und hinzufügen/ändern/entfernen; Validierung und Normalisierung über die vorhandene Endpunkt-Logik; Wechselwirkung mit dem 16er-Limit sichtbar machen; Zugangsdaten vor Anzeige redigieren. Erwarteter Nutzergewinn: Multi-Server ohne Handarbeit an der Konfigurationsdatei konfigurierbar. Ob danach weiterhin ein Neustart nötig ist, hängt davon ab, ob die überwachte Servermenge zur Laufzeit neu aufgebaut wird; das ist vor der Umsetzung von E1 zu klären (siehe Abschnitt 7). Nachweis: TestBackend-Tests (Feldeditierung, Limit) plus Round-Trip-Test der Persistenz.

**E2 — Veraltete Messwerte sichtbar hervorheben (3.3, Nutzen hoch / Aufwand S / Risiko niedrig).**
In Selektor und Detailpanel ab einem Schwellwert (Offline grace bzw. Mehrfaches des Sample-Intervalls) das Messalter farblich und/oder als Präfix markieren, ohne je einen Wert zu erfinden. Erwarteter Nutzergewinn: alte Zahlen sind sofort als alt erkennbar; stützt das Projektprinzip „Abwesenheit statt Null". Nachweis: neue TestBackend-Tests für frisch vs. veraltet vs. offline (Grundlage ist das Kindticket CFA-58).

**E3 — Server-Detailansicht verständlicher machen (3.2, Nutzen mittel / Aufwand M / Risiko niedrig–mittel).**
Abkürzungen (`S`, `CTX`, `tok/s`, `avg`, `ago`) auflösen bzw. eine Legende (Hilfe-Popup) ergänzen; Spaltenbreitenlogik und „Unbekannt"—Darstellung unverändert lassen. Erwarteter Nutzergewinn: geringere Fehldeutung der dicht gepackten Felder. Nachweis: bestehende `layout_regression_tests` grün plus neue Erwartungen für die Texte.

## 6. Nicht empfohlen in dieser Runde

- 3.4 (Tastatur) und 3.5 (Statusmeldungen) sind sinnvoll, aber kleiner Hebel; 3.4 setzt auf E1 auf und wird sinnvollerweise mit E1 gebündelt.
- 3.6 (kompakte Ansicht) ist im Wesentlichen bereits erledigt (CFA-38) und wird nicht erneut aufgegriffen.

## 7. Restrisiken und offene Punkte

- Aufwandsschätzungen (S/M/L) sind relative Größenordnungen, keine Personentage; E1 ist der einzige Kandidat mit Persistenz- und Migrationsnähe und verdient vor der Umsetzung einen eigenen Entwurf.
- Für E1 ist zusätzlich zu klären, ob die überwachte Servermenge nach einem Settings-Speichern zur Laufzeit neu aufgebaut wird. Falls nicht, bleibt ein Neustart nötig; der erwartete Nutzergewinn und die Aufwandschätzung von E1 sind dann entsprechend einzuordnen.
- Der Schwellwert für E2 muss fachlich festgelegt werden (Kopplung an `offline_grace_ms` vs. festes Mehrfaches); diese Entscheidung ist bewusst der nächsten Runde überlassen.
- Diese Bewertung ändert keinen Produktcode. Alle Aussagen sind aus dem o. g. Stand ableitbar; tatsächliche Vorher-/Nachher-Nachweise liefert erst die jeweilige Umsetzung.
