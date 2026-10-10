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
| Host | `cf-desktop` | `result.json` / `worker-info` |
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
`/home/chris/.local/share/paperclip-app/heavy-checks/157f3e387f3f46108d4196d5b19a5007`,
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

| Metrik | Baseline (cf-desktop, Debug-Profil) | Art |
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
- Nur **eine** Hardware (cf-desktop, i9-12900K); die Zahlen sind
  maschinenspezifisch.
- Transport ist **localhost-Mock**, nicht ein realer Netzwerk-Server.
- `BENCH_CLK_TCK = 100` wird angenommen (Linux-Standard), nicht geprobt.
- Der Benchmark misst die **synchronen** Laufzeitpfade im Testprozess, nicht die
  vollständige `app::run`-TUI-Schleife mit Crossterm-Backend.

## Scope und Änderungen

Geändert wurde ausschließlich Testcode in `src/app.rs` (zwei Benchmark-Tests +
Helfer, nur `mod tests`) sowie diese neue Dokumentation. Keine Produktionslogik,
keine Konfiguration, `src/strata.rs` unverändert. Die Messungen liefen
ausschließlich über das freigegebene cf-desktop-Gate (`pc-project cargo test`),
mit Host, Toolchain, Snapshot-Hash, Exitcode und Rohlog belegt.

## Querverweise

- `src/app.rs` — `RedrawScheduler`, `simulated_idle_redraws`, die zwei
  Benchmark-Tests.
- `src/llama.rs` — `LlamaMonitor::sample` (Polling-Pfad).
- `docs/TELEMETRY_VERIFICATION.md` — Telemetrie-Semantik (CFA-33).
- `docs/LIVE_VS_FIXTURE.md` — Live-/Fixture-Evidenzstufen.
