# Telemetrie — systematische Verifikation gegen HEAD

**Ticket:** CFA-33 (EPIC 2.1, CFA-27)
**Verifizierter Stand:** Branch `feature/cfa27-integration`, HEAD `2e0d37f`
**Testnachweis:** `pc-project cargo test --all-targets --locked` auf `cf-desktop`,
`468 passed / 0 failed / 3 ignored`, Exitcode 0, `source_unchanged: true`.
Belegt durch zwei Läufe am gleichen Quellstand:

- Implementierungslauf: Snapshot-Hash
  `ba432f5ccc0b6e963f4a2a8339a0b19824742c062afae153a544f05a5521b097`,
  Artifacts `/home/chris/.local/share/paperclip-worker/runs/c03db2693c8d49f4b66b28e713e26afa`
  (vor Fertigstellung dieses Dokuments ausgeführt).
- Unabhängiger Review-Lauf (Claudio/Opus 5.5): Snapshot-Hash
  `6eb28d2d687c574bc6e0528f5ad86c8584551a425597379c9e551442a5c6fbba`,
  Artifacts `/home/chris/.local/share/paperclip-worker/runs/2b423a7cef93491b849c17d09144af0d`.

## Zweck und Methode

Dieses Dokument verifiziert, wie jede in der LLM-Panel-Telemetrie angezeigte
Metrik **tatsächlich** aus den Rohdaten entsteht, und trennt echte Nullwerte,
unbekannte Werte und veraltete Werte sauber. Es ist **lesende** Evidenz: es
ändert keine Produktionslogik und insbesondere **nichts an `src/strata.rs`**.

Quellen sind der Programmcode (Zeilennummern unten) und die Unit-/Fixture-Tests
am oben genannten HEAD. **Alle Zeilenverweise beziehen sich auf den committeten
Stand `2e0d37f`.** Der gemeinsame Workspace kann uncommittete Änderungen anderer
Tickets enthalten (zum Zeitpunkt dieses Laufs u. a. CFA-34: zusätzliche
`mod tests`-Blöcke in `src/llama.rs` und neue Fixtures), die Zeilennummern
verschieben, ohne die hier zitierten Produktionsstellen zu verändern. Für
CFA-33 geändert ist ausschließlich `docs/TELEMETRY_VERIFICATION.md`. Es wurde
kein laufender Server kontaktiert und keine Inferenz ausgelöst; die
Anzeige-Semantik ist durch Fixtures und In-Process-Fake-Server abgedeckt. Wo
keine Live-Evidenz existiert, wird sie nicht behauptet (vgl.
`docs/LIVE_VS_FIXTURE.md`).

Datenfluss: `/metrics` + `/slots` + `/props` → `LlmStats` (`src/llama.rs:55`) →
`ServerSummary` (`src/domain.rs:855`) → UI (`src/ui/llm.rs:4`). Die Anzeige liest
fast ausschließlich `Option`-Felder; `None` rendert als `—`, nie als
fabrizierte `0`. Ausnahmen sind `context_size`, `slot_count` und `busy_slots`
(`u64` mit `0`-Sentinel) sowie über `slots_available` gegatete Zähler — für
diese ersetzt das `slots_available`-Gate die `Option`-Semantik.

## Zentrales Ergebnis (Acceptance Criteria)

Rein lesende Verifikation. Ergebnis differenziert:

- **Kriterium „Keine semantisch irreführende Anzeige nachgewiesen” ist NICHT
  vollständig erfüllt.** Zwei konkrete Anzeigen sind bei bestimmten (durch
  Fixtures belegbaren, aber nicht live bestätigten) Eingaben semantisch
  irreführend; sie werden unten als offene Befunde **F1** und **F2** geführt
  und gehören in Folgetickets, nicht in CFA-33:
  - **F1 — CACHE-Quote** wird auf 100 % geklemmt und als „% of PP“ ausgegeben,
    obwohl der Zähler keine Teilmenge belegen muss (`src/ui/llm.rs:165-170,324`).
  - **F2 — REQUEST prompt** zeigt im Fallback ohne `n_prompt_tokens_processed`
    die **KV-Belegung** des Slots, nicht den Prompt-Fortschritt
    (`src/llama/slots.rs:75-78`; belegt durch `slots_strata_new.json`).
- **CTX-Kernverhalten korrekt.** Die CTX-Zeile zeigt ausschließlich die aktuelle
  Slot-Occupancy; der High-Water-Mark wird **nirgends** als aktuelle Belegung
  substituiert und erscheint nur explizit als `watermark` in `orsiktop diag`.
  Belegt durch `watermark_is_not_shown_as_current_context_occupancy`
  (`src/ui.rs:4110`) und `missing_high_water_mark_is_not_rendered_as_zero`
  (`src/diagnostics.rs:885`).
- **Echte Nullwerte und unbekannte Werte korrekt getrennt** (für CTX und
  Zähler). Fehlend → `—`, gemeldetes `0` bleibt `0`. Belegt u. a. durch
  `unknown_context_occupancy_renders_the_ctx_meter_unavailable`
  (`src/ui.rs:4054`), `known_zero_context_occupancy_still_renders_zero_percent`
  (`src/ui.rs:4074`), `absent_metrics_render_as_unavailable_not_zero`
  (`src/ui.rs:4137`), `present_zero_prompt_tokens_is_a_real_zero_not_the_progress`
  (`src/llama.rs:2178`), `explicit_zero_slot_counter_is_a_real_zero`
  (`src/llama.rs:2501`). Einschränkung: siehe **F3** (STATE kann ohne
  Slot-Evidenz IDLE zeigen).
- **Behauptete Korrekturen durch Tests belegt.** Der High-Water-Fix `c83230f`
  ist am HEAD wirksam und durch Regressionstests abgesichert (siehe Abschnitt
  „High-Water-Fallback“). Für die Stale-Wert-Behandlung gilt das nur
  eingeschränkt (siehe **F4**).
- **Keine Änderung an Strata.** `src/strata.rs` unverändert (protected hash).

## Offene Befunde (Folgetickets, nicht in CFA-33)

### F1 — CACHE-Quote wird auf 100 % geklemmt (MUST)

- **Ort:** `src/ui/llm.rs:165-170` (`(cached / total * 100.0).clamp(0.0, 100.0)`),
  Ausgabe `{value:.1}% of PP` (`src/ui/llm.rs:324`).
- **Beleg:** `tests/fixtures/metrics_current.prom` enthält
  `prompt_tokens_cached_total 26000` und `prompt_tokens_total 12000`
  (→ 216,7 %, geklemmt auf `100.0% of PP`). Kein Test deckt die Quote ab.
- **Ist/Soll:** Angezeigt wird ein 100-%-Anteil, den die Zähler nicht hergeben
  (cached ist hier keine belegte Teilmenge von prompt_total). Korrekt wäre
  kein Anteilswert oder `—`.
- **Einschränkung:** Die Fixture ist synthetisch; ob reales llama.cpp
  `cached > total` liefert, ist nicht live belegt. Die Fehlanzeige für diese
  Eingabe ist aber aus dem Code eindeutig.
- **Folgeticket-Vorschlag:** „Telemetrie: CACHE-Quote (`% of PP`) korrekt
  berechnen/klemmen“. Scope: `src/ui/llm.rs` cache_share + Ausgabe;
  Regressionstest mit `metrics_current.prom`. AC: kein geklemmter 100-%-Anteil;
  Quote nur bei belegbarer Teilmengen-Semantik oder `—`.

### F2 — REQUEST prompt fällt auf die KV-Belegung zurück (MUST)

- **Ort:** `src/llama/slots.rs:75-78`
  (`n_prompt_tokens_processed` `.or(prompt_tokens)`); der Strata-Lückenfüller
  greift nur bei `None` (`src/llama.rs:607-609`).
- **Beleg:** `tests/fixtures/slots_strata_new.json` (busy Slot, nur
  `n_prompt_tokens: 113274`); `strata_new_slots_report_direct_occupancy`
  (`src/llama.rs:2228`) bestätigt `request_prompt_tokens == Some(113274)` — d. h.
  REQUEST PP ist identisch mit CTX used.
- **Ist/Soll:** Ohne `n_prompt_tokens_processed` (neueres Strata; llama.cpp,
  das den Feldnamen nicht führt) zeigt REQUEST PP die Slot-Belegung, obwohl die
  Zeile Prompt-Fortschritt bedeutet. Korrekt wäre `—` oder ein klar anderer
  Bezug.
- **Folgeticket-Vorschlag:** „Telemetrie: REQUEST prompt darf nicht
  stillschweigend die KV-Belegung anzeigen“. Scope: `src/llama/slots.rs`
  Fallback, `src/llama.rs:607-609`, UI-Label. AC: REQUEST PP nur bei echtem
  Prompt-Fortschritt, sonst `—`; Regressionstest mit `slots_strata_new.json`.

### F3 — STATE kann ohne Slot-Evidenz IDLE zeigen (SHOULD)

- **Ort:** `src/llama.rs:141-158` (`classify_phase`), `src/ui/llm.rs:573-586`.
  `classify_phase` liefert `Idle`, wenn alle Eingaben fehlen; `busy_slots` ist
  `u64` und bei nicht verfügbarem `/slots` `0`.
- **Beleg:** `llm_phase_marks_idle_without_activity` (`src/ui.rs:3226`) prüft
  genau `LlmStats::default()` → `IDLE`.
- **Einschränkung:** „nie geratenes IDLE“ gilt nur für offline/nie geantwortet
  (`activity_phase`, `src/llama.rs:165`). Ein **verbundener** Server mit
  `/metrics`, aber ohne `/slots`, ohne `requests_processing` (Strata hat keins,
  `src/strata.rs:121`) und ohne/ unbekanntem `live.state` (`src/strata.rs:246`)
  kann IDLE ohne Slot-Evidenz anzeigen. Auf Funktionsebene belegt, nicht live.
- **Empfehlung:** Einschränkung dokumentieren; ggf. Phase auf „—“ statt IDLE
  heben, wenn Slot-Evidenz fehlt (Folgeticket).

### F4 — Abschnitt „Veraltet“ war zu stark

- Während `offline_grace` wird die **komplette** letzte Probe weiter angezeigt,
  inklusive LIVE PP/TG, REQUEST und CTX (`src/app.rs:1438-1443`); das Panel
  verzweigt nur auf `!connected` (`src/ui/llm.rs:22`), die Kennzeichnung läuft
  nur über LINK/STATE `RECONNECTING`. Default 2500 ms, konfigurierbar bis
  60 000 ms (`src/config.rs:21,24`). Das ist **begrenzt/befristet**, nicht
  „verhindert“.
- Korrekter Belegtest: `transient_llm_failure_keeps_last_sample_and_marks_reconnecting`
  (`src/app.rs:1645`, prüft `held.prompt_tps == Some(123.0)`), **nicht**
  `build_server_summaries_derives_phase_age_and_backend`. Ein Test für den
  Ablauf der Grace-Frist existiert nicht.

## Verifikationstabelle

Aktualisierungsfrequenz: **1 s** ist der Standard-LLM-Poll (`refresh_ms`,
Default 1000 ms, geklemmt auf 100–10000 ms — `src/domain.rs:13-16`; die
LLM-Untergrenze ist 250 ms — `src/domain.rs:21`, angewandt in
`src/app.rs:1455`). **30 s** = `/props`- bzw. Local-Spec-Refresh
(`PROPS_REFRESH`/`SPEC_REFRESH`, `src/llama.rs:28,36`).

| Metrik | Quelldatenfeld | Berechnung | Aktualisierungsfrequenz | Bedeutung | Fehler-/Unavailable-Verhalten | Testnachweis |
| --- | --- | --- | --- | --- | --- | --- |
| **CTX used** (aktuell) | `/slots` → Slot `n_prompt_tokens` (`src/llama/slots.rs:74,107`) | Slot mit größter Belegung (busy bevorzugt), Wert verbatim | 1 s | Aktuell belegte KV-Tokens des angezeigten Slots | Slot meldet kein `n_prompt_tokens` → `None` → `— / capacity`; nie aus Progress-Countern abgeleitet; echtes `0` bleibt `0` | `derives_live_context_from_slots_fixture` (`src/llama.rs:1779`), `context_occupancy_is_not_derived_from_progress_counters` (`:1876`), `present_zero_prompt_tokens_is_a_real_zero_not_the_progress` (`:2178`), `unknown_context_occupancy_renders_the_ctx_meter_unavailable` (`src/ui.rs:4054`) |
| **CTX capacity** | `/props` `default_generation_settings.n_ctx` (`src/llama.rs:671`); Fallback `/slots` `n_ctx` (`src/llama/slots.rs:52,167`); Strata `engine.max_context` (`src/strata.rs:218`) | Erster nicht-null-Wert gewinnt; Slot-`n_ctx` verfeinert | 30 s (/props) bzw. 1 s (/slots/Strata) | Nenner der CTX-Anzeige | Kein Wert → `0` → CTX-Meter unavailable (nicht `0 %`) | `props_fixture_exposes_context_slots_and_model` (`src/llama.rs:1859`), `slot_missing_n_ctx_keeps_props_context_size` (`:2048`), `context_without_capacity_renders_the_meter_unavailable` (`src/ui.rs:4091`) |
| **CTX high-water (watermark)** | `/metrics` `llamacpp:n_tokens_max` (`src/llama.rs:527`) | Wert verbatim (Lebenszeit-Peak), `None` bei absent | 1 s | Historischer Spitzenwert — **nie** aktuelle Belegung | Absent oder /metrics aus → `None`; erscheint nur in `diag` als `watermark —`, nie als CTX | `watermark_is_not_shown_as_current_context_occupancy` (`src/ui.rs:4110`), `missing_high_water_mark_is_not_rendered_as_zero` (`src/diagnostics.rs:885`); High-Water-Fix `c83230f` |
| **CTX slot tag (`Sn`)** | `/slots` Slot `id` (`src/llama/slots.rs:55,163`) | `context_slot_id` des gewählten Slots | 1 s | Kennzeichnet den Slot der CTX-Zeile | Multi-Slot ohne `id` → kein Tag; Single-Slot → weggelassen | `context_slot_id_is_the_slot_api_id_not_the_array_position` (`src/llama.rs:1976`), `missing_slot_id_leaves_context_slot_id_unknown` (`:1991`), `llm_panel_without_slot_id_never_prints_a_fake_slot` (`src/ui.rs:3971`) |
| **REQUEST prompt** | `/slots` busy Slots `n_prompt_tokens_processed`, **Fallback** `n_prompt_tokens` (`src/llama/slots.rs:75-78`); Strata `live.prompt_tokens` nur bei fehlendem Slot-Wert (`src/llama.rs:607-609`) | Summe über busy Slots, nur wenn **jeder** busy Slot den Wert liefert | 1 s | Prompt-Fortschritt der laufenden Anfrage(n) | Ein busy Slot ohne Wert → Summe `None` → `— tok`; kein busy → `—`. **Siehe F2:** ohne `n_prompt_tokens_processed` (neueres Strata; llama.cpp ohne diesen Namen) ist der gezeigte Wert die **KV-Belegung**, nicht der Prompt-Fortschritt | `request_totals_require_all_busy_slots_to_report` (`src/llama.rs:2560`), `request_totals_sum_complete_busy_slots_and_ignore_idle_gaps` (`:2590`), `strata_new_slots_report_direct_occupancy` (`:2228`, belegt den Fallback), `incomplete_request_totals_render_unavailable_not_zero` (`src/ui.rs:4160`) |
| **REQUEST generated** | `/slots` busy Slots `next_token[0].n_decoded` (`src/llama/slots.rs:79,314`); Strata `live.generated` (`src/strata.rs:261`) | Summe über busy Slots, nur vollständig | 1 s | Generierte Tokens der laufenden Anfrage(n) | Wie REQUEST prompt; Strata liefert nur bei `state` reading/generating | `request_totals_*` (oben), `sample_keeps_strata_context_occupancy_unknown_but_tracks_progress` (`src/llama.rs:1592`) |
| **REQ active/deferred** | `/metrics` `requests_processing` / `requests_deferred` (`src/llama.rs:511,519`); Strata `live.queued` → `requests_deferred`, **kein** Active-Gauge (`src/strata.rs:121`) | Anzeige `active/deferred` | 1 s | Aktive bzw. wartende Anfragen | Nur wenn **beide** vorhanden → sonst `—`; bei Strata daher **immer** `—` (`src/ui/llm.rs:177-180`) | `absent_metrics_render_as_unavailable_not_zero` (`src/ui.rs:4137`) |
| **LIVE PP** | Slot-Counter-Delta (`src/llama/slots.rs:190`) sonst `/metrics` `prompt_total`-Delta über Sekunden (`src/llama.rs:729`); Strata: **kein** Instantwert | Δprompt / Δt; Slot-Delta bevorzugt (`choose_live_throughput`, `src/llama/slots.rs:294`) | 1 s | Momentane Prefill-Rate tok/s | Kein verwertbares Delta (erste Probe, Reset, Lücke, Counter fehlt) → `None` → `— tok/s`; Strata erzwingt `None` (kein Instant-Prefill) | `derives_live_throughput_from_matching_slot_task` (`src/llama.rs:1802`), `new_slot_task_does_not_create_false_live_spike` (`:1822`), `slot_delta_tps_counter_decrease_is_unavailable` (`:2426`), `reappearing_prompt_counter_does_not_spike` (`:2482`) |
| **LIVE TG** | Slot-Decode-Delta (`src/llama/slots.rs:190`) sonst `/metrics` `generated_total`-Delta (`src/llama.rs:734`); Strata `live.tok_s` nur bei `state=generating` (`src/strata.rs:248`) | Δgenerated / Δt bzw. Strata-Fensterrate | 1 s | Momentane Decode-Rate tok/s | Fehlend/Reset → `—`; Strata idle/reading → `None` (nie Rate der Vorgängeranfrage) | `slot_delta_tps_abstains_when_no_slot_reports_counters` (`src/llama.rs:2282`), `slot_delta_tps_abstains_when_a_slot_disappears_without_a_task_change` (`:2365`), `sample_reads_strata_json_metrics_end_to_end` (`:1406`) |
| **AVG (LIFE) PP** | `prompt_total` / `prompt_seconds_total` (`src/llama.rs:489`) | `average_over_seconds`, Fallback Legacy-Gauge `prompt_tokens_seconds` | 1 s | Lebenszeit-Durchschnitt Prefill tok/s | Nenner ≤ 0 oder Feld absent → `None` → `— tok/s` | `derives_average_throughput_from_time_counters` (`src/llama.rs:1730`), `maps_the_verified_strata_schema` (`src/strata.rs:439`) |
| **AVG (LIFE) TG** | `generated_total` / `generation_seconds_total` (`src/llama.rs:499`) | `average_over_seconds`, Fallback `predicted_tokens_seconds` | 1 s | Lebenszeit-Durchschnitt Decode tok/s | Wie AVG PP | `derives_average_throughput_from_time_counters` (`src/llama.rs:1730`), `maps_the_verified_strata_schema` (`src/strata.rs:439`) |
| **TOTAL PP/TG** | `prompt_total` / `generated_total` (`src/llama.rs:463,474`) | Monotone Counter verbatim | 1 s | Kumulierte Token seit Serverstart | Absent → `— tok`; Formatwechsel (llama.cpp↔Strata) verwirft die Delta-Basis | `parses_current_llama_metrics_fixture` (`src/llama.rs:1657`), `a_wire_format_switch_resets_the_counter_baseline` (`:1505`) |
| **CACHE** | `/metrics` `llamacpp:prompt_tokens_cached_total` (`src/llama.rs:472`) | Wert verbatim; Ratio = cached/prompt_total, nur bei `total>0`, **auf 0–100 % geklemmt** (`src/ui/llm.rs:165-170`) | 1 s | Zwischengespeicherte Prompt-Tokens (Lebenszeit) | llama.cpp ohne Feld **oder Strata** (nicht gemappt) → `None` → `—`, Ratio `—`. **Siehe F1:** der geklemmte `% of PP`-Anteil ist bei `cached > total` irreführend | `parses_current_llama_metrics_fixture` (`src/llama.rs:1657`); `maps_the_verified_strata_schema` belegt fehlendes Cache-Mapping (`src/strata.rs:439`); **kein** Test für die Quote |
| **SPEC drafts/accepted tokens** | `/metrics` `spec_decode_num_draft_tokens_total` / `_num_accepted_tokens_total` (`src/llama.rs:534,536`); Strata `drafts_offered`/`drafts_accepted` (`src/strata.rs:136,146`) | Counter verbatim | 1 s | Spekulative Draft-/Akzeptanz-Tokens (Lebenszeit) | Nur Draft vorhanden → Accepted `—`; beide absent bei `spec_enabled` → „no counters“ (`src/ui/llm.rs:542`) | `speculative_fixture_preserves_position_labels` (`src/llama.rs:1686`), `spec_row_hints_when_enabled_but_counters_unavailable` (`src/ui.rs:3267`), `spec_row_shows_values_when_any_counter_reported` (`src/ui.rs:3279`) |
| **SPEC Lebenszeit-Akzeptanz (SPEC TOK)** | `spec_draft_tokens` / `spec_accepted_tokens` (`src/ui/llm.rs:171-176`) | `accepted/draft * 100`, geklemmt 0–100 | 1 s | Kumulierte Akzeptanzrate | Draft `0` oder Feld absent → kein Prozent (`—`), nur `accepted`-Zahl | `spec_row_shows_values_when_any_counter_reported` (`src/ui.rs:3279`) |
| **SPEC acceptance % (ACC)** | Delta `spec_draft_tokens` & `spec_accepted_tokens` (`src/llama.rs:744-765`) | `accepted_delta / draft_delta * 100`, geklemmt 0–100 | 1 s, bei Ruhe max. 3 s gehalten (`SPEC_ACCEPTANCE_HOLD`) | Live-Akzeptanzrate der Spekulation | Kein identifiziertes Delta → letzten Wert max. 3 s halten, dann `—`; nie `0 %` erfunden | `spec_acceptance_computes_from_deltas_and_holds_after_quiet` (`src/llama.rs:2752`), `spec_acceptance_expires_after_the_hold_window` (`:2783`), `spec_counter_reset_does_not_create_fake_acceptance` (`:2833`) |
| **MTP/SPEC Kennzeichen** | `/props`+`/slots` speculative config (`src/llama.rs:686-713`); lokale CLI/env (`src/llama/speculative.rs`); `/metrics`-Counter als Enable-Hinweis (`src/llama.rs:540`) | Server-`/props` autoritativ; sonst lokale Prozess-Config | 30 s (/props + lokaler Scan) | `MTPn` / `SPEC` / `MTP OFF` | Kein Hinweis → `MTP OFF`; Server nennt Typ → autoritativ; reine Counter >0 → `SPEC` | `successful_props_response_replaces_spec_state_instead_of_accumulating` (`src/llama.rs:1186`), `props_without_spec_info_keeps_the_cached_state` (`:1228`), `reads_draft_mtp_type_from_custom_slot_shape` (`:2939`) |
| **Activity-Phase (STATE)** | Strata `live.state` (`src/strata.rs:236-247`) autoritativ; sonst Heuristik aus `generation_tps`/`prompt_tps`/`busy_slots`/`active_requests`/`deferred_requests` (`src/llama.rs:141-158`) | Erste zutreffende Regel: Generating > Prefill > Processing > Queued > Idle | 1 s | IDLE/PREFILL/GENERATING/PROCESSING/QUEUED/(RECONNECTING) | Offline/nie geantwortet → Phase `None` → `—` (nie geratenes IDLE); Reconnect explizit. **Siehe F3:** ein *verbundener* Server ohne Slot-Evidenz kann IDLE zeigen | `llm_phase_marks_idle_without_activity` (`src/ui.rs:3226`), `llm_phase_uses_a_reported_server_phase` (`:3234`), `build_server_summaries_derives_phase_age_and_backend` (`src/app.rs:2774`) |
| **SLOTS busy/total** | `/slots` `is_processing` / Array-Länge (`src/llama/slots.rs:26,61`) | Busy-Zähler / Slot-Anzahl | 1 s | Belegte Slots von Gesamt | `/slots` nicht verfügbar → `—/props_slots` oder `—`; nie `0/0` (Gate `slots_available`) | `idle_slots_fixture_reports_no_busy_slots` (`src/llama.rs:1842`), `server_selector_renders_unavailable_slots_without_a_fake_zero` (`src/ui.rs:2406`), `build_server_summaries_distinguishes_unavailable_slots_from_real_zero` (`src/app.rs:2688`) |
| **Slot-Übersicht (`S<n>`)** | `/slots` je Slot `id`,`n_prompt_tokens`,`n_ctx` (`src/llama/slots.rs:134`) | Aufsteigend nach `id`, Wert/Kapazität | 1 s | Belegung je Slot bei Multi-Slot | Slot ohne `id` wird ausgelassen; fehlende Werte `—` | `slot_overview_reports_each_slot_with_own_id_state_and_usage` (`src/llama.rs:2061`), `slot_overview_keeps_missing_values_unknown_and_drops_unidentified_slots` (`:2147`), `slot_overview_entries_never_print_a_fake_zero_for_missing_usage` (`src/ui.rs:4324`) |
| **Backend** | Klassifikation des geparsten `/metrics`-Bodys (`src/llama.rs:181`) | Strata-JSON-Objekt → Strata; Prometheus mit `llamacpp`-Namen → llama.cpp; sonst Unknown | 1 s | `llama.cpp` / `Strata` / `—` | Nie angenommen — nur positiv erkannt, sonst `—` | `recognizes_strata_objects_only` (`src/strata.rs:427`), `build_server_summaries_derives_phase_age_and_backend` (`src/app.rs:2774`) |
| **Messalter / Poll-Intervall** | Zeit seit letzter gültiger Probe; EMA des Intervalls (`src/domain.rs:869-879`) | `now - last_fresh`; EMA | 1 s | Aktualität der Anzeige | Nie gemessen → `—` (nicht `0`); online mit bekanntem Intervall → `~N ms avg` | `server_selector_shows_measurement_age_and_interval` (`src/ui.rs:2710`), `build_server_summaries_derives_phase_age_and_backend` (`src/app.rs:2774`) |

## Fehlende vs. reale Nullwerte vs. veraltete Werte

- **Fehlend → `—`.** Fast alle metrikabgeleiteten Anzeigefelder sind `Option`
  (Ausnahmen mit `0`-Sentinel bzw. `slots_available`-Gate: `context_size`,
  `slot_count`, `busy_slots`). `pick_metric_opt` (`src/llama/metrics.rs:43`)
  liefert `None`, wenn der Name fehlt; die UI rendert `None` als `—`
  (`src/ui/llm.rs:132-185`). Ein fehlender Counter wird **nicht** als
  Delta-Basis gespeichert (`src/llama.rs:774-781`), damit die Rückkehr des
  Counters keinen Nachhole-Spike erzeugt.
- **Reale `0`.** Ein gemeldetes `0` bleibt `0`: `n_prompt_tokens: 0` ist echte
  Slot-Belegung (`present_zero_prompt_tokens_is_a_real_zero_not_the_progress`,
  `src/llama.rs:2178`), `0 %`-CTX bei belegtem 0
  (`known_zero_context_occupancy_still_renders_zero_percent`, `src/ui.rs:4074`),
  `0`-Slot-Counter (`explicit_zero_slot_counter_is_a_real_zero`,
  `src/llama.rs:2501`).
- **Veraltet — begrenzt, nicht ausgeschlossen.** Mehrere Schranken begrenzen
  veraltete Anzeigen: während `offline_grace` (Default 2500 ms, konfigurierbar
  bis 60 000 ms — `src/config.rs:21,24`) hält `stabilize_llm_sample`
  (`src/app.rs:1419`) die **komplette** letzte Probe (inkl. LIVE PP/TG, REQUEST,
  CTX) und markiert nur LINK/STATE als RECONNECTING — die Werte selbst bleiben
  sichtbar; nach Ablauf wird die Probe verworfen. Belegt durch
  `transient_llm_failure_keeps_last_sample_and_marks_reconnecting`
  (`src/app.rs:1645`); für den **Ablauf** der Frist existiert kein Test.
  Weiter begrenzen die Strata-Decode-Rate (nur bei `state=generating`,
  `src/strata.rs:248`), die 3-s-Haltefrist der Spec-Akzeptanz
  (`src/llama.rs:29,766`), der Basis-Verwurf bei Formatwechsel/Strata-`since`-Reset
  (`src/llama.rs:547-562`) und die Slot-Identitätsprüfung
  (`src/llama/slots.rs:210-283`) die Veraltung. Für diese vier gilt: mit Tests
  belegt (`a_wire_format_switch_resets_the_counter_baseline`,
  `spec_acceptance_expires_after_the_hold_window`, `slot_delta_tps_*`).

## llama.cpp- vs. Strata-Versionsunterschiede

| Aspekt | llama.cpp (Prometheus) | Strata (JSON `/metrics`) |
| --- | --- | --- |
| Wire-Format | Text-Prometheus (`parse_prometheus`, `src/llama.rs:908`) | JSON-Objekt, erkannt über `totals` + Marker (`is_strata_metrics`, `src/strata.rs:162`) |
| CTX-Belegung | `/slots` `n_prompt_tokens` | Alt (0.1.40): kein `n_prompt_tokens` → `None`/`—`; Neu: direktes `n_prompt_tokens` (`src/llama.rs:2208,2228`) |
| CTX-Kapazität | `/props` `n_ctx` | Fallback `engine.max_context` (`src/strata.rs:218`) |
| REQUEST prompt | `n_prompt_tokens_processed`; führt der Server den Namen nicht, **Fallback auf `n_prompt_tokens`** (KV-Belegung — siehe F2, `src/llama/slots.rs:75-78`) | Slot-Wert, sonst `live.prompt_tokens`; **neueres Strata** (nur `n_prompt_tokens`) → REQUEST PP = KV-Belegung (F2) |
| LIVE TG | Counter-Delta bzw. Slot-Delta | `live.tok_s` (Fensterrate), nur bei `state=generating`; Counter-Totals sind completion-batched (`src/llama.rs:592`) |
| LIVE PP | Counter-Delta | nicht verfügbar (kein Instant-Prefill; `prefill_tok_s_mean` ist Mittel) → `—` (`src/llama.rs:599`) |
| Phase | Heuristik | `live.state` autoritativ (idle/reading/generating), normalisiert (`src/strata.rs:242`) |
| REQ active/deferred | `requests_processing`/`requests_deferred` | nur `live.queued` → deferred; **kein** Active-Gauge → REQ immer `—` (`src/strata.rs:121`, `src/ui/llm.rs:177-180`) |
| Cache-Zähler | `prompt_tokens_cached_total` | nicht gemappt → `—` |
| Spec-Counter | `spec_decode_num_*` | `drafts_offered`/`drafts_accepted` (`src/strata.rs:136,146`) |

Der Strata-Adapter mappt nur Felder, die das verifizierte Schema (Build
`0.1.40`) tatsächlich liefert; fehlende Felder bleiben absent, nie `0`
(`src/strata.rs:13-16`).

## High-Water-Fallback (Neuprüfung)

Der High-Water-Fallback wurde gemäß Aufgabenstellung **nur** auf tatsächliche
Fehlerhaftigkeit geprüft, nicht neu implementiert. Ergebnis: **nicht
fehlerhaft.** Der Fix `c83230f` ist am HEAD wirksam:

- `context_used` stammt ausschließlich aus dem direkten Slot-`n_prompt_tokens`
  (`src/llama/slots.rs:107`); `context_high_watermark` wird nur aus
  `llamacpp:n_tokens_max` befüllt (`src/llama.rs:527`) und **nie** in
  `context_used` substituiert (Kommentar `src/llama.rs:67-78`).
- Anzeige: CTX-Zeile nutzt nur `context_used` (`src/ui/llm.rs:61-105`); der
  Watermark erscheint nur in `diag` (`src/diagnostics.rs:320,348`).
- Regressionstests: `watermark_is_not_shown_as_current_context_occupancy`
  (`src/ui.rs:4110`) und `missing_high_water_mark_is_not_rendered_as_zero`
  (`src/diagnostics.rs:885`).

Keine weitere Bearbeitung erforderlich.

## Änderungen und Scope

Rein lesende Verifikation. Neu: dieses Dokument. Kein Produktionscode geändert,
`src/strata.rs` unverändert. Die Aussagen sind ausschließlich durch Code und
Tests am HEAD belegt; wo keine Live-Evidenz existiert, wird sie nicht behauptet
(vgl. `docs/LIVE_VS_FIXTURE.md`). Die Befunde F1 und F2 erfordern
Codekorrekturen und gehören in separate Folgetickets (Vorschläge oben), nicht in
CFA-33.

## Querverweise

- `docs/LIVE_VS_FIXTURE.md` — Live/Fixture/Unknown-Ledger je Funktionsbereich.
- `docs/HARDWARE_SUPPORT.md` — Hardware-Fähigkeiten und Evidenzstufen.
- `src/llama.rs`, `src/llama/slots.rs`, `src/llama/metrics.rs`, `src/strata.rs`,
  `src/ui/llm.rs`, `src/ui.rs`, `src/diagnostics.rs`, `src/app.rs`, `src/domain.rs`,
  `src/config.rs` — Quelle der obigen Zeilenverweise.
