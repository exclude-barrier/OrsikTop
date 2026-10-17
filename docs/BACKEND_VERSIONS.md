# Backend-Versionen — unterstützte und getestete Telemetrie-Varianten

**Ticket:** CFA-97 (Roadmap Phase 3, EPIC 5, HW-02)
**Basis:** OrsikTop **v0.4.0**, HEAD `e9600d6` (2026-10-14)
**Scope:** Dokumentiert wird, welche Backend-Formate und Schema-Varianten
OrsikTop unterstützt und welche davon belegt getestet sind. Es wird **kein**
Produktivcode geändert; insbesondere bleibt `src/strata.rs` unverändert
(geschützte Datei, Hash in `production-state.json`). Quelle der Wahrheit sind
Code und Fixtures am genannten HEAD — nicht die Herstellerprosa.

Zitierweise: Funktionsnamen sind der stabile Schlüssel; die `file:line`
verschieben sich mit dem Baum und sind nur Wegweiser. Jede Aussage ist in der
letzten Spalte auf Code oder Fixture zurückgeführt.

## Evidenzstufen

Wie in `docs/LIVE_VS_FIXTURE.md` / `docs/HARDWARE_SUPPORT.md`:

- **implementiert** — der Codepfad existiert am HEAD.
- **fixture-getestet** — durch Unit-Tests bzw. eingecheckte Fixtures
  (`tests/fixtures/*`) oder In-Process-Fake-Server in CI belegt; nie am echten
  Backend beobachtet.
- **live-verifiziert** — zusätzlich gegen ein echtes Backend gelaufen; die
  Version wird benannt.

## 1. Erkannte Backends

Es gibt genau zwei unterstützte Telemetrie-Backends; ein Backend wird **positiv
erkannt**, nie angenommen. Die Erkennung liest den tatsächlichen
`/metrics`-Antwortkörper.

| Backend | Wire-Format | Erkennung | Anzeige |
| --- | --- | --- | --- |
| llama.cpp | Prometheus-Text (`# TYPE` / `name value`) | Alles, was nicht als JSON beginnt, ist Prometheus; als llama.cpp gilt es nur, wenn mindestens ein Sample `llamacpp` im Namen trägt | `llama.cpp` |
| Strata | JSON-Objekt (`{…}`) | `is_strata_metrics`: `totals` ist ein Objekt **und** mindestens eine Markergruppe (`engine`, `live`, `requests_kept`, `hardware`, `hardware_static`, `conversation_cache`) ist vorhanden | `Strata` |
| — | JSON, aber kein Strata-Objekt / fremdes Prometheus | nicht erkannt | `—` (Unknown) |

Anker:

- Dispatch nach dem ersten Zeichen: `parse_metrics_body`
  (`src/llama/metrics.rs:12`). Nur ein Body, der mit `{`/`[` beginnt, geht in den
  JSON-Zweig; sobald `is_strata_metrics` zutrifft, wird er als Strata
  verarbeitet, sonst als JSON ohne Format (`format: None`, `json: true`).
- Backend-Klassifikation: `classify_backend` (`src/llama.rs:182`) — Strata nur
  bei erkanntem Strata-Objekt, llama.cpp nur bei einem Prometheus-Body mit
  `llamacpp`-Namen, sonst `Unknown`.
- Strata-Erkennung: `is_strata_metrics` (`src/strata.rs:162`).
- Die Strata-Obergruppen laut Modul-Doc: `engine`, `live`, `requests`,
  `conversation_cache`, `requests_kept`, `totals`, `hardware`,
  `hardware_static`, `history`, `time` (`src/strata.rs:5-6`).

Der `Backend`-Wert startet als `—` und kann nicht durch bloße Erreichbarkeit
entstehen; das ist in der Verifikationstabelle von
`docs/TELEMETRY_VERIFICATION.md` (Zeile „Backend") dokumentiert.

## 2. llama.cpp-Telemetrievarianten

llama.cpp liefert `/metrics` als Prometheus-Text. OrsikTop liest die
**aktuellen** Metriknamen und hält **ältere** Schreibweisen als
Kompatibilitäts-Fallback vor (`pick_metric_opt`, `src/llama/metrics.rs:45`;
Zuordnung `src/llama.rs:463-540`). Reihenfolge = Priorität: der erste
vorhandene Name gewinnt.

| Anzeigefeld | Bevorzugter Name | Akzeptierte Fallbacks |
| --- | --- | --- |
| TOTAL PP (`prompt_total`) | `llamacpp:prompt_tokens_total` | `llamacpp:tokens_evaluated_total`, `llamacpp_prompt_tokens_total`, `prompt_tokens_total` |
| TOTAL TG (`generated_total`) | `llamacpp:tokens_predicted_total` | `llamacpp_predicted_tokens_total`, `predicted_tokens_total` |
| AVG (LIFE) PP Nenner | `llamacpp:prompt_seconds_total` | — |
| AVG (LIFE) TG Nenner | `llamacpp:tokens_predicted_seconds_total` | — |
| AVG (LIFE) PP Gauge-Fallback | — | `llamacpp:prompt_tokens_seconds`, `llamacpp_prompt_tokens_seconds` |
| AVG (LIFE) TG Gauge-Fallback | — | `llamacpp:predicted_tokens_seconds`, `llamacpp_predicted_tokens_seconds` |
| CACHE | `llamacpp:prompt_tokens_cached_total` | — |
| REQ active | `llamacpp:requests_processing` | `llamacpp_requests_processing`, `requests_processing` |
| REQ deferred | `llamacpp:requests_deferred` | `llamacpp_requests_deferred`, `requests_deferred` |
| CTX high-water (watermark) | `llamacpp:n_tokens_max` | `llamacpp_n_tokens_max` |
| SPEC drafts (Anzahl) | `llamacpp:spec_decode_num_drafts_total` | — |
| SPEC drafts (Tokens) | `llamacpp:spec_decode_num_draft_tokens_total` | — |
| SPEC accepted (Tokens) | `llamacpp:spec_decode_num_accepted_tokens_total` | — |

Details:

- **Legacy-Gauges sind nur Fallback.** Die monotonen Zähler haben Vorrang; die
  alten Durchsatz-Gauges (`*_tokens_seconds`) dienen nur der Lebenszeit-AVG,
  wenn die Sekundenzähler fehlen (`src/llama.rs:487-511`). Test:
  `legacy_llama_metric_names_are_accepted_as_fallbacks` (`src/llama.rs:3178`).
- **Unbekannte/künftige Metriken bleiben erhalten** und stören die erkannten
  Namen nicht; der Prometheus-Parser verwirft nur leere Zeilen, Kommentare (`#`)
  und nicht-endliche Werte (`parse_prometheus`, `src/llama.rs:909`). Fixture
  `metrics_schema_unknown.prom`.
- **Doppelte/positionierte Serien** werden über Labels getrennt; `pick_metric_opt`
  verlangt `labels.is_none()`, damit eine gelabelte Serie einen ungelabelten
  Aggregatwert nicht ersetzt (`src/llama/metrics.rs:45-52`).

Fixtures für llama.cpp: `metrics_current.prom`, `metrics_legacy_names.prom`,
`metrics_partial.prom`, `metrics_schema_unknown.prom`, `metrics_speculative.prom`.
Zugehörige Tests: `parses_current_llama_metrics_fixture` (`src/llama.rs:1658`),
`speculative_fixture_preserves_position_labels` (`src/llama.rs:1687`),
`legacy_llama_metric_names_are_accepted_as_fallbacks` (`src/llama.rs:3178`).

## 3. Strata-JSON-Varianten

Strata antwortet auf `/metrics` mit HTTP 200 und einem JSON-Objekt statt
Prometheus-Text (`src/strata.rs:1-21`). Der Adapter normalisiert die kumulativen
Zähler auf dieselben kanonischen Namen, die der Prometheus-Pfad erzeugt, sodass
Durchsatz- und Reset-Logik unverändert wiederverwendet werden. Gemappt wird nur,
was das **verifizierte Schema** liefert; alles andere bleibt abwesend und wird
nie als `0` erfunden.

### 3.1 Verifizierter Stand

Verifiziert gegen Strata-Build **`0.1.40`** (`src/strata.rs:18`, Test
`maps_the_verified_strata_schema` `src/strata.rs:439`, „Shape captured from
Strata 0.1.40 (values synthetic)"). Live belegt ist nur dieser Build (Box nicht
benannt, Commit-Text `0629648`; siehe `docs/LIVE_VS_FIXTURE.md`). Ein Fixture ist
kein Live-Wert.

### 3.2 Kanonisches Mapping (Strata → OrsikTop)

Der Resolver matcht einen Alias gegen den Blatt-Schlüssel oder die letzten zwei
Pfadsegmente und normalisiert auf ASCII-klein (also `prompt_tokens`,
`promptTokens`, `prompt-tokens` gleich). Arrays (`requests`, `history`) werden
übersprungen, damit Einzelanfragen nicht als Aggregat erscheinen
(`flatten`, `src/strata.rs:323`).

| Kanonischer Name (Ziel) | Strata-Aliase (Gruppe) | Skalierung |
| --- | --- | --- |
| `llamacpp:prompt_tokens_total` | `prompt_tokens`, `prompt_tokens_total`, `total_prompt_tokens`, `prompt_token_count`, `prompt_eval_count`, `prefill_tokens` (`totals`) | ×1 |
| `llamacpp:tokens_predicted_total` | `output_tokens`, `generated_tokens`, `generated_tokens_total`, `completion_tokens`, `predicted_tokens`, `tokens_predicted` (`totals`) | ×1 |
| `llamacpp:prompt_seconds_total` | `prompt_seconds`, `prompt_seconds_total`, `prompt_time_seconds`, `prompt_eval_seconds`, `prefill_seconds` (`totals`, Sekunden) — sonst `prompt_ms`, `prompt_time_ms`, `prompt_eval_ms` (Millisekunden) | ×1 bzw. ×0,001 |
| `llamacpp:tokens_predicted_seconds_total` | `generation_seconds`, `generation_seconds_total`, `generation_time_seconds`, `decode_seconds`, `predicted_seconds`, `eval_seconds` (`totals`, Sekunden) — sonst `decode_ms`, `generation_ms`, `generation_time_ms` (Millisekunden) | ×1 bzw. ×0,001 |
| `llamacpp:requests_deferred` | `queued`, `requests_queued`, `queue_depth`, `deferred_requests`, `requests_deferred` (`live`) | ×1 |
| `llamacpp:spec_decode_num_draft_tokens_total` | `drafts_offered`, `draft_tokens_offered`, `draft_tokens_total` (`totals`) | ×1 |
| `llamacpp:spec_decode_num_accepted_tokens_total` | `drafts_accepted`, `accepted_tokens_total`, `num_accepted_tokens` (`totals`) | ×1 |

Anker: `CANONICAL` (`src/strata.rs:57-156`), Auflösung `resolve`/
`extract_samples` (`src/strata.rs:277`, `:358`). Ein explizites Sekundenfeld
gewinnt gegen ein Millisekundenfeld (Test
`an_explicit_seconds_field_wins_over_milliseconds`, `src/strata.rs:544`); ist
die kanonische Gruppe vorhanden, aber ein Zähler fehlt darin, wird er **nicht**
aus einer anderen Gruppe geborgt (`a_missing_totals_counter_is_not_borrowed_from_another_group`,
`src/strata.rs:589`).

### 3.3 Nicht von Strata geliefert (bewusst absent)

Diese Felder bleiben `—` und werden nie als `0` ausgegeben (Test
`maps_the_verified_strata_schema`, `src/strata.rs:493-509`):

- CTX high-water (watermark) — Strata hat kein `n_tokens_max`.
- REQ active — nur `live.queued` → `requests_deferred`; **kein** Active-Gauge.
- CACHE — kein `prompt_tokens_cached_total`.
- **SPEC drafts (Anzahl)** — Strata liefert `drafts_offered`/`drafts_accepted`
  (Tokens), aber keine **Draft-Anzahl**, also bleibt
  `spec_decode_num_drafts_total` abwesend.
- Reine `live`/`hardware`-Gauges (`tok_s`, `tok_s_mean`, `prefill_tok_s_mean`,
  `gpu_util`) werden **nicht** als Lebenszeit-Durchschnitt gemappt.

### 3.4 Schema-Toleranz

Die Erkennung ist gegen unbekannte künftige Felder tolerant: ein Objekt mit
`totals` + mindestens einer Markergruppe gilt als Strata, zusätzliche Felder
werden ignoriert. Fixture `strata_metrics_unknown.json` (mit
`a_future_field`, `unknown_*`), Test
`ignores_arrays_and_null_leaves` (`src/strata.rs:520`) und
`recognizes_strata_objects_only` (`src/strata.rs:427`).

### 3.5 Phasen- und Ratenfelder (Strata-spezifisch)

- `live.state` ist die autoritative Phase, normalisiert auf
  `idle` / `reading` / `generating` (`live_rates`, `src/strata.rs:236-247`); ein
  unbekannter/fehlender Zustand bleibt `None`, damit die llama.cpp-Heuristik
  greift.
- `live.tok_s` (Fensterrate) wird **nur** bei `state=generating` als LIVE TG
  gezeigt; ein echtes `0.0` bleibt `0.0`, ein fehlender Zustand liefert keine
  Rate (`live_rates_never_report_a_rate_while_idle_or_reading`,
  `src/strata.rs:624`).
- `live.prompt_tokens` / `live.generated` füllen REQUEST prompt/generated nur,
  wenn `/slots` sie nicht schon liefert, und nur bei aktivem Request
  (`src/llama.rs:605-613`).

## 4. Slot-Schemata (`/slots`)

`/slots` ist ein optionaler Endpunkt; fehlt er, degradiert OrsikTop, statt Werte
zu erfinden. Geparst wird in `apply_slots_json` (`src/llama/slots.rs:19`).

| Variante | Felder | CTX `used` | Beleg |
| --- | --- | --- | --- |
| llama.cpp (aktuell) | `id`, `n_ctx`, `is_processing`, `id_task`, `n_prompt_tokens`, `n_prompt_tokens_processed`, `n_prompt_tokens_cache`, `speculative`, `params`, `next_token[].n_decoded` | direkt aus `n_prompt_tokens` (echtes `0` inklusive) | `slots_active.json`, `slots_idle.json` |
| Strata **alt** (`0.1.40`) | nur `id`, `n_ctx`, `is_processing` — **kein** `n_prompt_tokens` | unbekannt → `—` gegen die Kapazität; REQUEST-Prompt kommt aus `/metrics` `live` | `slots_strata_old.json`; `strata_old_slots_report_capacity_only_never_occupancy` (`src/llama.rs:2209`) |
| Strata **neu** | zusätzlich `n_prompt_tokens` | direkt aus `n_prompt_tokens` (direkte Belegung) | `slots_strata_new.json`; `strata_new_slots_report_direct_occupancy` (`src/llama.rs:2229`) |

Feste Regeln:

- **Belegung ≠ Fortschritt.** CTX `used` stammt **ausschließlich** aus dem
  direkten `n_prompt_tokens`; sie wird nie aus `n_prompt_tokens_processed` oder
  `next_token[].n_decoded`(Fortschritt) hergeleitet (`src/llama/slots.rs:103-110`;
  Test `context_occupancy_is_not_derived_from_progress_counters`,
  `src/llama.rs:1877`). Ein vorhandenes `0` ist ein echtes Null
  (`present_zero_prompt_tokens_is_a_real_zero_not_the_progress`,
  `src/llama.rs:2179`); ein fehlendes `n_prompt_tokens` bleibt `None`/`—`.
- **CTX-Paar aus einem Slot.** Belegung, Kapazität und Slot-Tag stammen immer aus
  **einem** Slot (meist-genutzter Busy-Slot, sonst meist-genutzter Slot), nie
  aus unabhängigen Maxima (`src/llama/slots.rs:31-42`).
- **REQ prompt Fallback.** `n_prompt_tokens_processed` **oder**, falls der
  Server den Namen nicht führt, `n_prompt_tokens` (`src/llama/slots.rs:78-81`).
  Diesen Fallback nutzt auch Strata **neu** (das den Fortschrittsnamen nicht
  führt); die CACHE-/REQUEST-Semantik dieses Falls ist der in
  `docs/TELEMETRY_VERIFICATION.md` als **F2** geführte offene Befund und hier
  nur referenziert, nicht korrigiert.
- **Aggregat nur vollständig.** Meldet ein Busy-Slot seinen Fortschritts-Zähler
  nicht, ist die REQUEST-Summe unvollständig → `None`/`—`, kein stiller
  Teilsummenwert (`src/llama/slots.rs:46-50, 84-101`).
- **Slot-Identität.** Die Array-Position ist nie Identität; ohne `id` kein
  Slot-Tag (`src/llama/slots.rs:128-144`; Test
  `context_slot_id_is_the_slot_api_id_not_the_array_position`).

`/props` (gecacht, `PROPS_REFRESH = 30 s`) liefert die CTX-Kapazität
(`default_generation_settings.n_ctx`, Fixture `props.json`), die durch ein
Slot-`n_ctx` verfeinert wird, ohne auf `0` zu schrumpfen
(`src/llama/slots.rs:168-174`). Strata liefert die Kapazität ersatzweise aus
`engine.max_context` (Fallback `engine.context`) — reine Kapazität, nie
Belegung (`engine_capacity`, `src/strata.rs:218`).

## 5. MTP- und Speculative-Counter

### 5.1 Counter

| Counter | llama.cpp-Prometheus | Strata-`totals` |
| --- | --- | --- |
| Draft-Tokens (kumuliert) | `llamacpp:spec_decode_num_draft_tokens_total` | `drafts_offered` → `…draft_tokens_total` |
| Accepted-Tokens (kumuliert) | `llamacpp:spec_decode_num_accepted_tokens_total` | `drafts_accepted` → `…accepted_tokens_total` |
| Draft-**Anzahl** | `llamacpp:spec_decode_num_drafts_total` | **nicht vorhanden** → `—` |
| Accepted je Position | `llamacpp:spec_decode_num_accepted_tokens_per_pos_total{position=…}` | — |

Quelle: `src/llama.rs:533-540` (llama.cpp-Namen), `src/strata.rs:136-155`
(Strata-Aliase), Fixture `metrics_speculative.prom`. Ein reines
Counter-`>0` gilt als Enable-Hinweis (`src/llama.rs:541-546`).

### 5.2 Konfigurationsquelle und MTP-Kennzeichen

Die Spec-Konfiguration kommt **autoritativ vom Server** (`/props` und `/slots`),
ersatzweise aus der lokalen Prozess-CLI; das MTP-Kennzeichen in der UI ist
`spec_is_mtp`.

- Server-Sicht: `speculative_config` liest aus `/props` bzw. einem Slot
  (`src/llama/speculative.rs:8-32`): boolesches `speculative`,
  `params["speculative.n_max"]` / `speculative.n_max`,
  `params["speculative.types"]` / `speculative.types`. `draft-mtp` in der
  Typliste ⇒ MTP.
- Lokaler Fallback: `local_speculative_process_config` /
  `speculative_from_process` (`src/llama/speculative.rs:81,151`) inspiziert
  `/proc/<pid>/cmdline` und `environ` eines **lokalen** llama.cpp-Prozesses:
  `--spec-type` (bzw. `LLAMA_ARG_SPEC_TYPE`) = `draft-mtp` ⇒ MTP;
  `--spec-draft-n-max` (bzw. `LLAMA_ARG_SPEC_DRAFT_N_MAX`) ⇒ `n_max`.
- Vorrang (`apply_local_spec_fallback`, `src/llama/speculative.rs:47-65`):
  Ein Server-`speculative: false` oder `speculative.n_max: 0` schaltet ab und
  wird **nie** durch die CLI zu MTP überschrieben; alles, was der Server nicht
  aussagt, füllt die CLI. Der lokale Scan wird höchstens alle `SPEC_REFRESH`
  (30 s) neu gefahren (`src/llama.rs:37`).
- Anzeige: `MTP`/`MTP<n>` bei `spec_is_mtp`, sonst `SPEC`, sonst `MTP OFF`
  (`src/ui/llm.rs:124-136`); Test `reads_draft_mtp_type_from_custom_slot_shape`
  (`src/llama.rs:3417`).

## 6. Capability-Unterschiede llama.cpp vs. Strata

| Aspekt | llama.cpp (Prometheus) | Strata (JSON `/metrics`) |
| --- | --- | --- |
| Wire-Format | Prometheus-Text | JSON-Objekt (`totals` + Markergruppe) |
| Backend-Erkennung | `llamacpp`-Name im Sample | `is_strata_metrics` |
| CTX-Belegung | `/slots` `n_prompt_tokens` | alt: fehlt → `—`; neu: `n_prompt_tokens` |
| CTX-Kapazität | `/props` `n_ctx`, sonst Slot-`n_ctx` | dazu `engine.max_context`/`engine.context` |
| CTX watermark | `n_tokens_max` → `diag` `watermark` | nicht vorhanden → `—` |
| REQUEST prompt | `n_prompt_tokens_processed`, sonst `n_prompt_tokens` | Slot-Wert, sonst `live.prompt_tokens`; neu = Belegung (F2) |
| REQUEST generated | `next_token[].n_decoded` | `live.generated` |
| REQ active/deferred | `requests_processing` + `requests_deferred` | nur `live.queued` → deferred; Active immer `—` |
| LIVE PP | Counter-/Slot-Delta, sonst `prompt_total`-Delta | **nicht verfügbar** (kein Instant-Prefill) → `—` |
| LIVE TG | Slot-Delta, sonst `generated_total`-Delta | `live.tok_s` (Fensterrate), nur bei `state=generating` |
| AVG (LIFE) | `*_total` / `*_seconds_total`, Gauge-Fallback | `totals` (ms → s); Gauges nicht als Mittel |
| Phase (STATE) | Heuristik (`classify_phase`) | `live.state` autoritativ, normalisiert |
| CACHE | `prompt_tokens_cached_total` | nicht gemappt → `—` |
| Spec-Counter | `spec_decode_num_*` | `drafts_offered`/`drafts_accepted` (keine Draft-Anzahl) |
| Reset-Erkennung | Formatwechsel | zusätzlich `totals.since` |

Quellen: `src/llama.rs:463-540` (llama.cpp-Mapping, Phasen/Heuristik
`classify_phase` `:142`), `src/strata.rs` (Strata-Mapping und Live-Felder),
`src/llama/metrics.rs:12-52` (Dispatch). Ein Formatwechsel llama.cpp↔Strata
**oder** ein Strata-`totals.since`-Wechsel verwirft die Delta-Basis, statt über
die Lücke zu differenzieren (`src/llama.rs:548-563`; Test
`a_wire_format_switch_resets_the_counter_baseline`, `src/llama.rs:1506`).

## 7. Test- und Live-Evidenz

**Fixture-/CI-Evidenz (läuft ohne Backend):** die unter §2–§5 genannten
Unit-Tests plus Fixtures `tests/fixtures/metrics_*.prom`, `props.json`,
`slots_*.json`, `strata_metrics_unknown.json`. End-to-End gegen einen
In-Process-Fake-Server: `sample_reads_strata_json_metrics_end_to_end`
(`src/llama.rs:1407`), `derives_live_context_from_slots_fixture`
(`src/llama.rs:1780`).

**Live-Evidenz:**

- llama.cpp `--parallel 2`, Build **b10909**: remote über Tailscale und lokal
  auf test-desktop (S24) — SLOTS 1/2, CTX/MTP(`MTP3`)/Spec-Counter, Live-TPS
  (`docs/HARDWARE_SUPPORT.md`, Multi-Slot-Zeile; `docs/LIVE_VS_FIXTURE.md`).
- Strata Build **`0.1.40`**: `/metrics` live (Box **nicht** benannt,
  Commit-Text `0629648`) — Totals completion-batched, `live.tok_s` als
  Fensterrate.

## 8. Offene Punkte und Grenzen

- **Nur Strata `0.1.40` ist live-verifiziert.** Das neuere `/slots` mit
  `n_prompt_tokens` ist als `slots_strata_new.json` erfasst, aber **nicht**
  live nachgeprüft; das live gelaufene Schema kannte das Feld nicht.
- **Kein Live-Box-Name** für den Strata-Lauf; die Evidenz ist der Commit-Text.
- **F2 (aus `docs/TELEMETRY_VERIFICATION.md`):** ohne
  `n_prompt_tokens_processed` (neueres Strata; llama.cpp ohne den Namen) zeigt
  REQUEST prompt die **KV-Belegung**, nicht den Prompt-Fortschritt. Hier nur
  dokumentiert; eine Korrektur wäre ein eigener Code-Task, nicht Teil von
  HW-02.
- **Strata Draft-Anzahl** und **Active-Request-Gauge** existieren im Schema
  nicht; beide bleiben dauerhaft `—`.
- Der Strata-Adapter mappt **nur** das verifizierte Schema; unbekannte Felder
  bleiben absent, nie `0` (`src/strata.rs:13-16`).

## Querverweise

- `docs/LIVE_VS_FIXTURE.md` — Live/Fixture/Unknown je Funktionsbereich.
- `docs/HARDWARE_SUPPORT.md` — Zeilen „llama.cpp server discovery",
  „Strata backend (JSON `/metrics`)", „Multi-slot / parallel llama.cpp".
- `docs/TELEMETRY_VERIFICATION.md` — Metrik-für-Metrik-Verifikation,
  Befunde F1–F4, Abschnitt „llama.cpp- vs. Strata-Versionsunterschiede".
- `README.md` — „Supported stack" und der Backend-Abschnitt.
- Code/Fixtures: `src/llama.rs`, `src/llama/{metrics,slots,props,speculative}.rs`,
  `src/strata.rs`, `src/ui/llm.rs`, `tests/fixtures/`.
