# Performance-Baseline — Dirty-only Redraw und Polling (reale Messdaten)

**Ticket:** CFA-35 (EPIC 3.1 „Reproduzierbares Performance-Profiling“, CFA-27)

Dieses Dokument trennt strikt zwischen **gemessenen** Werten, **abgeleiteten**
Werten und **modellierten** Werten und kennzeichnet jede Zahl als eines davon.
Es ändert keine Produktionslogik; insbesondere bleibt `src/strata.rs`
unangetastet, es wird kein Inferenz-/Modellserver kontaktiert und kein echter
llama.cpp befragt. Es werden ausschließlich lokale Mock-Server im Testprozess
verwendet.

## Quellstand und Provenienz (F6)

- **Quellstand:** HEAD `2e0d37f` **plus uncommittete Worktree-Änderungen** —
  nicht nur der Commit. Der geteilte Worktree enthält zusätzlich die
  noch nicht committeten CFA-33/CFA-34-Arbeiten (`src/llama.rs`, gestagte
  `docs/TELEMETRY_VERIFICATION.md`, drei `tests/fixtures/*`). Der Benchmark
  selbst ist Teil von `src/app.rs`.
- **Snapshot = ganzer Worktree.** Der Prüf-Helfer hasht den gesamten
  Arbeitsbaum (getrackte **und** untrackte Dateien), nicht den Git-Commit.
  Deshalb:
  - jeder Lauf mit verändertem Worktree ergibt einen anderen Snapshot-Hash,
  - die frühere Formulierung „drei Läufe am selben Quellstand“ war ungenau:
    die Läufe A/B/C hatten unterschiedliche Hashes, weil der gemeinsame
    Worktree (parallele CFA-33-Arbeit) sich zwischen den Läufen änderte,
  - der Snapshot ist **aus Git allein nicht rekonstruierbar**.
- **Der Snapshot umfasst auch dieses Dokument.** Der Hash geht über den ganzen
  Worktree, deshalb ändert **jede Bearbeitung von `docs/PERFORMANCE_BASELINE.md`**
  den Snapshot erneut; ein im Text genannter „finaler“ Hash kann daher nie der
  endgültige sein. Maßgeblich ist der Snapshot des letzten Gates **nach**
  Einfrieren dieses Dokuments. Die folgenden Zahlen stammen aus vier Läufen über
  praktisch denselben Code (Autor `73392475…`/Snapshot `a3cd34d1…`, `6385bc02…`/
  Snapshot `ed3ef4b7…`, früherer `157f3e…`/Snapshot `82c55cb…`, Reviewer
  `1d476b16…`/Snapshot `92796a60…`) und werden in der
  Reproduzierbarkeitstabelle einheitlich nebeneinandergestellt.
- **Referenzlauf nach dem Commit wiederholen.** Sobald CFA-35 (und CFA-33)
  committet sind, ist der maßgebliche Basislauf neu zu erzeugen; die hier
  dokumentierten Zahlen gelten für den oben genannten Worktree-Zustand.

## Zweck

Die Dirty-only-Redraw-Optimierung (`9d1e08e`, „perf(app): repaint the TUI only
when the rendered view changes“) war bislang nur strukturell belegt. Diese
Baseline

- quantifiziert die Wirkung des Redraw-Schedulers auf Rendering und CPU
  (Redraw-Scheduler selbst ist ein Modell, die CPU-Kosten sind gemessen),
- erfasst Renderdauer, Polling-Dauer, TUI-Eingabelatenz und Thread-CPU über die
  realen Laufzeitpfade,
- trennt „erstes Frame im Testprozess“ vom warmen Dauerbetrieb,
- dient als **Referenz für spätere Performance-Regressionen** (EPIC 3.2).

## Benchmark-Verfahren (zwei Tests, reproduzierbar)

### 1. Regulärer In-Gate-Benchmark (billig, gültig, keine `/proc`-Tabellen)

`app::tests::perf_baseline_benchmark_for_dirty_only_redraw_and_polling`
(`src/app.rs`). Er läuft in **jedem** freigegebenen Gate und ist dadurch
reproduzierbar. Er übt die realen Pfade aus:

- den Produktions-`RedrawScheduler` (nur **modelliert**, siehe F3),
- die reale `ui::draw`-Render-Closure über ein ratatui-`TestBackend`
  (erstes Frame im Testprozess, Warmbetrieb, wiederholte Eingabelatenz),
- den synchronen `LlamaMonitor::sample`-HTTP-Poll gegen 1, 4 und 16 echte
  In-Process-Mock-Server mit **blockierendem `accept`** (`spawn_blocking_mock`,
  Stop über eine Weck-Verbindung). In den gemessenen Zyklen holt `sample`
  (`src/llama.rs:333-345`) nur `/metrics` und `/slots` — nebenläufig in zwei
  kurzlebigen `thread::scope`-Threads —, **nicht** `/props` (das wird nur im
  30-s-Refresh-Fenster nachgeladen). Gegen den seriell antwortenden Mock mit
  `Connection: close` (`src/app.rs:2175`) enthält der Messwert damit den
  Thread-Start und einen neuen Verbindungsaufbau je Anfrage, aber kein `/props`.

Er liest **keine** `/proc`-Prozesstabelle, läuft ~0,3 s zusätzlich und ist damit
klein genug für das geteilte Gate.

### 2. `#[ignore]`-Benchmark (schwer: `/proc`+DRM und Dauerbetrieb)

`app::tests::perf_baseline_soak_and_process_collection_benchmark` (`src/app.rs`),
`#[ignore]`. Er enthält genau die teuren Teile, die **nicht** in jedem Gate
laufen dürfen:

- den produktiven Prozesszyklus `system.refresh_processes_specifics` **plus**
  `collect_process_stats` (beide zusammen und getrennt gemessen),
- ein ~10 s dauerndes Renderfenster mit sekündlicher RSS-/Thread-Abtastung
  („längerer Betrieb“, erste Näherung).

Zielaufruf (ein Test-Thread, gezielt):

```
cargo test --all-targets --locked -- --ignored --test-threads=1 \
  perf_baseline_soak_and_process_collection_benchmark
```

**Offener Punkt:** Das freigegebene Gate `pc-project cargo test --all-targets
--locked` akzeptiert nur die feste Argumentliste (`pc-heavy-check` `VALID`) und
kann `-- --ignored …` **nicht** durchreichen; ein lokaler `cargo`-Lauf ist per
Team-Regel untersagt. Der `#[ignore]`-Benchmark benötigt daher einen
**operator-freigegebenen Benchmark-Runner**. Bis dahin sind die
Prozesskollektor-Zahlen **nicht** aus diesem Lauf belegt (siehe unten).

### Rohdaten-Kanal

Ergebnisse werden als `ORSKTOP_BENCH|<key>=<value>` auf den **rohen**
Prozess-Stdout geschrieben (libtest fängt `println!` ab). Beim In-Gate-Test
verschränken sich diese Zeilen mit der libtest-Ausgabe (Zeilen bleiben aber
einzeln und mit Präfix parsebar).

### Keine Schwellwerte

Alle Zeitwerte werden *berichtet*, nie *erwartet*. Die Tests prüfen nur
strukturelle Invarianten (Szenariogrößen, nicht-leere Messreihen). Sie können
dadurch nicht flaken. Die Zahlen sind eine maschinenspezifische Referenz.

## Testbedingungen (belegt)

| Feld | Wert | Quelle |
| --- | --- | --- |
| Host | `test-desktop` | `result.json` / `worker-info` |
| CPU | 12th Gen Intel(R) Core(TM) i9-12900K, 24 logische CPUs | `ORSKTOP_BENCH\|host_cpu_model`, `host_logical_cpus` |
| RAM | 65 555 880 kB (≈ 62,5 GiB) | `ORSKTOP_BENCH\|host_mem_total_kb` |
| Kernel | `7.2.5-3-omarchy` | `ORSKTOP_BENCH\|host_kernel` |
| GPU | NVIDIA GeForce RTX 4090, **39 % Auslastung**, 4761 MiB | `worker-info` zum Messzeitpunkt |
| Toolchain | `rustc 1.100.0-nightly (feaadeeac 2026-09-19)` | `worker-info` |
| Quellstand | HEAD `2e0d37f` + Worktree (s. o.) | `result.json` |
| Exitcode | 0 | `result.json` |
| Testsumme | `477 passed / 0 failed / 4 ignored`, 0,56–0,60 s | `output.log` |
| **Build-Profil** | **`test` (unoptimized + debuginfo)** | `output.log` (`Finished `test` profile [unoptimized + debuginfo]`); `Cargo.toml` hat nur `[profile.dist]` |

**Alle in diesem Dokument genannten Zeit- und CPU-Werte sind Werte des
unoptimierten `test`-Profils, nicht der Release-Binary.** Das Gate baut ohne
`--release`. Rendern und HTTP-Polling sind im Debug-Build deutlich langsamer als
in der ausgelieferten Binary, deshalb sind die **absoluten** Werte (Framezeit,
CPU/Frame, Leerlauf-CPU-Prozentsätze) **nicht auf die Release-Binary
übertragbar**. Profilunabhängig ist das **Verhältnis** der Redraw-Raten
(20/s gegen 4/s), da es nur aus dem Scheduler bzw. einem Literal folgt. Eine
Release-Messung steht aus (siehe „Einschränkungen und offene Punkte“).

Die Suite lief **parallel** (libtest-Standard). Das verrauscht die **Wanduhr**-
Werte; die **Thread-CPU** ist davon unabhängig, weil sie nur den eigenen Thread
liest (sie ist aber jiffy-quantisiert, s. u.).

## Rohdaten — regulärer In-Gate-Lauf (primärer Lauf)

Lauf über `pc-project cargo test --all-targets --locked`, Artifact
`<artifact-root>/heavy-checks/157f3e387f3f46108d4196d5b19a5007`,
Snapshot `82c55cbd29c9519a68ffd3ffc79c14d0111e8c90bef03e84e346cda4d6f3cc4d`,
`source_unchanged: true`, `exit 0`.

```
ORSKTOP_BENCH|model_redraw_pre_optimization_per_s=20
ORSKTOP_BENCH|model_redraw_idle_per_s=4
ORSKTOP_BENCH|model_redraw_per_s_interval_100ms=10
ORSKTOP_BENCH|model_redraw_per_s_interval_250ms=4
ORSKTOP_BENCH|model_redraw_per_s_interval_1000ms=4
ORSKTOP_BENCH|render_first_frame_in_process_us=7715
ORSKTOP_BENCH|render_warm_mean_us=3696
ORSKTOP_BENCH|render_warm_p95_us=6476
ORSKTOP_BENCH|render_warm_max_us=7660
ORSKTOP_BENCH|render_warm_max_fps=270
ORSKTOP_BENCH|render_thread_cpu_ticks_total=22
ORSKTOP_BENCH|render_cpu_us_per_frame_derived=3667
ORSKTOP_BENCH|derived_idle_cpu_pct=1.467
ORSKTOP_BENCH|derived_pre_optimization_cpu_pct=7.333
ORSKTOP_BENCH|rss_kb_after_render=27468
ORSKTOP_BENCH|threads_after_render=8
ORSKTOP_BENCH|input_to_frame_mean_us=4545
ORSKTOP_BENCH|input_to_frame_p95_us=4604
ORSKTOP_BENCH|input_to_frame_max_us=4699
ORSKTOP_BENCH|poll_1_servers_cycle_mean_us=827
ORSKTOP_BENCH|poll_1_servers_cycle_max_us=1137
ORSKTOP_BENCH|poll_1_servers_per_server_us=827
ORSKTOP_BENCH|poll_4_servers_cycle_mean_us=3102
ORSKTOP_BENCH|poll_4_servers_cycle_max_us=3408
ORSKTOP_BENCH|poll_4_servers_per_server_us=775
ORSKTOP_BENCH|poll_16_servers_cycle_mean_us=12268
ORSKTOP_BENCH|poll_16_servers_cycle_max_us=13557
ORSKTOP_BENCH|poll_16_servers_per_server_us=766
ORSKTOP_BENCH|measurement_wall_ms=575
ORSKTOP_BENCH|process_rss_kb=27416
ORSKTOP_BENCH|process_threads=3
ORSKTOP_BENCH|thread_cpu_ticks_total=40
ORSKTOP_BENCH|thread_cpu_pct_avg=69.53
ORSKTOP_BENCH|host_cpu_model=12th Gen Intel(R) Core(TM) i9-12900K
ORSKTOP_BENCH|host_logical_cpus=24
ORSKTOP_BENCH|host_kernel=7.2.5-3-omarchy
ORSKTOP_BENCH|host_mem_total_kb=65555880
```

`render_warm_p95_us`/`max` liegen wegen paralleler Tests über dem Mittel; die
CPU-Felder sind davon unabhängig (Thread-CPU).

## Reproduzierbarkeit und Streuung

Vier unabhängige Läufe des In-Gate-Benchmarks (praktisch identischer Code,
jeweils anderer Worktree-Snapshot, teils parallele Testlast):

| Metrik | A `157f3e` (`82c55cb…`) | B `73392475` (`a3cd34d1…`) | C `6385bc02` (`ed3ef4b7…`) | Reviewer `1d476b16` (`92796a60…`) |
| --- | --- | --- | --- | --- |
| `render_first_frame_in_process_us` | 7 715 | 9 523 | 8 477 | 13 757 |
| `render_warm_mean_us` | 3 696 | 3 556 | 3 671 | 3 660 |
| `render_warm_p95_us` | 6 476 | 3 982 | 5 178 | 4 767 |
| `render_cpu_us_per_frame_derived` | 3 667 | 3 667 | 3 667 | 3 500 |
| `render_thread_cpu_ticks_total` | 22 | 22 | 22 | 21 |
| `derived_idle_cpu_pct` | 1,467 | 1,467 | 1,467 | 1,400 |
| `derived_pre_optimization_cpu_pct` | 7,333 | 7,333 | 7,333 | 7,000 |
| `input_to_frame_mean_us` | 4 545 | 4 551 | 4 577 | 4 541 |
| `poll_1_servers_per_server_us` | 827 | 750 | 963 | 929 |
| `poll_4_servers_per_server_us` | 775 | 795 | 807 | 824 |
| `poll_16_servers_per_server_us` | 766 | 744 | 786 | 834 |
| `poll_16_servers_cycle_mean_us` | 12 268 | 11 904 | 12 588 | 13 355 |
| `thread_cpu_pct_avg` | 69,53 | 71,25 | 70,49 | 69,04 |
| `process_rss_kb` | 27 416 | 27 256 | 27 260 | 27 264 |

Die Streuung ist **größer als „wenige Prozent“** — die erste Fassung dieses
Dokuments hat sie zu optimistisch beschrieben:

- `render_first_frame_in_process_us`: 7,7 / 8,5 / 9,5 / 13,8 ms — **Faktor ~1,8**
  (das erste Frame ist unter paralleler Testlast am stärksten betroffen),
- `poll_*_per_server_us` bei **einem** Server: 750–963 µs — bis **~28 %**,
- `render_cpu_us_per_frame_derived`: 3 500–3 667 µs; die Thread-Ticks 21/22
  zeigen, dass die scheinbare **Gleichheit** mehrerer Läufe (3 667 = 3 667)
  reine **Quantisierung** ist: **ein Tick = 10 ms ≈ 4,5 %** der 60-Frame-Summe
  (~0,22 s CPU).

Damit ist über Läufe hinweg belastbar vor allem das **Verhältnis und die
Größenordnung** (20/s gegen 4/s; Poll näherungsweise linear; CPU/Frame ≈
Framezeit), **nicht** ein einzelner Prozentwert. Für EPIC 3.2 heißt das: eine
Regression **unterhalb dieser Spannen** (Frame bis ~×1,8, Poll/Server bis ~28 %,
abgeleitete CPU < ~4,5 %) ist mit dieser Messung **nicht** nachweisbar.

### Abschlussgates

Gate-Läufe über den finalen Code (Autor und Reviewer, jeweils `test`/`clippy`/
`fmt`). Der Snapshot hasht den **ganzen** Worktree inklusive dieses Dokuments,
ändert sich also mit **jeder** weiteren Bearbeitung dieser Datei.

| Gate | Artifact | Ergebnis |
| --- | --- | --- |
| `cargo fmt --all -- --check` (Autor, final) | `d4b8a64479a14eaeb3adf6e955e0cfd4` | Exit 0 |
| `cargo clippy --all-targets --locked -- -D warnings` (Autor, final) | `35835568e92f4f75a98381f62e127931` | Exit 0 |
| `cargo test --all-targets --locked` (Autor, final) | `4166fc0e59074a3f9c15adc21b8f3394` | Exit 0, `477 passed / 0 failed / 4 ignored`, 0,59 s |
| `cargo test --all-targets --locked` (Reviewer) | `1d476b1628fc4c739a2d65594158ae78` | Exit 0, `477 passed / 0 failed / 4 ignored`, 0,60 s |

Die drei Autor-`(final)`-Läufe stammen gemeinsam vom Snapshot
`957b4a24fec75e0dbee2b43dc26b3d3b5502388353f963ab2099852137867fcc`; die
Reviewer-`test`-Zahl vom Snapshot `92796a60…`. **Hinweis (N3):** Da der Hash das
Dokument mit einschließt, erzeugt bereits das Eintragen dieses Satzes wieder
einen neuen Snapshot; `957b4a24…` ist daher der letzte gemessene Stand **vor**
dieser Schlussnotiz. Frühere Läufe desselben Codes über andere Worktree-Stände:
`ed3ef4b7…` (Autor) und `92796a60…` (Reviewer).

(Die vier ignorierten Tests umfassen den schweren `#[ignore]`-Benchmark; er wird
im Gate **nicht** ausgeführt.)

## Ergebnis und Interpretation

**Dirty-only Redraw wirkt — aber die Renderhäufigkeit ist ein Modell (F3).**
`model_redraw_*` kommen aus dem deterministischen Produktions-`RedrawScheduler`
(bzw. der Literal-Referenz 20/s) und sind **keine** neuen Messdaten; sie sind
bereits durch `idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick` belegt.
Die Aussage: vor der Optimierung 20 Voll-Redraws/s (Modell), jetzt im Leerlauf
4/s (Modell). **Das Abnahmekriterium „Messdaten statt Schätzungen“ ist für
diesen Punkt daher nicht als erfüllt gemeldet.**

**CPU-Kosten des Redraws (gemessen, abgeleitet) — die Kernfrage (F2).**
**Alle CPU- und Zeitwerte in diesem Abschnitt sind Debug-Profil-Werte** (siehe
Testbedingungen). Die CPU kommt aus `/proc/thread-self/stat`, also **nur** aus
dem eigenen Thread; parallele libtest-Threads können sie nicht aufblasen. Ein
einzelnes ≤6 ms-Frame liegt unter der 10-ms-Jiffy-Auflösung, deshalb wird die
CPU pro Frame aus der Summe über 60 Frames abgeleitet; durch diese
Quantisierung springt `render_cpu_us_per_frame_derived` in **4,5 %-Schritten**
(Ticks 21–22 ⇔ 3 500–3 667 µs) — die scheinbare Gleichheit zweier Läufe ist
keine Stabilität:

- `render_cpu_us_per_frame_derived = 3 500–3 667 µs` gegen
  `render_warm_mean_us ≈ 3 556–3 696 µs` → das Rendern ist praktisch
  **CPU-gebunden** (CPU ≈ Wanduhr).
- Abgeleitete Leerlauf-CPU = Redraws/s × CPU/Frame:
  - heute (4/s): **`derived_idle_cpu_pct = 1,40–1,47 %`** einer logischen CPU,
  - vor der Optimierung (20/s): **`derived_pre_optimization_cpu_pct = 7,00–7,33 %`**.
  - Ersparnis ≈ **5,6–5,9 Prozentpunkte einer logischen CPU** im Leerlauf.
  Diese Werte sind als **abgeleitet** gekennzeichnet (Modellrate × gemessene
  CPU/Frame), nicht als direkt gemessene CPU-Prozentsätze.
  **Nicht auf die Release-Binary übertragbar:** die Ersparnis ist eine
  Debug-Profil-Größe. Übertragbar ist allein die **Reduktion der Redraw-Rate**
  (20/s → 4/s, Faktor 5); die absoluten Prozentpunkte skalieren mit der (in
  Release kleineren) Render-CPU/Frame. Ohne Release-Messung (offen, Runner
  nötig) ist **kein** Release-Prozentsatz belegt.

**Polling skaliert linear und ist jetzt gültig gemessen (F1).**
Der alte Mock (`spawn_plain_server`) schlief bei `WouldBlock` 2 ms; der sequenzielle
Poll traf deshalb fast immer auf den schlafenden Accept-Thread, und der frühere
Wert ~2,06 ms/Server war im Wesentlichen das **Schlafquantum**. Der neue Mock
(`spawn_blocking_mock`) blockiert in `accept` und antwortet bei Ankunft; der
Messwert fällt auf **~0,74–0,96 ms/Server** (vier Läufe) und liegt über 1/4/16
Server näherungsweise konstant (16-Server-Zyklus 11,9–13,4 ms statt vorher
33 ms); die Streuung bei **einem** Server reicht bis ~28 % (s. o.).
**Gegenprobe:** der gemessene Wert bewegt sich nicht mehr mit dem 2-ms-Raster,
sondern mit der realen Client-Arbeit.

**Erstes Frame vs. Programmstart (F5b).**
`render_first_frame_in_process_us` (vier Läufe: 7,7 / 8,5 / 9,5 / 13,8 ms,
Debug-Profil) ist das **erste Frame im Testprozess**, **nicht** der
Programmstart; die Spanne ist wegen der Sensitivität des ersten Frames
gegenüber paralleler Testlast groß (Faktor ~1,8). GPU-Discovery, NVML-Init und
Sensor-Discovery laufen in `spawn_fast_worker` (`src/app.rs:867-902`) und sind
**nicht** enthalten.

**Eingabelatenz (F5c).**
`input_to_frame_*` ist über 30 Wiederholungen gemittelt (Mittel ~4,5 ms ≈ ein
warmes Frame). Der Anteil des Terminal-Event-Polls (`event::poll`) der echten
`app::run`-Schleife ist im `TestBackend`-Harness nicht reproduzierbar und
**nicht** enthalten.

**Footprint.**
RSS ~27 MB und `process_threads` (3 am Ende) beschreiben den **Testprozess**,
nicht die produktive TUI (die zusätzlich ihre Worker-Threads hat).

**Prozesskollektor (offen, F5a).**
Der produktive Zyklus ist `refresh_processes_specifics` **plus**
`collect_process_stats`; die frühere Zahl ~0,55 s maß nur den Kollektor und
unterschätzt den Zyklus. Diese Messung liegt im `#[ignore]`-Benchmark und ist
aus dem Gate **nicht** erzeugbar (Runner nötig). Der Reviewer hat die reine
Kollektor-Zahl (~0,55 s über ~1990 Prozesse) unabhängig reproduziert; sie ist
ein belastbarer Kandidat für EPIC 3.2, die vollständigen Zykluskosten bleiben
bis zu einem freigegebenen Runner offen.

## Baseline-Tabelle (für Regressionen)

Alle Zeit- und CPU-Werte sind **Debug-Profil-Werte** (`test`, unoptimized +
debuginfo), **nicht** die Release-Binary (siehe Testbedingungen). Angegeben ist
die über **vier** Läufe beobachtete Spanne.

| Metrik | Baseline (test-desktop, Debug-Profil) | Art |
| --- | --- | --- |
| Idle-Redraws | 4 /s | **Modell** |
| Redraws bei 100 ms | 10 /s | **Modell** |
| Redraws vor Optimierung | 20 /s | **Modell** |
| Frame warm (Mittel) | 3,6–3,7 ms | gemessen |
| Frame warm (p95) | 4,0–6,5 ms (parallel, verrauscht) | gemessen, verrauscht |
| Erstes Frame im Testprozess | 7,7–13,8 ms (Faktor ~1,8) | gemessen |
| Eingabe → Frame (Mittel) | ~4,5 ms | gemessen (30 Reps) |
| Poll pro Server | 0,74–0,96 ms (1 Server: bis ~28 %) | gemessen (linear bis 16 Server) |
| Poll 16 Server (Zyklus) | 11,9–13,4 ms | gemessen |
| CPU/Frame | 3 500–3 667 µs (±1 Tick ≈ 4,5 %) | **abgeleitet** (60-Frame-Summe) |
| Leerlauf-CPU (4/s) | 1,40–1,47 % einer CPU | **abgeleitet** |
| Leerlauf-CPU vorher (20/s) | 7,00–7,33 % einer CPU | **abgeleitet** |
| Thread-CPU gesamt (Messfenster) | ~69–71 % einer CPU | gemessen |
| RSS | ~27 MB | gemessen (Testprozess) |
| Threads | 3 | gemessen (Testprozess) |
| Prozesszyklus (refresh+collect) | offen (Runner nötig) | **nicht belegt** |

**Regressionsschwelle:** Da die Spanne zwischen unabhängigen Läufen so groß ist
(erste Frame ×1,8, Poll/Server bis ~28 %, CPU/Frame-Quantisierung 4,5 %), ist
über die Baseline eine Regression **unterhalb** dieser Spannen **nicht**
nachweisbar. Für EPIC 3.2 sind die Verhältnisgrößen (Redraw-Rate, lineares
Polling) die belastbare Grundlage, nicht einzelne Absolutwerte.

## Einschränkungen und offene Punkte (nicht behauptet)

Die folgenden Lücken werden **ausdrücklich als offen** benannt, statt die
Abnahmekriterien pauschal als erfüllt zu melden (F4):

- **„Längerer Betrieb“** ist nur als ~10 s-Renderfenster im `#[ignore]`-Benchmark
  vorhanden (RSS/Threads sekündlich) und wird im Gate nicht ausgeführt; ein
  echter Mehrstunden-Soak gehört zu EPIC 4.1.
- **Per-Szenario-Ressourcen der Anwendung** (Idle, 1/4/16 Server: CPU, RSS,
  Threads) sind **nicht** gemessen. Der Benchmark läuft im Testprozess; dessen
  RSS/Threads sind durch die Mock-/reqwest-Threads verfälscht und beschreiben
  nicht die TUI.
- **Refresh-Intervalle** existieren nur als Modell (F3).
- **Programmstart** (Discovery/NVML/Sensoren) ist nicht gemessen (F5b).
- **Event-Poll-Anteil** der Eingabelatenz fehlt (F5c).
- **CPU-Spitzen pro Frame** sind unter der 10-ms-Jiffy-Auflösung nicht messbar
  und werden deshalb **nicht** berichtet.
- **Prozesszyklus** (refresh+collect) benötigt den operator-freigegebenen Runner
  (s. o.).
- **Release-Messung fehlt:** alle Zeit-/CPU-Werte stammen aus dem `test`-Profil
  (**unoptimized + debuginfo**), nicht aus der ausgelieferten `--release`-Binary.
  Das Gate baut ohne `--release`; die absoluten Werte und die
  idle-CPU-Prozentsätze sind daher nur für das Debug-Profil gültig und **nicht**
  auf die Release-Binary übertragbar. Eine Release-Messung benötigt den
  operator-freigegebenen Runner (s. o.).
- Nur **eine** Hardware (test-desktop, i9-12900K); die Zahlen sind
  maschinenspezifisch.
- Transport ist **localhost-Mock**, nicht ein realer Netzwerk-Server.
- `BENCH_CLK_TCK = 100` wird angenommen (Linux-Standard), nicht geprobt.
- Der Benchmark misst die **synchronen** Laufzeitpfade im Testprozess, nicht die
  vollständige `app::run`-TUI-Schleife mit Crossterm-Backend.

## Scope und Änderungen

Geändert wurde ausschließlich Testcode in `src/app.rs` (zwei Benchmark-Tests +
Helfer, nur `mod tests`) sowie diese neue Dokumentation. Keine Produktionslogik,
keine Konfiguration, `src/strata.rs` unverändert. Die Messungen liefen
ausschließlich über das freigegebene test-desktop-Gate (`pc-project cargo test`),
mit Host, Toolchain, Snapshot-Hash, Exitcode und Rohlog belegt.

## CFA-36 — Prozesskollektor: DRM-fd-Scan optimiert (Vorher/Nachher)

Dieser Abschnitt gehört zu **CFA-36** (EPIC 3.2 „Nur bestätigte Engpässe
optimieren“). Er baut auf dem `#[ignore]`-Benchmark aus CFA-35 auf, der seit der
operator-freigegebenen Runner-Verdrahtung im Gate ausführbar ist. **Alle Werte
sind Debug-Testprofil-Werte** (`test`, unoptimized + debuginfo) auf `test-desktop`,
gemessen über das Gate
`pc-project cargo test --all-targets --locked -- --ignored --test-threads=1 perf_baseline_soak_and_process_collection_benchmark`.

### Engpass-Analyse (belegt)

Der Benchmark wurde um eine **Komponenten-Aufteilung** des Kollektors erweitert
(zwei zusätzliche `ORSKTOP_BENCH`-Größen, nur im `#[ignore]`-Test): der
Prozesszyklus ist die Summe aus dem Thread-Zähler-Lesen (`/proc/<pid>/status`,
eine Datei je Prozess) und dem DRM-fd-Scan (`/proc/<pid>/fd` + `fdinfo`, je
Prozess). Über die Läufe hinweg dominiert der **DRM-fd-Scan** klar:

| Größe | typischer Wert (Debug) | Anteil |
| --- | --- | --- |
| `process_threads_read_total_us` | ~15 000–22 000 µs (ganze Tabelle) | ~5–10 % |
| `process_drm_scan_total_us` | ~260 000–295 000 µs (ganze Tabelle) | **~90–95 %** |
| `process_collect_mean_us` | ~277 000–318 000 µs | (Summe + Aggregation) |

Eine einmalige Detailmessung (Instrumentierung während der Analyse, 1806 bzw.
1805 Prozesse, **einmalige Diagnose, nicht Teil des Dauer-Benchmarks**;
Artifacts `1233195fe2cc44e1946ceb0f3ceaf3ba` und
`e793e4fdd4cd43039a70572233adb6d6`) trennt den Scan weiter auf:

| Phase | Wert | Anteil am Scan | Anzahl |
| --- | --- | --- | --- |
| fd-Verzeichnis listen (gesamte Listenphase je `fd`-Verzeichnis) | ~164 000–178 000 µs | ~32 % | 1 306–1 307 lesbare `fd`-Verzeichnisse, **~295 000–305 000 fds** |
| je fd `readlink` (`symlink_target`) | ~351 000–393 000 µs | **~68 %** | ~295 000–305 000 readlink-Aufrufe |

Die ~32 % sind die **gesamte Listenphase**; welcher Anteil davon auf
`file_type()` entfällt, ist **nicht getrennt gemessen**. Ebenso ist **nicht
getrennt gemessen**, wie viel `read_dir_names` allein (ohne
`symlink_target_starts_with` und Puffer-Wiederverwendung) zum Gewinn beiträgt.

Damit ist belegt: **Der bestätigte Engpass ist der DRM-fd-Scan**, und darin das
**je fd anfallende `readlink`** (ein Syscall je offenem fd). Die Anzahl der
Syscalls ist ohne Caching/Staleness-Kompromiss nicht reduzierbar; reduzierbar
sind die **Allokationen** um diesen Syscall herum (Debug-Profil).

### Optimierung (verlustfrei)

Zwei kleine, generische Erweiterungen der `system::Sys`-Naht (beide mit
Default-Impl, also für `FixtureSys`/weitere Implementierungen automatisch
korrekt) plus eine Änderung des Scans in `src/drm.rs`:

- `read_dir_names` — listet nur die **Namen** eines Verzeichnisses, ohne die
  `file_type()`-Klassifikation je Eintrag; der fd-Scan braucht nur die fd-Nummern
  (`/proc/<pid>/fd` enthält keine Unterverzeichnisse).
- `symlink_target_starts_with` — prüft das Symlink-Ziel **byteweise** gegen ein
  Präfix (`b"/dev/dri/"`), statt für **jeden** fd ein `String`-Ziel zu bauen
  (`to_string_lossy().into_owned()`).
- `sample_process_gpus` (`src/drm.rs`) baut den `/proc/<pid>/fd/<n>`- und
  `/proc/<pid>/fdinfo/<n>`-Pfad in **wiederverwendeten Puffern** statt je fd neu
  (`join`-Allokation).

**Keine Semantikänderung:** Es wird weiterhin **jeder** offene fd genau einmal
per `readlink` geprüft; der Vergleich `starts_with("/dev/dri/")` bleibt inhaltlich
identisch (byteweise vs. `str`-Präfix). Die Telemetrie-Aktualität bleibt
**unverändert** (gleiche Frequenz `process_refresh_ms`, gleiche Daten); es wird
kein Wert zwischengespeichert.

### Vorher/Nachher (gepaart, gleicher Host/Zustand)

Um Host-Last als Störgröße auszuschließen, wurden Vorher- und Nachher-Zustand
**unmittelbar hintereinander** gemessen (nahezu gleiche Prozesszahl, nur der Code
`src/drm.rs`+`src/system.rs` getauscht). Der Vorher-Zustand ist der Stand
`84da992`; der Nachher-Zustand ergänzt die o. g. Naht-Methoden. Nur Paar 3 hat
eine **exakt identische** Prozesszahl; Paar 1 weicht um 3 und Paar 2 um 27
Prozesse ab, daher ist die `Δ/prozess`-Spalte die belastbarere Größe. Drei Paare:

| Paar | Prozesse (vor/nach) | `collect_mean` vor → nach (µs) | Δ | `drm_scan` vor → nach (µs) | Δ/prozess |
| --- | --- | --- | --- | --- | --- |
| 1 | 1755 / 1758 | 312 883 → 282 753 | **−9,6 %** | 288 010 → 265 316 | 164,1 → 150,9 µs (−8,0 %) |
| 2 | 1782 / 1755 | 318 443 → 277 293 | **−12,9 %** | 296 191 → 256 696 | 166,2 → 146,3 µs (−12,0 %) |
| 3 | 1752 / 1752 | 310 365 → 281 124 | **−9,4 %** | 289 496 → 261 397 | 165,2 → 149,2 µs (−9,7 %) |

Ergebnis: ein **reproduzierbarer Gewinn von ~10 %** auf dem dominierenden
Bauteil (`process_collect_mean_us` und `process_drm_scan_total_us`) bei
**unveränderter Telemetrie** und ohne zusätzliche Syscalls.

Rohbelege der sechs Läufe (test-desktop-Gate, je `source_unchanged: true`,
`exit 0`), Vorher → Nachher je Paar:
`0066affd853547a1a92f35c368716149` → `6b43566779524dfc83a30795e3009d25`,
`2244e523898d442985c4def8909ec28f` → `0910bec9e2514b22887f35b0d966e7ef`,
`253590e0eecc4426b77f280505633ba5` → `f418628b3ddd4b55a252f6fad1956ba9`
(unter `<artifact-root>/heavy-checks/…`).

Ohne die Paarung wird der Gewinn **überschätzt**: ungepaarte Läufe auf
wechselnd belastetem Host streuen stark (dieselbe unbeänderte Codebasis ergab
`collect_mean` zwischen ~312 000 und ~697 000 µs), so dass der Vergleich nur
**innerhalb** eines Paares belastbar ist. Das bestätigt die Streuungs-Warnung
aus CFA-35 (F3).

**Abgrenzung zum Operator-Baseline-Lauf.** Der Operator-Baseline-Lauf
(`process_collect_mean_us=326054`, Snapshot `307184f3…`) lag in einem
Niedriglast-Fenster und ist damit **kompatibel** mit dem hier gemessenen
Nachher-Bereich (~277 000–282 000 µs). Der belastbare Vorher/Nachher-Vergleich
ist deshalb der **gepaarte** oben, nicht der Vergleich gegen den einzelnen
Operator-Lauf.

### Gates (Nachher-Stand)

| Gate | Ergebnis |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0 |
| `cargo clippy --all-targets --locked -- -D warnings` | Exit 0 |
| `cargo test --all-targets --locked` | Exit 0, `480 passed / 0 failed / 4 ignored` |

Gemeinsamer Snapshot der drei Läufe: `f0efbba1736d06e23a8c18dad8af3e2996e2255566a5e07651b37d8a46a648f9`
(Artifacts `548ebf087c1a4a869ca2fb5d1ace1ee5` / `27dd17ee939741629d6706af91fc6e6e` /
`709bc6ae81cc46709aebec68e003c704`). **Hinweis:** Der Hash umfasst dieses
Dokument; das Eintragen dieser Zeile erzeugt bereits einen neuen Snapshot, daher
ist `f0efbba1…` der letzte gemessene Stand **vor** dieser Notiz. Die drei neuen
Tests aus der Review-Nacharbeit (`real_read_dir_names_…`,
`real_symlink_target_starts_with_…`, `default_and_real_naht_methods_…`) laufen im
regulären (nicht-`#[ignore]`) Gate; die Testzahl steigt dadurch 477 → 480.

### Offene Punkte / Restrisiko

- Das verbleibende `readlink` je fd (der Syscall selbst) ist **nicht** ohne
  Caching reduzierbar. Caching (fd→DRM-Menge je Prozess, seltener neu scannen)
  würde die Telemetrie-Aktualität verschlechtern und verstößt gegen das
  Abnahmekriterium „keine Verschlechterung der Telemetrie-Aktualität“; daher
  **nicht** umgesetzt.
- Die Sub-Aufteilung (readlink vs. klassifiziertes `read_dir`) stammt aus einer
  **einmaligen** Analyse-Instrumentierung; der Dauer-Benchmark berichtet nur die
  Komponenten-Aufteilung Thread-Read vs. DRM-Scan.
- Kein Release-Profil; alle Werte gelten für das Debug-Testprofil (s. o.).

## CFA-92 — Release-Binary-Baseline (Anwendungsprozess, extern über `/proc`)

**Ticket:** CFA-92 (EPIC 3 / PERF-01 „Release-Binary-Benchmark“). Dieser
Abschnitt misst erstmals die **ausgelieferte Release-Binary** (`[profile.release]`,
`optimized`) statt des Debug-Testprofils der Abschnitte CFA-35/CFA-36. Er trennt
strikt zwischen dem **Anwendungsprozess** (von außen über `/proc/<pid>` und den
Pty-Ausgabestrom gemessen) und dem **Harness-/Testprozess** (im `#[ignore]`-Test
gemessen). Beide werden **nie** zu einer Zahl vermischt. Die DRM-fd-Scan-
Optimierung aus CFA-36 wird **nicht** erneut durchgeführt.

### Messobjekt, Trennung und „kein echter Inferenzserver“

- **Anwendungsprozess:** `target/release/orsiktop`, gestartet auf einem eigenen
  Pty (160×48). Alle CPU-, RSS-, Thread-, Frame- und Latenzwerte stammen aus
  `/proc/<app-pid>/{stat,status,task}` bzw. aus dem rohen Pty-Byte-Strom und
  beschreiben **ausschließlich** die Anwendung.
- **Mock-Server:** je Szenario 1/4/16 llama.cpp-artige Loopback-HTTP-Mocks, die
  in **eigenen Prozessen** laufen; ihre CPU wird nie als Anwendungskosten
  gezählt. „Idle ohne Mock“ pollt einen geschlossenen Loopback-Port.
- **Erzwungene Isolation:** Die Anwendung läuft mit einer isolierten
  `XDG_CONFIG_HOME` und `auto_discovery=false`, die Konfiguration setzt den
  Endpunkt auf den jeweiligen Mock. Der auf dem Host real laufende
  `llama-server` wird daher **nicht** entdeckt und **nicht** kontaktiert; es
  werden keine echten Inferenzserver befragt. `ORSIKTOP_*`- und Proxy-
  Umgebungsvariablen werden entfernt.

### Herkunft jeder Metrik (Trennung Anwendung vs. Harness)

| Metrik | Quelle | Klasse |
| --- | --- | --- |
| Prozess-CPU (gesamt + je Thread) | `/proc/<app-pid>/stat`, `/task/*/stat` | Anwendung |
| RSS / VmHWM | `/proc/<app-pid>/status` | Anwendung |
| Thread-Anzahl | `/proc/<app-pid>/status` | Anwendung |
| Renderhäufigkeit / Frame-Intervall | Hide-Cursor-Escape (`ESC[?25l`) je `Terminal::draw` auf der Pty | Anwendung |
| Eingabelatenz (Taste → Frame) | Zeitstempel im Runner zwischen Pty-Write und nächstem Frame | Anwendung (Pty) |
| LLM-Poll-Zyklus (Periode, Fan-out) | Request-Log der Mock-Prozesse | Harness (Netzseite) |
| Startdauer (erste Ausgabe / erstes Frame) | Zeitstempel ab `exec` bis erstes Pty-Byte / Frame 2 | Anwendung |
| **Prozesskollektor-Laufzeit** | `ORSKTOP_BENCH` im `#[ignore]`-Release-Test | **Harness-/Testprozess — getrennt** |

### Reproduzierbarer Runner

`scripts/perf_release_bench.py` (nur Python-Standardbibliothek; Linux, `/proc`,
`pty`). Erzeugt `ORSKTOP_RELEASE_BENCH|<szenario>#<rep>|<key>=<value>`-Rohzeilen
und schreibt ein JSON nach `target/perf-bench/` (`target/` ist gitignored, die
aggregierten Rohwerte stehen deshalb unten im Dokument).

```bash
cargo build --release --locked
python3 scripts/perf_release_bench.py \
  --scenarios idle_no_mock mock_1 mock_4 mock_16 \
              mock_1_refresh_100 mock_1_refresh_250 mock_1_refresh_5000 \
              large_proc_list_mock_1 \
  --duration 30 --warmup 5 --input-reps 40 --repeat 3 --out target/perf-bench
python3 scripts/perf_release_bench.py \
  --scenarios soak_mock_4 --soak-duration 600 --warmup 5 --repeat 2 \
  --out target/perf-bench
# Harness-/Testprozess, getrennt vom Anwendungsprozess:
cargo test --release --all-targets --locked -- --ignored --test-threads=1 \
  perf_baseline_soak_and_process_collection_benchmark
```

### Messbedingungen (belegt)

| Feld | Wert | Quelle |
| --- | --- | --- |
| Host | `test-laptop` | `hostname` (Runner) |
| CPU | Intel(R) Core(TM) Ultra 9 288V, **8 logische CPUs** (kein SMT) | `/proc/cpuinfo`, `lscpu` |
| RAM | 32 319 768 kB (≈ 30,8 GiB) | `host_mem_total_kb` |
| Kernel | `7.2.5-3-omarchy` (`Linux test-laptop … x86_64 GNU/Linux`) | `uname -a` |
| Toolchain | `rustc 1.100.0-nightly (feaadeeac 2026-09-19)`, `cargo 1.100.0-nightly` | `rustc -V` / `cargo -V` |
| CPU-Frequenz-Governor | `powersave`; EPP `performance`; Netzbetrieb (`ac_online=1`) | Runner |
| Quellstand | HEAD `536e424`, `git_dirty=false` | Runner |
| **Build-Profil** | **`release` (optimized)** | `Finished \`release\` profile [optimized]` |
| Last (wichtig!) | **Geteilter Laptop unter Fremdlast:** parallel liefen mehrere `rustc`/`clippy`-Prozesse anderer Agenten (Last…1 min ≈ **10–20** auf 8 CPUs) sowie ein realer `llama-server` (~180 % CPU). | `loadavg_start/end`, `ps` |

**Konsequenz:** Die Last liegt während der Messung **deutlich über** der
CPU-Zahl. Die Absolutwerte (Frame-Zeiten, CPU-Prozentsätze, Eingabelatenz) sind
daher **maschinen- und lastabhängige Referenzwerte**, kein sauberer
Ruhe-Baseline-Lauf. Zur Belastbarkeit dient die **Spanne über Wiederholungen**;
aus diesem Grund wurde jedes Szenario **dreimal** (Soak: zweimal) gemessen. Eine
Wiederholung auf einem unbelasteten Host steht aus (offener Punkt).

### Rohdaten — Anwendungsprozess (Release, 3 Wiederholungen)

Angegeben ist **Mittelwert [Min–Max]** über die Wiederholungen. `—` = nicht
zutreffend (kein Mock). CPU in **% einer logischen CPU**, RSS/VmHWM in **MB**,
Poll-Zyklus in **ms**.

| Szenario | SRV | refresh ms | Dummy-Proz. | CPU % | main % | fast % | llm % | Frames/s | Eing. ms | RSS MB | VmHWM MB | Thr. | Zyklus ms | Req/s | Ctx/s | Host-P. |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| idle_no_mock | 0 | 1000 | 0 | 22,28 [21,81–22,64] | 0,64 | 21,53 | 0,01 | 4,82 [4,76–4,86] | 4,22 [3,85–4,47] | 17,27 [17,19–17,32] | 31,14 [30,99–31,27] | 5,01 | — | — | 27,5 | 514 |
| mock_1 | 1 | 1000 | 0 | 19,84 [15,28–22,93] | 0,63 | 19,09 | 0,03 | 4,93 [4,90–4,96] | 3,93 [2,31–5,11] | 17,25 [16,96–17,42] | 31,14 [31,02–31,27] | 5,02 | 1000,19 | 2,08 | 26,6 | 520 |
| mock_4 | 4 | 1000 | 0 | 21,68 [17,11–26,38] | 0,73 | 20,68 | 0,03 | 5,16 [4,99–5,30] | 5,46 [2,64–7,93] | 18,13 [17,96–18,26] | 31,79 [31,68–31,90] | 8,05 | 1000,20 | 8,39 | 27,1 | 521 |
| mock_16 | 16 | 1000 | 0 | 22,16 [18,88–26,19] | 0,72 | 20,48 | 0,06 | 5,23 [5,13–5,32] | 4,31 [2,32–6,23] | 20,56 [20,41–20,75] | 33,94 [33,76–34,05] | 20,36 | 1000,26 | 33,55 | 25,4 | 515 |
| mock_1_refresh_100 | 1 | 100 | 0 | 27,19 [20,30–31,23] | 1,27 | 25,58 | 0,04 | 9,41 [9,02–10,00] | 3,60 [1,82–4,74] | 17,42 [17,20–17,65] | 31,06 [30,79–31,21] | 5,07 | 250,19 | 8,07 | 31,7 | 514 |
| mock_1_refresh_250 | 1 | 250 | 0 | 26,57 [19,52–30,37] | 0,81 | 25,43 | 0,04 | 6,32 [5,76–6,80] | 4,33 [2,28–6,04] | 17,39 [17,32–17,47] | 31,12 [31,05–31,22] | 5,06 | 250,22 | 8,06 | 27,4 | 522 |
| mock_1_refresh_5000 | 1 | 5000 | 0 | 7,59 [5,19–8,89] | 0,50 | 7,02 | 0,03 | 4,20 [4,16–4,23] | 3,71 [2,00–5,11] | 16,88 [16,78–17,02] | 31,16 [30,95–31,31] | 5,00 | 5000,05 | 0,47 | 23,5 | 527 |
| large_proc_list_mock_1 | 1 | 1000 | 2000 | 23,90 [15,79–28,33] | 0,87 | 22,86 | 0,01 | 4,90 [4,80–5,00] | 6,14 [2,82–8,32] | 22,17 [20,95–23,18] | 31,14 [31,04–31,30] | 5,02 | 1000,16 | 2,05 | 27,3 | 2511 |

Erläuterungen zu den Spalten:

- **fast %** ist der Fast-Worker (Prozesskollektor + System/GPU-Sampling),
  **main %** der UI-/Render-Thread (`tid == pid`), **llm %** der LLM-Worker.
  In \(app::run\) wird der Fast-Worker **vor** dem LLM-Worker gestartet, daher
  ist `thread_1` = Fast-Worker und `thread_2` = LLM-Worker (Reihenfolge, nicht
  geraten — siehe `src/app.rs:195-210`). Der Fast-Worker trägt **den gesamten**
  CPU-Aufwand: z. B. `idle_no_mock` 21,53 von 22,28 % ≈ **96 %**.
- **Threads**: die Anwendung startet mit **5** Threads; je Mock-Server kommt
  **genau ein** Thread hinzu (1/4/16 → 5/8/20). Der Runner selbst und die Mocks
  sind separate Prozesse und zählen nicht.
- **Req/s** sind die tatsächlich vom Mock gesehenen HTTP-Requests (je Zyklus
  `/metrics` **und** `/slots`, also ≈ 2 × Zyklen/s). Der Zyklus folgt dem
  wirksamen Poll-Intervall: 100 ms und 250 ms laufen beide mit **250 ms**
  (Untergrenze des Pollings in `src/app.rs:1454`), 1000 ms und 5000 ms
  unverändert. Bei `mock_16` ergeben 16 Server × 2 Requests ≈ 32 Req/s.
- **Frames/s**: Leerlauf-Redraws liegen bei **~4,8/s** (idle-Tick 4/s plus
  Fast-Snapshots); bei `refresh_ms=100` steigen sie auf **~9,4/s** (Fast-Worker
  sendet alle 100 ms einen Snapshot). Bei `refresh_ms=5000` bleibt es bei
  ~4,2/s (idle-Tick). Die Redraw-Rate folgt also dem Fast-Worker-Takt, nicht der
  Modellgröße — und sie ist hier **direkt am echten Terminal gemessen**, nicht
  modelliert.
- **CPU** korreliert klar mit der **wirksamen Kollektor-Frequenz**: bei
  `refresh_ms=5000` läuft der Prozesszyklus nur alle 5 s (die
  `process_refresh_ms=1000`-Prüfung liegt **innerhalb** der `refresh_ms`-
  Schleife, `src/app.rs:926-942` und `sleep_until_next_cycle`, `src/app.rs:1038`),
  daher fällt die CPU von ~22 % auf ~7,6 %.
- **large_proc_list_mock_1**: 2000 zusätzliche `sleep`-Prozesse erhöhen die
  Host-Prozesszahl auf ~2500 und den RSS der Anwendung leicht (mehr
  Prozesszeilen), die CPU der Anwendung bleibt aber im Bereich der übrigen
  Szenarien (der Kollektor skaliert mit **Prozess-/fd-Zahl des Hosts**, s. u.).

### Rohdaten — Startdauer (Release)

Zeit ab `exec` bis zum ersten Pty-Byte bzw. bis zum zweiten Frame-Marker; für
Mock-Szenarien zusätzlich bis der Modellname (`benchmodel-q4`) auf dem Schirm
erscheint.

| Szenario | erste Ausgabe ms | erstes Frame ms | Modell sichtbar ms |
| --- | --- | --- | --- |
| idle_no_mock | 3,26 [1,76–4,35] | 5,03 [3,79–5,76] | — |
| mock_1 | 3,10 [2,35–3,84] | 6,35 [5,93–7,08] | 77,88 [58,46–112,80] |
| mock_4 | 3,23 [1,29–6,54] | 5,52 [2,81–8,80] | 73,53 [53,39–109,86] |
| mock_16 | 4,39 [1,17–10,47] | 7,82 [2,65–16,82] | 91,94 [53,19–116,93] |
| mock_1_refresh_100 | 1,76 [1,15–2,90] | 4,00 [2,71–6,47] | 54,72 [53,24–57,57] |
| mock_1_refresh_250 | 1,79 [0,80–2,92] | 3,99 [2,01–6,68] | 54,81 [52,48–57,33] |
| mock_1_refresh_5000 | 1,55 [1,21–2,19] | 3,15 [2,57–3,99] | 53,83 [53,17–54,96] |
| large_proc_list_mock_1 | 1,56 [0,95–2,35] | 2,85 [2,20–3,48] | 71,59 [52,69–108,63] |

Die Anwendung ist bis zur **ersten Ausgabe in ~1,2–4,4 ms** und bis zum
**ersten Frame in ~2,7–8,8 ms** da (Release, hoch ausgelasteter Host). „Modell
sichtbar“ ist die Zeit bis zum **ersten erfolgreichen LLM-Sample** inklusive
HTTP-Roundtrip zu 1/4/16 Mocks (~54 ms bei 1 Server bis ~92 ms bei 16). Das ist
**nicht** der vollständige Programmstart: die GPU-/NVML-/Sensor-Discovery läuft
im Fast-Worker **nebenläufig** und ist in diesen Zahlen nicht garantiert
abgeschlossen (s. „Nicht gemessen“).

### Rohdaten — längerer stabiler Betrieb (Soak, `soak_mock_4`, 4 Mocks, 600 s)

| Wdh. | Frames/s | CPU % | RSS erste 10 s MB | RSS letzte 10 s MB | VmHWM MB | Thr. | Eing. ms | Zyklus ms | Host-P. | Last 1 min (Start → Ende) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 5,15 | 24,59 | 17,68 | 20,23 | 31,73 | 8,06 | 4,06 | 1000,25 | 469 | 16,16 → 20,07 |
| 1 | 5,05 | 24,23 | 17,45 | 19,28 | 31,40 | 8,06 | 2,15 | 1000,23 | 480 | 19,35 → 4,22 |

Über 10 Minuten bleiben Frames/s, CPU und Zyklus **stabil** (keine Zunahme der
Redraw- oder Poll-Frequenz). **RSS wächst leicht** (≈ +2,0/+1,8 MB in 10 min);
das ist eine **Beobachtung**, kein belegtes Leck — die Host-Last änderte sich
zwischen Start und Ende (s. Spalte), und HWM enthält bereits den Startpeak. Der
Wert verdient bei einem echten Mehrstunden-Soak (EPIC 4.1) Nachverfolgung.

### Rohdaten — Harness-/Testprozess (getrennt!) — Prozesskollektor in `release`

Aus dem `#[ignore]`-Release-Test (`-- --ignored --test-threads=1`), der **im
Testprozess** die reale `/proc`+DRM-Sammlung ausführt. Diese Zahlen gehören zum
**Testprozess**, nicht zur TUI und werden nicht mit den Anwendungswerten oben
verrechnet.

```
ORSKTOP_BENCH|process_count=1425
ORSKTOP_BENCH|process_refresh_mean_us=23140
ORSKTOP_BENCH|process_collect_mean_us=237082
ORSKTOP_BENCH|process_collect_max_us=238430
ORSKTOP_BENCH|process_cycle_mean_us=260223
ORSKTOP_BENCH|process_cycle_max_us=283985
ORSKTOP_BENCH|process_threads_read_total_us=8191
ORSKTOP_BENCH|process_threads_read_failures=0
ORSKTOP_BENCH|process_drm_scan_total_us=233898
ORSKTOP_BENCH|process_drm_procs_with_gpu=148
ORSKTOP_BENCH|soak_rss_kb_at_1s=9224
ORSKTOP_BENCH|soak_rss_kb_at_10s=9224
ORSKTOP_BENCH|soak_threads_at_1s=2
ORSKTOP_BENCH|soak_threads_at_10s=2
ORSKTOP_BENCH|soak_frames_total=28920
ORSKTOP_BENCH|soak_window_s=10
```

Auch im Release-Profil ist der **DRM-fd-Scan mit 233 898 µs ≈ 89,9 %** die
dominierende Komponente des Kollektorzyklus (260 223 µs), der Thread-Zähler nur
8 191 µs (~3 %). `process_count=1425`; der Testprozess selbst hat konstant
**9 224 kB RSS** und 2 Threads.

### Rohdaten — Beispiel eines vollständigen Rohblocks (Anwendung)

`idle_no_mock#0` (gekürzt auf die Kernfelder; vollständige Blöcke je Szenario
und Wiederholung über den Runner reproduzierbar):

```
ORSKTOP_RELEASE_BENCH|host|cpu_model=Intel(R) Core(TM) Ultra 9 288V
ORSKTOP_RELEASE_BENCH|host|logical_cpus=8
ORSKTOP_RELEASE_BENCH|host|mem_total_kb=32319768
ORSKTOP_RELEASE_BENCH|host|kernel=7.2.5-3-omarchy
ORSKTOP_RELEASE_BENCH|host|rustc=rustc 1.100.0-nightly (feaadeeac 2026-09-19)
ORSKTOP_RELEASE_BENCH|host|git_head=536e424
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|host_processes=514
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|loadavg_start=14.71 8.83 5.84 21/1597 235143
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|startup_first_output_ms=1.755
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|startup_first_frame_ms=3.787
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|cpu_pct_one_cpu=22.64
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|cpu_pct_by_thread.main_ui=0.732
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|cpu_pct_by_thread.thread_1=21.808
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|cpu_pct_by_thread.thread_2=0.0
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|cpu_pct_by_thread.exited_or_new=0.1
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|frames_per_s=4.861
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|rss_kb.mean=17740.793
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|vm_hwm_kb=32024
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|threads.mean=5.015
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|input_to_frame_ms.mean=4.468
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|input_to_frame_ms.p95=8.801
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|exit_code=0
ORSKTOP_RELEASE_BENCH|idle_no_mock#0|shutdown_ms=220.154
```

### Interpretation (was die Release-Messung zeigt)

- **Der Prozesskollektor ist der dominierende Kostenblock der Anwendung.** Der
  Fast-Worker trägt den Großteil der Anwendungs-CPU (über alle Läufe 90–97 %,
  Median ~95 %); main- und LLM-Thread liegen
  zusammen meist unter 1 %. Das deckt sich mit dem getrennt gemessenen
  Kollektorzyklus (260 ms bei 1425 Prozessen, davon ~90 % DRM-fd-Scan). Die
  **Refresh-Intervalle** wirken vor allem indirekt: Sie bestimmen den Takt, in
  dem der Kollektor läuft (`refresh_ms=5000` → ~7,6 % statt ~22 %).
- **Polling und Rendering sind im Release klein.** Die 1/4/16 Mocks verändern
  die Anwendungs-CPU kaum (fast-Anteil bleibt ~20 %); der LLM-Worker bleibt bei
  ~0,01–0,06 % einer CPU. Die Renderlast ist bei 4–10 Frames/s (< 1 % CPU)
  vernachlässigbar.
- **Der Speicher ist niedrig und stabil:** RSS ~17–22 MB, VmHWM ~31–34 MB; je
  Mock-Server genau ein zusätzlicher Thread.
- **Eingabelatenz (Taste → Frame):** Mittel ~2–6 ms, p95 bis ~8 ms — auf einem
  ausgelasteten Host und damit nur als Größenordnung belastbar.

### Nicht gemessen (ausdrücklich offen)

- **Vollständiger Programmstart inkl. GPU-/NVML-/Sensor-Discovery.** Der
  Fast-Worker startet die Discovery **nebenläufig**; „erstes Frame“ garantiert
  nicht, dass sie abgeschlossen ist. Gemessen ist nur bis zum ersten Frame.
- **Client-seitige LLM-Poll-Latenz (Request → geparster Sample) im Release.**
  Von außen messbar ist nur der **Zyklus** (Mock-Request-Zeitpunkte) und die
  Rechenzeit des LLM-Workers. Die Mock-eigene Servicezeit ist Harness-Kosten und
  wird nicht als Client-Latenz ausgegeben.
- **CPU-Spitzen pro Frame** liegen unter der 10-ms-Jiffy-Auflösung und werden
  deshalb nicht berichtet.
- **GPU-Rechnerkosten** (NVML/DRM) sind Teil des Fast-Workers, aber nicht
  getrennt von der `refresh_ms`-Kadenz isoliert.
- **Ruhe-Baseline ohne Fremdlast** fehlt (dieser Lauf war lastbehaftet).
- Nur **eine** Maschine (test-laptop, Core Ultra 9 288V); die Werte sind
  maschinenspezifisch. Transport ist **Loopback**, kein reales Netz.

## CFA-93 — Messbasierte Optimierung: Entscheid (mit Zahlen)

**Ticket:** CFA-93 (EPIC 3 / PERF-02). Auftrag: nur einen durch PERF-01
**reproduzierbar bestätigten Engpass** optimieren, vorher/nachher gemessen, ohne
Verlust der Telemetrie-Aktualität — andernfalls die Entscheidung mit Zahlen
dokumentieren (gültiger Abschluss).

**Befund (CFA-92):** Der einzige reproduzierbare Engpass ist der
**Prozesskollektor im Fast-Worker**, und darin der **DRM-fd-Scan**
(89,9 % des Zyklus, s. o.). Genau dieser ist **bereits in CFA-36 optimiert**
worden (~10 % gepaarter Gewinn durch Vermeidung von Allokationen um den
`readlink`-Syscall). Der verbleibende, **je fd anfallende `readlink`-Syscall**
ist ohne Caching nicht reduzierbar; Caching (fd→DRM-Menge seltener neu scannen)
würde die Telemetrie-Aktualität verschlechtern und verstößt gegen das
Abnahmekriterium.

**Entscheidung: keine weitere Code-Änderung in diesem Lauf** (gültiger
Abschluss nach Aufgabenstellung). Begründung mit Zahlen:

1. Der Engpass ist **bestätigt**, aber die dominierende Komponente ist bereits
   optimiert (CFA-36); die Restkosten sind der Syscall selbst.
2. Die verbleibende kandidatische Mikro-Optimierung (die `String`-Allokation je
   fd-Name in `Sys::read_dir_names`, `src/system.rs:79-87`) liegt nach der
   CFA-36-Analyse im **einstelligen Prozentbereich** des Scans und damit **unter
   der Messstreuung dieses lastbehafteten Hosts** (Anwendungs-CPU z. B.
   `mock_4` 17,1–26,4 %; der Kollektor selbst schwankt über Läufe stark). Ein
   belastbarer Vorher/Nachher-Beleg ist hier nicht führbar.
3. Kein **pauschales Caching** (verboten); keine Verschlechterung der
   Aktualität; die Semantik bleibt unverändert.

Damit wird **kein Performancegewinn behauptet**. CFA-93 ist als
**dokumentierte Entscheidung** abgeschlossen; ein erneuter Optimierungsanlauf
lohnt nur mit einem **lastfreien** Messfenster (dann ist auch die fd-Allokation
prüfbar) oder einem Messpunkt für den Client-Poll.

## Querverweise

- `src/app.rs` — `RedrawScheduler`, `simulated_idle_redraws`, die zwei
  Benchmark-Tests; Fast-Worker/Prozesskollektor (`spawn_fast_worker`,
  `collect_process_stats`).
- `src/llama.rs` — `LlamaMonitor::sample` (Polling-Pfad).
- `docs/TELEMETRY_VERIFICATION.md` — Telemetrie-Semantik (CFA-33).
- `docs/LIVE_VS_FIXTURE.md` — Live-/Fixture-Evidenzstufen.
