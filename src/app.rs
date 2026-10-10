use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Stdout,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use std::path::PathBuf;

use crate::system::{RealSys, Sys};
use crate::{
    config,
    cpu::{detect_cpu_topology, CpuTopology},
    cpu_sensors::CpuSensors,
    discovery::discover_gpus,
    discovery_llm::LocalServerIdentity,
    domain::{
        DashboardSnapshot, FastSnapshot, GpuMapping, GpuSelector, ProcessIdentity, ProcessStats,
        ServerSummary, SystemStats, MAX_MONITORED_SERVERS, MAX_REFRESH_MS, MIN_LLM_POLL_MS,
        MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
    drm::{sample_process_gpus, DrmSamplerState},
    gpu::new_gpu_provider,
    gpu_map::{map_server_gpus, nvml_compute_gpus, process_render_gpus},
    llama::{LlamaMonitor, LlmStats},
    providers::nvidia::discover_mig_children,
    ui::{self, UiState},
};
use nvml_wrapper::Nvml;
const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
/// S12: CPU temperature and power are slow-changing; they are sampled on this
/// slower cadence (not the 250 ms system cadence) and reused in between.
/// Power additionally blocks ~50 ms for the RAPL two-read window. Frequency
/// stays on the fast cadence — it is cheap and changes quickly.
const SLOW_SENSOR_REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);
/// How often auto-discovery re-resolves the local server's endpoint/PID while
/// the TUI runs (only in auto-discovery mode). A llama.cpp restart or exit is
/// reflected within at most this interval; the endpoint/PID are resolved with
/// a single `/proc` scan, never per UI frame. Manual endpoints are static, so
/// no scan happens for them.
const SERVER_RESYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum time the render loop may go without repainting when nothing changed.
///
/// The loop used to repaint in every iteration, and an idle iteration was
/// bounded by `event::poll(50 ms)` — about 20 full redraws per second with no
/// new data. A repaint now happens only when the visible state changed or when
/// this tick elapses. The tick is kept because parts of the view are
/// wall-clock-relative ("{age} ago", uptime) and the transient CONNECTED flash
/// must be able to expire; 250 ms bounds that staleness while an idle dashboard
/// drops to ~4 repaints/s.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);

/// Upper bound on how long `event::poll` may block before the loop re-checks
/// its channels. This preserves the original ~50 ms data-drain latency, so a
/// configured 100 ms refresh still reaches the screen promptly; only the
/// repaint itself is paced, not the collection cadence.
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Paces repaints for the render loop.
///
/// A repaint is requested (`mark_dirty`) whenever an applied snapshot/sample or
/// an input event can change what is on screen; `should_paint` also fires once
/// per `interval` so time-relative text keeps advancing while idle. Keeping the
/// decision in one place is what makes the idle redraw rate measurable without
/// a terminal.
struct RedrawScheduler {
    dirty: bool,
    next_tick: Instant,
    interval: Duration,
}

impl RedrawScheduler {
    fn new(now: Instant, interval: Duration) -> Self {
        Self {
            dirty: true,
            next_tick: now,
            interval,
        }
    }

    /// Request a repaint at the next opportunity (state or input changed).
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether the view must be painted at `now`. Advances the idle tick on
    /// every paint so a stationary dashboard repaints at a bounded rate.
    fn should_paint(&mut self, now: Instant) -> bool {
        if self.dirty || now >= self.next_tick {
            self.dirty = false;
            self.next_tick = now + self.interval;
            true
        } else {
            false
        }
    }

    /// How long `event::poll` may block. A dirty view returns zero so the
    /// repaint is not delayed; a clean view waits for the next idle tick but
    /// never longer than `MAX_POLL_INTERVAL`, so newly arrived samples are
    /// still drained promptly. Input wakes the poll immediately either way.
    fn poll_timeout(&self, now: Instant) -> Duration {
        if self.dirty {
            Duration::ZERO
        } else {
            self.next_tick
                .saturating_duration_since(now)
                .min(MAX_POLL_INTERVAL)
        }
    }
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    // Stable identity (PID + start time) of the discovered server process
    // (S16), when present.
    server_identity: Option<LocalServerIdentity>,
    initial_settings: config::AppConfig,
    // True when `server` was resolved via local auto-discovery.
    server_auto: bool,
    // Full set of servers to monitor, in display order (configured + locally
    // discovered), already de-duplicated, plus any configured endpoints that
    // exceed the monitoring cap.
    servers: crate::ServerList,
    // Stable key of the server selected at startup.
    default_selected: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Endpoint, local server identity and origin are tracked together but
    // compared independently: a stable URL can still get a new identity, and
    // the origin can flip while the URL stays the same (see `reconcile_server`).
    let mut resolved = crate::ResolvedServer {
        endpoint: server.to_string(),
        identity: server_identity,
        auto: server_auto,
    };
    let mut settings = initial_settings.sanitized();
    let mut refresh_ms = settings.refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);
    settings.refresh_ms = refresh_ms;
    let refresh_shared = Arc::new(AtomicU64::new(refresh_ms));
    let process_refresh_shared = Arc::new(AtomicU64::new(settings.process_refresh_ms));
    let offline_grace_shared = Arc::new(AtomicU64::new(settings.offline_grace_ms));
    let gpu_selector_shared = Arc::new(RwLock::new(settings.gpu_selector.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let (fast_tx, fast_rx) = mpsc::sync_channel::<FastSnapshot>(2);
    // One sample per server per cycle must fit without being dropped, so the
    // bound matches the server cap rather than the single-server size.
    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
    // Server-set changes are carried with a monotonically increasing
    // generation so a result produced for a previous set can be rejected
    // instead of being shown under the new one (see `sync_server_set`).
    let (server_tx, server_rx) = mpsc::channel::<LlmCommand>();
    let (server_identity_tx, server_identity_rx) =
        mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
    let mut llm_generation: u64 = 0;
    let mut mapping_generation: u64 = 0;

    // Multi-server registry: the full endpoint set to monitor, keyed by stable
    // identity, in a deterministic display order. `resolved` above describes
    // the primary local server (endpoint/auto label); the GPU association
    // follows the *selected* server's verified local process identity.
    let mut server_list = servers.monitored;
    // Endpoints past the cap: shown as "limit reached", never polled.
    let mut overflow: Vec<ServerSpec> = servers.overflow;
    let mut order: Vec<String> = server_list.iter().map(|spec| spec.key.clone()).collect();
    let mut endpoints: BTreeMap<String, String> = server_list
        .iter()
        .map(|spec| (spec.key.clone(), spec.endpoint.clone()))
        .collect();
    let mut registry: BTreeMap<String, LlmStats> = BTreeMap::new();
    let mut selected = if endpoints.contains_key(&default_selected) {
        default_selected
    } else {
        order.first().cloned().unwrap_or_default()
    };
    // Attribute the initially selected server from its verified process
    // identity. A remote/configured server has no identity => Unknown.
    publish_selected_mapping(
        selected_identity_of(&server_list, &selected),
        &mut mapping_generation,
        &server_identity_tx,
    );

    let fast_worker = spawn_fast_worker(
        Arc::clone(&gpu_selector_shared),
        Arc::clone(&process_refresh_shared),
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        fast_tx,
        server_identity_rx,
    );
    let llm_worker = spawn_llm_worker(
        server_list.clone(),
        Arc::clone(&refresh_shared),
        Arc::clone(&offline_grace_shared),
        Arc::clone(&stop),
        llm_tx,
        server_rx,
    );

    let mut snapshot = DashboardSnapshot::default();
    let mut ui_state = UiState::default();
    let mut last_server_resync = Instant::now();
    let mut scheduler = RedrawScheduler::new(Instant::now(), IDLE_REDRAW_INTERVAL);

    loop {
        // Each applied fast snapshot can change the overview; repaint on the
        // next iteration rather than waiting for the idle tick. Fast snapshots
        // have no rejection path — only LLM samples are filtered (below).
        while let Ok(next) = fast_rx.try_recv() {
            apply_fast_snapshot(&mut snapshot, &mut ui_state, mapping_generation, next);
            scheduler.mark_dirty();
        }
        while let Ok(next) = llm_rx.try_recv() {
            // Drop results produced for a server set we already replaced, or
            // for a server that is no longer known: they must not appear under
            // a different server's state.
            if !accepts_llm_sample(llm_generation, &next) {
                continue;
            }
            if !endpoints.contains_key(&next.server) {
                continue;
            }
            let is_selected = next.server == selected;
            // Record for every server (background servers keep accumulating),
            // mirroring into the active view only for the selected server.
            ui_state.record_llm_sample(&next.server, &selected, &next.stats);
            registry.insert(next.server, next.stats.clone());
            if is_selected {
                snapshot.llm = next.stats;
            }
            // An accepted sample changes the overview and/or the active view.
            scheduler.mark_dirty();
        }
        snapshot.servers = build_server_summaries(&order, &endpoints, &registry);
        snapshot.selected_server = selected.clone();

        // S16: an auto-discovered server is dynamic — it can restart on the
        // same port (new PID) or exit entirely during a run. Re-resolve on a
        // bounded cadence so the local GPU mapping and the auto label cannot
        // stay pinned to a stale PID. Manual/remote endpoints are static, so
        // this is skipped for them. The UI thread never scans `/proc` per
        // frame; the worst-case staleness is `SERVER_RESYNC_INTERVAL`.
        if settings.auto_discovery && last_server_resync.elapsed() >= SERVER_RESYNC_INTERVAL {
            let next = crate::resolve_monitor_target(&settings);
            update_resolved_server(&mut resolved, next);
            sync_server_set(
                &settings,
                &mut server_list,
                &mut overflow,
                &mut order,
                &mut endpoints,
                &mut registry,
                &mut selected,
                &mut llm_generation,
                &mut mapping_generation,
                &server_tx,
                &server_identity_tx,
                &mut snapshot,
                &mut ui_state,
            );
            last_server_resync = Instant::now();
            // A resync can add/remove/replace a monitored server.
            scheduler.mark_dirty();
        }

        if scheduler.should_paint(Instant::now()) {
            terminal.draw(|frame| {
                ui::draw(
                    frame,
                    &snapshot.system,
                    &snapshot.llm,
                    &snapshot.gpu,
                    &snapshot.gpu_map,
                    &snapshot.servers,
                    &snapshot.selected_server,
                    &overflow,
                    &mut ui_state,
                    &resolved.endpoint,
                    refresh_ms,
                    resolved.auto,
                )
            })?;
        }

        if event::poll(scheduler.poll_timeout(Instant::now()))? {
            // Any input/resize can change the rendered view; let the next
            // iteration repaint promptly instead of waiting for the idle tick.
            scheduler.mark_dirty();
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Repeat
                        && !ui_state.is_help_open()
                        && !ui_state.is_settings_open()
                        && !ui_state.is_process_search_open()
                        && !ui_state.is_server_selector_open() =>
                {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            ui_state.move_process_selection(-1, &snapshot.system.processes);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            ui_state.move_process_selection(1, &snapshot.system.processes);
                        }
                        KeyCode::PageUp => {
                            ui_state.move_process_selection(-10, &snapshot.system.processes);
                        }
                        KeyCode::PageDown => {
                            ui_state.move_process_selection(10, &snapshot.system.processes);
                        }
                        _ => {}
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && ui_state.is_settings_open() =>
                {
                    match key.code {
                        KeyCode::Esc => ui_state.close_settings(),
                        KeyCode::Tab | KeyCode::Down => ui_state.settings_next_field(),
                        KeyCode::BackTab | KeyCode::Up => ui_state.settings_previous_field(),
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                            ui_state.settings_toggle_selected();
                        }
                        KeyCode::Backspace => ui_state.settings_backspace(),
                        KeyCode::Enter => match ui_state.settings_config() {
                            Ok(next_settings) => match config::save(&next_settings) {
                                Ok(()) => {
                                    settings = next_settings;
                                    refresh_ms = settings.refresh_ms;
                                    refresh_shared.store(refresh_ms, Ordering::Relaxed);
                                    process_refresh_shared
                                        .store(settings.process_refresh_ms, Ordering::Relaxed);
                                    offline_grace_shared
                                        .store(settings.offline_grace_ms, Ordering::Relaxed);
                                    *gpu_selector_shared
                                        .write()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        settings.gpu_selector.clone();
                                    // Re-resolve every field independently:
                                    // a settings edit can change the PID or the
                                    // auto/manual origin without changing the
                                    // endpoint (and vice versa).
                                    let next = crate::resolve_monitor_target(&settings);
                                    update_resolved_server(&mut resolved, next);
                                    sync_server_set(
                                        &settings,
                                        &mut server_list,
                                        &mut overflow,
                                        &mut order,
                                        &mut endpoints,
                                        &mut registry,
                                        &mut selected,
                                        &mut llm_generation,
                                        &mut mapping_generation,
                                        &server_tx,
                                        &server_identity_tx,
                                        &mut snapshot,
                                        &mut ui_state,
                                    );
                                    last_server_resync = Instant::now();
                                    ui_state.close_settings();
                                }
                                Err(err) => ui_state
                                    .set_settings_error(format!("Could not save settings: {err}")),
                            },
                            Err(err) => ui_state.set_settings_error(err),
                        },
                        KeyCode::Char(ch) => ui_state.settings_insert_char(ch),
                        _ => {}
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && ui_state.is_process_search_open() =>
                {
                    match key.code {
                        KeyCode::Esc => {
                            ui_state.clear_process_search();
                            ui_state.clamp_process_selection(&snapshot.system.processes);
                        }
                        KeyCode::Enter => ui_state.accept_process_search(),
                        KeyCode::Backspace => {
                            ui_state.process_search_backspace();
                            ui_state.clamp_process_selection(&snapshot.system.processes);
                        }
                        KeyCode::Char(ch) => {
                            ui_state.process_search_insert_char(ch);
                            ui_state.clamp_process_selection(&snapshot.system.processes);
                        }
                        _ => {}
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && ui_state.is_server_selector_open() =>
                {
                    match key.code {
                        KeyCode::Esc => ui_state.close_server_selector(),
                        KeyCode::Up | KeyCode::Char('k') => {
                            ui_state.move_server_selection(-1, order.len());
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            ui_state.move_server_selection(1, order.len());
                        }
                        KeyCode::Enter => {
                            if let Some(index) = ui_state.server_selector_index(order.len()) {
                                if let Some(key) = order.get(index) {
                                    selected = key.clone();
                                    snapshot.llm = registry.get(key).cloned().unwrap_or_default();
                                    // Show this server's own accumulated history;
                                    // every other server's history is preserved.
                                    ui_state.select_llm_history(key);
                                    // Re-attribute the GPU association to this
                                    // server's verified process and drop the
                                    // previous server's mapping so it is never
                                    // shown for the newly selected one.
                                    publish_selected_mapping(
                                        selected_identity_of(&server_list, &selected),
                                        &mut mapping_generation,
                                        &server_identity_tx,
                                    );
                                    snapshot.gpu_map = GpuMapping::None;
                                }
                            }
                            ui_state.close_server_selector();
                        }
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('s') => ui_state.open_server_selector(
                        order.iter().position(|key| key == &selected).unwrap_or(0),
                    ),
                    KeyCode::Char('/') => ui_state.open_process_search(),
                    KeyCode::Char('h') => ui_state.toggle_help(),
                    KeyCode::Esc if ui_state.is_help_open() => ui_state.close_help(),
                    _ if ui_state.is_help_open() => {}
                    KeyCode::Char('q') => ui_state.open_settings(&resolved.endpoint, &settings),
                    KeyCode::Esc => break,
                    KeyCode::Char('-') | KeyCode::Char('[') => {
                        change_refresh(&mut refresh_ms, false, &refresh_shared);
                        settings.refresh_ms = refresh_ms;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => {
                        change_refresh(&mut refresh_ms, true, &refresh_shared);
                        settings.refresh_ms = refresh_ms;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui_state.move_process_selection(-1, &snapshot.system.processes);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        ui_state.move_process_selection(1, &snapshot.system.processes);
                    }
                    KeyCode::PageUp => {
                        ui_state.move_process_selection(-10, &snapshot.system.processes);
                    }
                    KeyCode::PageDown => {
                        ui_state.move_process_selection(10, &snapshot.system.processes);
                    }
                    KeyCode::Home => ui_state.process_home(&snapshot.system.processes),
                    KeyCode::End => ui_state.process_end(&snapshot.system.processes),
                    _ => {}
                },
                Event::Mouse(mouse)
                    if !ui_state.is_help_open()
                        && !ui_state.is_settings_open()
                        && !ui_state.is_process_search_open()
                        && !ui_state.is_server_selector_open() =>
                {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if ui_state.click_process_row(mouse.column, mouse.row) {
                                continue;
                            }

                            // Any left-click away from a process row releases the pinned process.
                            ui_state.clear_process_selection();

                            let (width, height) = crossterm::terminal::size()?;
                            let mut handled = false;
                            // The refresh buttons only exist in the full
                            // layout; in the compact range those cells show the
                            // status line, so a click there must not change the
                            // interval behind an invisible control.
                            if ui::uses_full_layout(width, height) {
                                let header = Rect::new(0, 0, width, 3);
                                if let Some(controls) = ui::refresh_controls(header) {
                                    if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                                        change_refresh(&mut refresh_ms, false, &refresh_shared);
                                        settings.refresh_ms = refresh_ms;
                                        handled = true;
                                    } else if ui::rect_contains(
                                        controls.plus,
                                        mouse.column,
                                        mouse.row,
                                    ) {
                                        change_refresh(&mut refresh_ms, true, &refresh_shared);
                                        settings.refresh_ms = refresh_ms;
                                        handled = true;
                                    }
                                }
                            }
                            if !handled {
                                ui_state.click_process_sort(mouse.column, mouse.row);
                            }
                        }
                        MouseEventKind::Down(MouseButton::Right) => {
                            ui_state.right_click_process_group(mouse.column, mouse.row);
                        }
                        MouseEventKind::ScrollUp
                            if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                        {
                            ui_state.scroll_processes(-3);
                        }
                        MouseEventKind::ScrollDown
                            if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                        {
                            ui_state.scroll_processes(3);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    // S13: owned, clean shutdown — wait for the workers to observe `stop` and
    // exit before returning. Each `sleep_until_next_cycle` polls `stop` at 25 ms
    // granularity, so a healthy worker exits within a couple of cycles. The
    // join is bounded so a worker wedged in a slow vendor/filesystem call can
    // never hold the process open indefinitely (the OS reclaims it on exit).
    let join_bounded = |handle: thread::JoinHandle<()>| {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if done_rx.try_recv().is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    join_bounded(llm_worker);
    join_bounded(fast_worker);
    Ok(())
}

fn change_refresh(refresh_ms: &mut u64, increase: bool, shared: &AtomicU64) {
    *refresh_ms = if increase {
        refresh_ms
            .saturating_add(REFRESH_STEP_MS)
            .min(MAX_REFRESH_MS)
    } else {
        refresh_ms
            .saturating_sub(REFRESH_STEP_MS)
            .max(MIN_REFRESH_MS)
    };
    shared.store(*refresh_ms, Ordering::Relaxed);
}

/// Which parts of the resolved monitor target changed between two
/// resolutions. Each field is independent so the caller can react precisely:
/// only an endpoint change requires resetting the LLM HTTP session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ServerChange {
    endpoint: bool,
    pid: bool,
    auto: bool,
}

/// Compare two resolutions of the monitor target field by field, instead of
/// keying everything off the URL.
fn reconcile_server(
    previous: &crate::ResolvedServer,
    next: &crate::ResolvedServer,
) -> ServerChange {
    ServerChange {
        endpoint: previous.endpoint != next.endpoint,
        pid: previous.identity != next.identity,
        auto: previous.auto != next.auto,
    }
}

/// One LLM sample tagged with the session generation it was produced for.
///
/// A sample is sampled synchronously by the LLM worker, so an endpoint change
/// can land while a request for the previous server is still in flight. The
/// tag lets the app reject that late sample instead of showing it under the
/// new endpoint. Generations increase monotonically, so A→B→A is handled too
/// (the second A session has a different generation than the first).
struct LlmSample {
    /// Stable identity key of the server this sample was produced for.
    server: String,
    generation: u64,
    stats: LlmStats,
}

/// True when an LLM sample belongs to the currently selected LLM generation.
fn accepts_llm_sample(current_generation: u64, sample: &LlmSample) -> bool {
    sample.generation == current_generation
}

/// Build the compact per-server summaries for the overview, in `order`.
fn build_server_summaries(
    order: &[String],
    endpoints: &BTreeMap<String, String>,
    registry: &BTreeMap<String, LlmStats>,
) -> Vec<ServerSummary> {
    order
        .iter()
        .map(|key| {
            let endpoint = endpoints.get(key).cloned().unwrap_or_default();
            let stats = registry.get(key);
            let label = stats
                .map(|stats| stats.model.clone())
                .filter(|model| !model.is_empty())
                .unwrap_or_else(|| crate::redact::safe_endpoint(&endpoint));
            ServerSummary {
                key: key.clone(),
                endpoint,
                label,
                connected: stats.is_some_and(|stats| stats.connected && !stats.reconnecting),
                reconnecting: stats.is_some_and(|stats| stats.reconnecting),
                // Slot availability is propagated explicitly so an absent or
                // failed `/slots` never collapses to a fabricated `0/0`.
                slots_available: stats.is_some_and(|stats| stats.slots_available),
                slot_count: stats.map_or(0, |stats| stats.slot_count),
                busy_slots: stats.map_or(0, |stats| stats.busy_slots),
                props_slot_count: stats.map_or(0, |stats| stats.props_slot_count),
                context_used: stats.and_then(|stats| stats.context_used),
                context_size: stats.map_or(0, |stats| stats.context_size),
                generation_tps: stats.and_then(|stats| stats.generation_tps),
            }
        })
        .collect()
}

/// One server to monitor: a stable key, the endpoint to poll, and (for a
/// locally discovered process) its identity so a same-endpoint restart can be
/// detected and that server's caches reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    pub key: String,
    pub endpoint: String,
    pub identity: Option<LocalServerIdentity>,
}

/// Command to the LLM worker.
enum LlmCommand {
    /// Replace the monitored server set. `generation` tags every sample the
    /// worker produces from now on; a monitor whose key, endpoint and identity
    /// all persist keeps its per-server baselines, while a new key — or the
    /// same key with a changed process identity (a same-port restart) — starts
    /// fresh, and a dropped key is discarded.
    SetServers {
        generation: u64,
        servers: Vec<ServerSpec>,
    },
}

/// Apply a fast snapshot.
///
/// `gpu`/`system` are always applied — they do not belong to the server
/// session. The server→GPU mapping is applied only when the snapshot was
/// computed for the current mapping generation, so a snapshot carrying the
/// previous server's mapping cannot overwrite the new state.
fn apply_fast_snapshot(
    snapshot: &mut DashboardSnapshot,
    ui_state: &mut UiState,
    current_mapping_generation: u64,
    next: FastSnapshot,
) {
    ui_state.clamp_process_selection(&next.system.processes);
    ui_state.push_sample(&next.gpu, &next.system);
    snapshot.gpu = next.gpu;
    snapshot.system = next.system;
    if next.mapping_generation == current_mapping_generation {
        snapshot.gpu_map = next.gpu_map;
    }
}

/// Update the tracked primary `resolved` server (used for the header endpoint
/// and the `auto` label). It no longer drives GPU attribution: the association
/// follows the *selected* server via `publish_selected_mapping`.
fn update_resolved_server(current: &mut crate::ResolvedServer, next: crate::ResolvedServer) {
    let change = reconcile_server(current, &next);
    if change.endpoint || change.pid || change.auto {
        *current = next;
    }
}

/// Reconcile the monitored server set against the current settings/discovery.
///
/// When the set changes, a new LLM generation is started and the worker is
/// told the full new list. Per-server baselines for keys that persist are kept
/// by the worker. The selection falls back deterministically to the first
/// entry (by discovery order) only when the selected server is gone.
#[allow(clippy::too_many_arguments)]
fn sync_server_set(
    settings: &config::AppConfig,
    server_list: &mut Vec<ServerSpec>,
    overflow: &mut Vec<ServerSpec>,
    order: &mut Vec<String>,
    endpoints: &mut BTreeMap<String, String>,
    registry: &mut BTreeMap<String, LlmStats>,
    selected: &mut String,
    llm_generation: &mut u64,
    mapping_generation: &mut u64,
    server_tx: &mpsc::Sender<LlmCommand>,
    server_identity_tx: &mpsc::Sender<(u64, Option<LocalServerIdentity>)>,
    snapshot: &mut DashboardSnapshot,
    ui_state: &mut UiState,
) {
    let next = crate::collect_server_list(settings);
    let monitored_changed = next.monitored != *server_list;
    let overflow_changed = next.overflow != *overflow;
    if !monitored_changed && !overflow_changed {
        return;
    }
    *overflow = next.overflow;

    if monitored_changed {
        let next_list = next.monitored;
        // A server whose process identity changed while its endpoint stayed the
        // same is a known restart: start a fresh history boundary for it so
        // pre/post-restart throughput is never drawn as continuous.
        for key in changed_identity_keys(server_list, &next_list) {
            ui_state.reset_llm_history(&key);
        }

        *llm_generation = llm_generation.wrapping_add(1);
        let _ = server_tx.send(LlmCommand::SetServers {
            generation: *llm_generation,
            servers: next_list.clone(),
        });
        *server_list = next_list;
        *order = server_list.iter().map(|spec| spec.key.clone()).collect();
        endpoints.clear();
        endpoints.extend(
            server_list
                .iter()
                .map(|spec| (spec.key.clone(), spec.endpoint.clone())),
        );
        registry.retain(|key, _| endpoints.contains_key(key));
        // Bound per-server history memory and drop histories of servers that
        // are gone (no tombstone growth).
        let live_keys: HashSet<String> = server_list.iter().map(|spec| spec.key.clone()).collect();
        ui_state.retain_llm_histories(&live_keys);
        if !endpoints.contains_key(selected) {
            *selected = order.first().cloned().unwrap_or_default();
            snapshot.llm = registry.get(selected).cloned().unwrap_or_default();
            ui_state.select_llm_history(selected);
        }
        // The selected server's identity (or its disappearance) may have
        // changed: republish so the GPU association follows it and any stale
        // mapping is invalidated.
        publish_selected_mapping(
            selected_identity_of(server_list, selected),
            mapping_generation,
            server_identity_tx,
        );
        snapshot.gpu_map = GpuMapping::None;
    }
}

/// The verified local process identity of the selected server, if it has one.
/// A remote or explicitly configured endpoint without a matching process is
/// `None` and must stay UNKNOWN — never guessed.
fn selected_identity_of(server_list: &[ServerSpec], selected: &str) -> Option<LocalServerIdentity> {
    server_list
        .iter()
        .find(|spec| spec.key == selected)
        .and_then(|spec| spec.identity)
}

/// Publish which process's GPU association the fast worker should resolve,
/// advancing the mapping generation so a stale mapping of a previously
/// selected server cannot be applied.
fn publish_selected_mapping(
    identity: Option<LocalServerIdentity>,
    mapping_generation: &mut u64,
    server_identity_tx: &mpsc::Sender<(u64, Option<LocalServerIdentity>)>,
) {
    *mapping_generation = mapping_generation.wrapping_add(1);
    let _ = server_identity_tx.send((*mapping_generation, identity));
}

/// Keys present in both sets whose process identity changed (a known
/// same-endpoint restart). A new or removed key is not a restart.
fn changed_identity_keys(old: &[ServerSpec], new: &[ServerSpec]) -> Vec<String> {
    let previous: BTreeMap<&str, Option<LocalServerIdentity>> = old
        .iter()
        .map(|spec| (spec.key.as_str(), spec.identity))
        .collect();
    new.iter()
        .filter(
            |spec| matches!(previous.get(spec.key.as_str()), Some(prev) if *prev != spec.identity),
        )
        .map(|spec| spec.key.clone())
        .collect()
}

/// Map the local server to a GPU, gated on its identity.
///
/// `current_start_time` is `/proc/<pid>/stat` field 22 read now for the
/// expected PID. It must equal the start time captured at discovery: a PID
/// reused by another process (or an unreadable one) yields `Unknown` instead
/// of attributing the new process's GPU usage to the earlier server. The next
/// server resync supplies the fresh identity that re-enables the mapping.
fn map_server_identity(
    expected: LocalServerIdentity,
    current_start_time: Option<u64>,
    render: Vec<crate::domain::DeviceId>,
    nvml: Vec<crate::domain::DeviceId>,
    gpus: &[crate::discovery::DiscoveredGpu],
) -> GpuMapping {
    if current_start_time != Some(expected.start_time) {
        return GpuMapping::Unknown;
    }
    map_server_gpus(true, render, nvml, gpus)
}

fn spawn_fast_worker(
    gpu_selector: Arc<RwLock<GpuSelector>>,
    process_refresh_ms: Arc<AtomicU64>,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<FastSnapshot>,
    server_identity_rx: Receiver<(u64, Option<LocalServerIdentity>)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut current_selector = gpu_selector
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let static_gpus = discover_gpus(&RealSys);
        let mut gpu = new_gpu_provider(current_selector.clone(), &static_gpus);
        let mut system = System::new();
        let mut system_stats = SystemStats::default();
        let mut last_system_refresh: Option<Instant> = None;
        let mut last_process_refresh: Option<Instant> = None;
        let mut process_cache = ProcessCache::default();
        // S8: baseline for DRM fdinfo engine-utilization deltas, keyed by
        // (process identity, BDF, engine). Pruned on the process cadence.
        let mut drm_state = DrmSamplerState::default();
        let mut process_stats = Vec::<ProcessStats>::new();
        let mut previous_cpu_times = read_cpu_times();
        let mut cpu_topology: Option<CpuTopology> = None;
        // S12: slow-changing CPU sensors (temperature, power) sampled on their
        // own cadence; cached values are reused on the fast system cycles.
        let mut last_sensor_refresh: Option<Instant> = None;
        let mut cpu_temperature_c: Option<f64> = None;
        let mut cpu_power_w: Option<f64> = None;
        // S11: one-shot sensor discovery (cpufreq policies, CPU hwmon, RAPL
        // package zone); samples only re-read the dynamic counters.
        let mut cpu_sensors = CpuSensors::default();
        cpu_sensors.discover(&RealSys);

        // S16: server→GPU mapping state. Static topology and NVML are cached
        // once; the mapping itself is recomputed on the process-refresh cadence.
        let mut server_identity: Option<LocalServerIdentity> = None;
        // Mapping generation the current `server_identity` was published with;
        // stamped into every snapshot so the app can drop stale mappings.
        let mut server_generation: u64 = 0;
        let nvml_handle = Nvml::init().ok();
        // MIG topology is static: captured once at startup and never re-probed
        // on the process-refresh cadence (no static NVML polling in the loop).
        let mig_children = nvml_handle
            .as_ref()
            .map(discover_mig_children)
            .unwrap_or_default();
        let mut gpu_map: GpuMapping = GpuMapping::None;
        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            let requested_selector = gpu_selector
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if requested_selector != current_selector {
                current_selector = requested_selector;
                gpu = new_gpu_provider(current_selector.clone(), &static_gpus);
            }
            // Drain any server-identity updates (initial + resync/settings).
            while let Ok((generation, identity)) = server_identity_rx.try_recv() {
                server_generation = generation;
                server_identity = identity;
                gpu_map = GpuMapping::None;
            }
            let process_interval =
                Duration::from_millis(process_refresh_ms.load(Ordering::Relaxed).clamp(
                    config::MIN_PROCESS_REFRESH_MS,
                    config::MAX_PROCESS_REFRESH_MS,
                ));

            if last_process_refresh
                .map(|at| at.elapsed() >= process_interval)
                .unwrap_or(true)
            {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::All,
                    true,
                    ProcessRefreshKind::nothing()
                        .with_memory()
                        .with_cpu()
                        // Refresh on every cycle: `execve` keeps the PID and
                        // start time, so `OnlyIfNotSet` would pin the pre-exec
                        // image forever.
                        .with_exe(UpdateKind::Always),
                );
                process_stats = collect_process_stats(&system, &mut process_cache, &mut drm_state);
                last_process_refresh = Some(Instant::now());

                // S16: recompute the server→GPU mapping on the process cadence.
                // The mapping is only trusted while the PID still carries the
                // start time read at discovery, so a reused PID is invalidated
                // immediately — before the next server resync.
                if let Some(expected) = server_identity {
                    let current_start =
                        crate::discovery_llm::read_process_start_time(&RealSys, expected.pid);
                    let render = process_render_gpus(&RealSys, expected.pid, &static_gpus);
                    let nvml = nvml_compute_gpus(nvml_handle.as_ref(), expected.pid, &mig_children);
                    gpu_map =
                        map_server_identity(expected, current_start, render, nvml, &static_gpus);
                } else {
                    // No local server process (configured/remote endpoint) →
                    // there is no process to attribute; keep the mapping empty.
                    gpu_map = GpuMapping::None;
                }
            }

            // S12: slow-changing CPU thermal/power on their own cadence. The
            // RAPL power sample blocks ~50 ms for its two-read window, so it
            // must not run on the 250 ms system cadence.
            if last_sensor_refresh
                .map(|at| at.elapsed() >= SLOW_SENSOR_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                cpu_temperature_c = cpu_sensors.sample_temperature_c(&RealSys);
                cpu_power_w = cpu_sensors.sample_power_w(&RealSys);
                last_sensor_refresh = Some(Instant::now());
            }

            if last_system_refresh
                .map(|at| at.elapsed() >= SYSTEM_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                system.refresh_cpu_usage();
                system.refresh_memory();

                let current_cpu_times = read_cpu_times();
                let io_wait_pct = previous_cpu_times
                    .zip(current_cpu_times)
                    .and_then(|(previous, current)| io_wait_percent(previous, current));
                if current_cpu_times.is_some() {
                    previous_cpu_times = current_cpu_times;
                }

                // Unknown when /proc/loadavg is unreadable: kept as `None`
                // and rendered `—`, never silently turned into a fake zero.
                let load_average = read_load_average();
                let per_cpu_pairs = system
                    .cpus()
                    .iter()
                    .map(|cpu| (cpu.name(), cpu.cpu_usage()))
                    .collect::<Vec<_>>();
                let per_cpu_usage = per_cpu_usage_by_id(&per_cpu_pairs);
                let topology = cpu_topology
                    .get_or_insert_with(|| detect_cpu_topology(per_cpu_usage.len()))
                    .clone();
                system_stats = SystemStats {
                    cpu_usage: system.global_cpu_usage() as f64,
                    per_cpu_usage,
                    cpu_topology: topology,
                    // S11: cpufreq first, /proc/cpuinfo as fallback (cheap,
                    // so it stays on the fast system cadence).
                    cpu_frequency_mhz: cpu_sensors
                        .sample_frequency_mhz(&RealSys)
                        .or_else(|| read_cpu_frequency_mhz(&RealSys)),
                    // S12: reused from the slower sensor cadence.
                    cpu_temperature_c,
                    cpu_power_w,
                    io_wait_pct,
                    load_one: load_average.map(|(one, _, _)| one),
                    load_five: load_average.map(|(_, five, _)| five),
                    load_fifteen: load_average.map(|(_, _, fifteen)| fifteen),
                    memory_used_bytes: system.used_memory(),
                    memory_total_bytes: system.total_memory(),
                    swap_used_bytes: system.used_swap(),
                    swap_total_bytes: system.total_swap(),
                    processes: process_stats.clone(),
                };
                last_system_refresh = Some(Instant::now());
            }

            let snapshot = FastSnapshot {
                gpu: gpu.sample(),
                system: system_stats.clone(),
                gpu_map: gpu_map.clone(),
                mapping_generation: server_generation,
            };

            match tx.try_send(snapshot) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_REFRESH_MS, &stop);
        }
    })
}

/// Cached *static* process metadata, keyed by [`ProcessIdentity`].
///
/// The kernel recycles PIDs, so identity is `pid + start_time`. Once a
/// process has been seen, its static metadata (program name, executable
/// path) is cached and reused; only the dynamic counters (CPU, memory,
/// thread count) are resampled on each process refresh.
///
/// The thread count is deliberately **not** cached here: a process can create
/// and destroy threads at any time, so a cached value goes stale immediately,
/// and a one-off read failure must not pin a fabricated count forever. It is
/// re-read from `/proc/<pid>/status` on every process refresh (not per UI
/// frame) and reported as `Option`: an unreadable process is `None` and the
/// next refresh retries.
#[derive(Default)]
struct ProcessCache {
    entries: std::collections::HashMap<ProcessIdentity, CachedProcess>,
}

struct CachedProcess {
    program: String,
    command: String,
}

impl ProcessCache {
    /// Return the cached static metadata for `identity`, computing and storing
    /// it on first sight. Only identity-bound, stable fields are cached.
    fn stable(
        &mut self,
        identity: ProcessIdentity,
        program: &str,
        command: &str,
    ) -> (String, String) {
        let entry = self
            .entries
            .entry(identity)
            .or_insert_with(|| CachedProcess {
                program: program.to_string(),
                command: command.to_string(),
            });
        // `execve` keeps the PID and start time, so the same identity can name a
        // new image. Refresh the cached names when the freshly read ones change
        // (an empty read is transient and keeps the last good value).
        if !program.is_empty() && entry.program != program {
            entry.program = program.to_string();
        }
        if !command.is_empty() && entry.command != command {
            entry.command = command.to_string();
        }
        (entry.program.clone(), entry.command.clone())
    }

    /// Drop entries for identities that no longer exist (exited processes).
    fn retain(&mut self, live: impl Iterator<Item = ProcessIdentity>) {
        let live: std::collections::HashSet<_> = live.collect();
        self.entries.retain(|identity, _| live.contains(identity));
    }
}

/// Per-CPU usage indexed by kernel CPU id (matching the id-indexed topology
/// vectors), so a non-prefix online set does not misalign the heatmap. sysinfo
/// only reports the online CPUs; offline holes stay `0.0`. When the ids are
/// not recoverable from the names the compact order is kept.
fn per_cpu_usage_by_id(cpus: &[(&str, f32)]) -> Vec<f64> {
    let ids: Option<Vec<usize>> = cpus
        .iter()
        .map(|(name, _)| {
            name.strip_prefix("cpu")
                .and_then(|id| id.parse::<usize>().ok())
        })
        .collect();
    match ids {
        Some(ids) => {
            let span = ids.iter().copied().max().map_or(0, |max| max + 1);
            let mut usage = vec![0.0; span];
            for ((_, value), id) in cpus.iter().zip(ids) {
                if let Some(slot) = usage.get_mut(id) {
                    *slot = *value as f64;
                }
            }
            usage
        }
        None => cpus.iter().map(|(_, value)| *value as f64).collect(),
    }
}

/// Static metadata from the cache plus a freshly read dynamic thread count.
///
/// `read_threads` is injected so the refresh behavior is testable without a
/// live `/proc`: the count is read on every call (never taken from the
/// cache), and a failed read yields `None` rather than a substitute value.
fn refresh_process_metadata<F>(
    cache: &mut ProcessCache,
    identity: ProcessIdentity,
    program: &str,
    command: &str,
    read_threads: F,
) -> (String, String, Option<usize>)
where
    F: FnOnce(u32) -> Option<usize>,
{
    let (program, command) = cache.stable(identity, program, command);
    (program, command, read_threads(identity.pid))
}

fn collect_process_stats(
    system: &System,
    cache: &mut ProcessCache,
    drm: &mut DrmSamplerState,
) -> Vec<ProcessStats> {
    let now = Instant::now();
    let mut stats = Vec::with_capacity(system.processes().len());
    let mut live = Vec::new();
    for (pid, process) in system.processes().iter() {
        let identity = ProcessIdentity::new(pid.as_u32(), process.start_time());
        live.push(identity);

        let program = process.name().to_string_lossy().into_owned();
        let command = process
            .exe()
            .map(|path| path.display().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| program.clone());

        let (program, command, threads) = refresh_process_metadata(
            cache,
            identity,
            &program,
            &command,
            read_process_thread_count,
        );

        // S8: per-device GPU usage from the process's DRM fdinfo entries.
        let gpu = sample_process_gpus(&RealSys, now, identity, drm);

        stats.push(ProcessStats {
            pid: identity.pid,
            program,
            command,
            cpu_pct: process.cpu_usage() as f64,
            memory_bytes: process.memory(),
            threads,
            start_time: identity.start_time,
            gpu,
        });
    }
    cache.retain(live.iter().copied());
    drm.retain(live.into_iter());
    stats
}

fn read_process_thread_count(pid: u32) -> Option<usize> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("Threads:")?;
        value.trim().parse().ok()
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct CpuTimes {
    total: u64,
    io_wait: u64,
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_times(&text)
}

fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 5 {
        return None;
    }

    Some(CpuTimes {
        total: values.iter().copied().sum(),
        io_wait: values[4],
    })
}

fn io_wait_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total_delta = current.total.saturating_sub(previous.total);
    if total_delta == 0 {
        return None;
    }
    let io_wait_delta = current.io_wait.saturating_sub(previous.io_wait);
    Some(io_wait_delta as f64 / total_delta as f64 * 100.0)
}

fn read_load_average() -> Option<(f64, f64, f64)> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    Some((
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ))
}

fn read_cpu_frequency_mhz(sys: &dyn Sys) -> Option<f64> {
    let text = sys.read_to_string(&PathBuf::from("/proc/cpuinfo"))?;
    let mut total = 0.0;
    let mut count = 0u64;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "cpu MHz" {
            continue;
        }
        let Ok(mhz) = value.trim().parse::<f64>() else {
            continue;
        };
        if mhz.is_finite() && mhz > 0.0 {
            total += mhz;
            count += 1;
        }
    }

    (count > 0).then_some(total / count as f64)
}

/// One monitored server's worker-side state.
struct LlmServerMonitor {
    key: String,
    endpoint: String,
    /// Process identity of a locally discovered server (pid + start time); a
    /// change on the same key means the process restarted, so the monitor's
    /// caches are rebuilt.
    identity: Option<LocalServerIdentity>,
    monitor: Option<LlamaMonitor>,
    init_error: String,
    /// Per-server last good sample and its time, for the offline-grace hold.
    last_good: Option<(LlmStats, Instant)>,
}

fn new_server_monitor(spec: ServerSpec) -> LlmServerMonitor {
    let monitor = LlamaMonitor::new(&spec.endpoint).ok();
    let init_error = if monitor.is_none() {
        "failed to initialize HTTP client".to_string()
    } else {
        String::new()
    };
    LlmServerMonitor {
        key: spec.key,
        endpoint: spec.endpoint,
        identity: spec.identity,
        monitor,
        init_error,
        last_good: None,
    }
}

/// Rebuild the monitor list for a new server set. A monitor is preserved only
/// when its key, endpoint *and* process identity all still match, so a periodic
/// resync keeps telemetry for an unchanged server while a same-endpoint
/// restart (new identity) resets that server's caches.
fn rebuild_server_monitors(
    existing: Vec<LlmServerMonitor>,
    list: Vec<ServerSpec>,
) -> Vec<LlmServerMonitor> {
    let mut old: BTreeMap<String, LlmServerMonitor> = existing
        .into_iter()
        .map(|monitor| (monitor.key.clone(), monitor))
        .collect();
    list.into_iter()
        .map(|spec| match old.remove(&spec.key) {
            Some(existing)
                if existing.endpoint == spec.endpoint && existing.identity == spec.identity =>
            {
                existing
            }
            _ => new_server_monitor(spec),
        })
        .collect()
}

fn spawn_llm_worker(
    initial_servers: Vec<ServerSpec>,
    refresh_ms: Arc<AtomicU64>,
    offline_grace_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<LlmSample>,
    server_rx: Receiver<LlmCommand>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Session generation of the current server set; every published sample
        // is tagged with it so a result from a set the app already replaced can
        // be rejected. Each server additionally carries its own key, so samples
        // never cross server boundaries.
        let mut generation: u64 = 0;
        let mut servers: Vec<LlmServerMonitor> = initial_servers
            .into_iter()
            .map(new_server_monitor)
            .collect();

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            while let Ok(command) = server_rx.try_recv() {
                match command {
                    LlmCommand::SetServers {
                        generation: next_generation,
                        servers: list,
                    } => {
                        generation = next_generation;
                        servers = rebuild_server_monitors(servers, list);
                    }
                }
            }

            let offline_grace = Duration::from_millis(
                offline_grace_ms
                    .load(Ordering::Relaxed)
                    .min(config::MAX_OFFLINE_GRACE_MS),
            );
            // Poll every server concurrently (bounded by the server cap) and
            // deliver each completed sample to the app as soon as it is ready,
            // so a slow or hanging server cannot delay a healthy server's
            // refresh for the whole cycle. A scoped thread per server is
            // bounded and joined when the scope closes.
            let disconnected = thread::scope(|scope| {
                let (done_tx, done_rx) = mpsc::channel::<LlmSample>();
                for server in servers.iter_mut() {
                    let done_tx = done_tx.clone();
                    scope.spawn(move || {
                        let raw_stats = match server.monitor.as_mut() {
                            Some(monitor) => monitor.sample(),
                            None => LlmStats {
                                error: server.init_error.clone(),
                                ..Default::default()
                            },
                        };
                        let stats = stabilize_llm_sample(
                            raw_stats,
                            &mut server.last_good,
                            Instant::now(),
                            offline_grace,
                        );
                        let _ = done_tx.send(LlmSample {
                            server: server.key.clone(),
                            generation,
                            stats,
                        });
                    });
                }
                drop(done_tx);
                let mut disconnected = false;
                for sample in done_rx.iter() {
                    match tx.try_send(sample) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                disconnected
            });
            if disconnected {
                return;
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_LLM_POLL_MS, &stop);
        }
    })
}

fn stabilize_llm_sample(
    mut stats: LlmStats,
    last_good: &mut Option<(LlmStats, Instant)>,
    now: Instant,
    offline_grace: Duration,
) -> LlmStats {
    if stats.connected {
        stats.reconnecting = false;
        *last_good = Some((stats.clone(), now));
        return stats;
    }

    let error = stats.error.to_ascii_lowercase();
    let hard_failure = error.contains("metrics disabled")
        || error.contains("--metrics")
        || error.contains("http 501")
        || error.contains("failed to initialize http client");

    if !hard_failure {
        if let Some((previous, at)) = last_good.as_ref() {
            if now.saturating_duration_since(*at) <= offline_grace {
                let mut held = previous.clone();
                held.error.clear();
                held.reconnecting = true;
                return held;
            }
        }
    }

    stats
}

/// Sleeps until the next cycle boundary, clamping the live refresh value to
/// `[min_ms, MAX_REFRESH_MS]`. Callers pass their own floor: `MIN_REFRESH_MS`
/// for the fast worker and `MIN_LLM_POLL_MS` for the LLM worker, so a fast UI
/// refresh cannot drive the /metrics HTTP poll faster than 250 ms.
fn next_cycle_target(refresh_ms: &AtomicU64, min_ms: u64) -> Duration {
    Duration::from_millis(
        refresh_ms
            .load(Ordering::Relaxed)
            .clamp(min_ms, MAX_REFRESH_MS),
    )
}

fn sleep_until_next_cycle(
    started: Instant,
    refresh_ms: &AtomicU64,
    min_ms: u64,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let target = next_cycle_target(refresh_ms, min_ms);
        let elapsed = started.elapsed();
        if elapsed >= target {
            break;
        }

        thread::sleep((target - elapsed).min(Duration::from_millis(25)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec(key: &str, endpoint: &str) -> ServerSpec {
        ServerSpec {
            key: key.to_string(),
            endpoint: endpoint.to_string(),
            identity: None,
        }
    }

    #[test]
    fn per_cpu_usage_is_indexed_by_kernel_cpu_id_with_holes() {
        // Online set 0,2-3 (cpu1 offline): usage lands at its ids, hole stays 0.
        let usage = per_cpu_usage_by_id(&[("cpu0", 10.0), ("cpu2", 30.0), ("cpu3", 40.0)]);
        assert_eq!(usage, vec![10.0, 0.0, 30.0, 40.0]);

        // Non-prefix start (cpuset/isolcpus): ids preserved, holes zeroed.
        let usage = per_cpu_usage_by_id(&[("cpu4", 4.0), ("cpu5", 5.0)]);
        assert_eq!(usage, vec![0.0, 0.0, 0.0, 0.0, 4.0, 5.0]);

        // Unparseable names fall back to compact order.
        let usage = per_cpu_usage_by_id(&[("weird", 1.0), ("other", 2.0)]);
        assert_eq!(usage, vec![1.0, 2.0]);
    }

    #[test]
    fn cache_reuses_stable_metadata_for_same_identity() {
        let mut cache = ProcessCache::default();
        let identity = ProcessIdentity::new(0, 1234);
        let first = cache.stable(identity, "app", "/bin/app");
        let second = cache.stable(identity, "app", "/bin/app");
        assert_eq!(first, second);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn cache_refreshes_static_metadata_when_it_changes_for_the_same_identity() {
        // `execve` keeps the PID and start time but changes the image.
        let mut cache = ProcessCache::default();
        let identity = ProcessIdentity::new(4242, 1000);
        assert_eq!(
            cache.stable(identity, "sh", "/bin/sh"),
            ("sh".to_string(), "/bin/sh".to_string())
        );
        assert_eq!(
            cache.stable(identity, "llama-server", "/usr/bin/llama-server"),
            (
                "llama-server".to_string(),
                "/usr/bin/llama-server".to_string()
            )
        );
        // An empty transient read keeps the last good value.
        assert_eq!(
            cache.stable(identity, "", ""),
            (
                "llama-server".to_string(),
                "/usr/bin/llama-server".to_string()
            )
        );
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn cache_treats_reused_pid_with_new_start_time_as_new_process() {
        let mut cache = ProcessCache::default();
        let old = ProcessIdentity::new(4242, 1000);
        let new = ProcessIdentity::new(4242, 2000);
        cache.stable(old, "old-prog", "/bin/old");
        let (_, command) = cache.stable(new, "new-prog", "/bin/new");
        assert_eq!(command, "/bin/new");
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn thread_count_is_resampled_each_refresh_and_recovers_after_a_failed_read() {
        let mut cache = ProcessCache::default();
        let identity = ProcessIdentity::new(4242, 1000);

        // First sighting: static metadata is cached, threads read fresh.
        let (program, command, first) =
            refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(4));
        assert_eq!((program.as_str(), command.as_str()), ("app", "/bin/app"));
        assert_eq!(first, Some(4));

        // Same identity: the static metadata is reused, but the thread count
        // is read again — a changed count reaches the next refresh.
        let (program, command, second) =
            refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(9));
        assert_eq!((program.as_str(), command.as_str()), ("app", "/bin/app"));
        assert_eq!(second, Some(9));

        // A transient read failure is unknown for that cycle, not a
        // permanent substitute...
        let (_, _, failed) =
            refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| None);
        assert_eq!(failed, None);

        // ...and the next successful read recovers.
        let (_, _, recovered) =
            refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(11));
        assert_eq!(recovered, Some(11));

        assert_eq!(cache.entries.len(), 1, "static metadata stays cached once");
    }

    #[test]
    fn reused_pid_does_not_inherit_cached_static_metadata() {
        let mut cache = ProcessCache::default();
        let old = ProcessIdentity::new(5150, 1000);
        let new = ProcessIdentity::new(5150, 2000);
        refresh_process_metadata(
            &mut cache,
            old,
            "llama-server",
            "/usr/bin/llama-server",
            |_| Some(8),
        );
        let (program, command, threads) =
            refresh_process_metadata(&mut cache, new, "other", "/usr/bin/other", |_| Some(2));
        assert_eq!(
            (program.as_str(), command.as_str()),
            ("other", "/usr/bin/other")
        );
        assert_eq!(threads, Some(2));
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn cache_retain_drops_exited_processes() {
        let mut cache = ProcessCache::default();
        let alive = ProcessIdentity::new(0, 1);
        let gone = ProcessIdentity::new(0, 2);
        cache.stable(alive, "a", "a");
        cache.stable(gone, "b", "b");
        cache.retain(std::iter::once(alive));
        assert_eq!(cache.entries.len(), 1);
    }
    #[test]
    fn refresh_change_is_clamped() {
        let shared = AtomicU64::new(MIN_REFRESH_MS);
        let mut value = MIN_REFRESH_MS;
        change_refresh(&mut value, false, &shared);
        assert_eq!(value, MIN_REFRESH_MS);

        value = MAX_REFRESH_MS;
        change_refresh(&mut value, true, &shared);
        assert_eq!(value, MAX_REFRESH_MS);
    }

    #[test]
    fn refresh_change_updates_shared_value() {
        let shared = AtomicU64::new(1000);
        let mut value = 1000;
        change_refresh(&mut value, false, &shared);
        assert_eq!(value, 900);
        assert_eq!(shared.load(Ordering::Relaxed), 900);
    }

    #[test]
    fn transient_llm_failure_keeps_last_sample_and_marks_reconnecting() {
        let now = Instant::now();
        let mut last_good = None;
        let good = LlmStats {
            connected: true,
            prompt_tps: Some(123.0),
            ..LlmStats::default()
        };
        let fresh = stabilize_llm_sample(good, &mut last_good, now, Duration::from_millis(2500));
        assert!(fresh.connected);
        assert!(!fresh.reconnecting);

        let failed = LlmStats {
            error: "cannot reach llama.cpp: timeout".to_string(),
            ..LlmStats::default()
        };
        let held = stabilize_llm_sample(
            failed,
            &mut last_good,
            now + Duration::from_millis(500),
            Duration::from_millis(2500),
        );
        assert!(held.connected);
        assert!(held.reconnecting);
        assert_eq!(held.prompt_tps, Some(123.0));
    }

    #[test]
    fn parses_linux_cpu_times_and_iowait_delta() {
        let previous = parse_cpu_times(
            "cpu  100 0 50 800 20 0 10 0 0 0
",
        )
        .unwrap();
        let current = parse_cpu_times(
            "cpu  120 0 60 850 25 0 15 0 0 0
",
        )
        .unwrap();
        assert_eq!(previous.io_wait, 20);
        assert_eq!(current.io_wait, 25);
        let io_wait = io_wait_percent(previous, current).unwrap();
        assert!((io_wait - 5.555555556).abs() < 0.001);
    }

    #[test]
    fn cycle_target_clamps_to_worker_floor() {
        let fast = AtomicU64::new(100);
        assert_eq!(
            next_cycle_target(&fast, MIN_LLM_POLL_MS),
            Duration::from_millis(250)
        );
        assert_eq!(
            next_cycle_target(&fast, MIN_REFRESH_MS),
            Duration::from_millis(100)
        );

        let slow = AtomicU64::new(5000);
        assert_eq!(
            next_cycle_target(&slow, MIN_LLM_POLL_MS),
            Duration::from_millis(5000)
        );
        assert_eq!(
            next_cycle_target(&slow, MIN_REFRESH_MS),
            Duration::from_millis(5000)
        );

        let unbounded = AtomicU64::new(60_000);
        assert_eq!(
            next_cycle_target(&unbounded, MIN_LLM_POLL_MS),
            Duration::from_millis(MAX_REFRESH_MS)
        );
    }

    fn ident(pid: u32, start_time: u64) -> LocalServerIdentity {
        LocalServerIdentity { pid, start_time }
    }

    fn target(
        endpoint: &str,
        identity: Option<LocalServerIdentity>,
        auto: bool,
    ) -> crate::ResolvedServer {
        crate::ResolvedServer {
            endpoint: endpoint.to_string(),
            identity,
            auto,
        }
    }

    fn mapped(bdf: &str) -> crate::domain::MappedGpu {
        crate::domain::MappedGpu {
            device: crate::domain::DeviceId::new(Some(bdf.to_string()), None),
            name: "card0".to_string(),
            vendor: crate::domain::GpuVendor::Amd,
            evidence: crate::domain::GpuEvidence::RenderNodeFd,
        }
    }

    fn discovered_gpu(bdf: &str) -> crate::discovery::DiscoveredGpu {
        crate::discovery::DiscoveredGpu {
            card: "card0".to_string(),
            device_id: crate::domain::DeviceId::new(Some(bdf.to_string()), None),
            vendor: crate::domain::GpuVendor::Amd,
            pci_vendor_id: 0x1002,
            pci_device_id: 0x74a0,
            pci_class_code: 0x030000,
            driver: "amdgpu".to_string(),
            render_nodes: vec!["renderD128".to_string()],
            outputs: Vec::new(),
        }
    }

    #[test]
    fn reconcile_detects_endpoint_identity_and_origin_independently() {
        let unchanged = ServerChange {
            endpoint: false,
            pid: false,
            auto: false,
        };
        let base = target("http://127.0.0.1:8081", Some(ident(5000, 1)), true);

        assert_eq!(reconcile_server(&base, &base), unchanged);

        // Restart on the same port: endpoint stable, identity changed.
        assert_eq!(
            reconcile_server(
                &base,
                &target("http://127.0.0.1:8081", Some(ident(6000, 1)), true)
            ),
            ServerChange {
                endpoint: false,
                pid: true,
                auto: false,
            }
        );

        // Reused PID (same number, new start time) still counts as a changed
        // identity, so the mapping is invalidated even before the next resync.
        assert_eq!(
            reconcile_server(
                &base,
                &target("http://127.0.0.1:8081", Some(ident(5000, 2)), true)
            ),
            ServerChange {
                endpoint: false,
                pid: true,
                auto: false,
            }
        );

        // Server disappeared: identity cleared and origin flips to manual.
        assert_eq!(
            reconcile_server(&base, &target("http://127.0.0.1:8081", None, false)),
            ServerChange {
                endpoint: false,
                pid: true,
                auto: true,
            }
        );

        // New endpoint entirely.
        assert_eq!(
            reconcile_server(
                &base,
                &target("http://127.0.0.1:9090", Some(ident(5000, 1)), true)
            ),
            ServerChange {
                endpoint: true,
                pid: false,
                auto: false,
            }
        );

        // Origin-only change is still detected (defensive: the origin is
        // tracked separately, not inferred from the URL).
        assert_eq!(
            reconcile_server(
                &base,
                &target("http://127.0.0.1:8081", Some(ident(5000, 1)), false)
            ),
            ServerChange {
                endpoint: false,
                pid: false,
                auto: true,
            }
        );
    }

    #[test]
    fn selected_identity_and_mapping_publication() {
        let (tx, rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
        let specs = vec![
            ServerSpec {
                key: "a".to_string(),
                endpoint: "http://a".to_string(),
                identity: Some(ident(5000, 1)),
            },
            ServerSpec {
                key: "b".to_string(),
                endpoint: "http://b".to_string(),
                identity: None,
            },
        ];
        // The identity comes from the selected spec; a remote/configured server
        // (no local process) yields None and must stay UNKNOWN.
        assert_eq!(selected_identity_of(&specs, "a"), Some(ident(5000, 1)));
        assert_eq!(selected_identity_of(&specs, "b"), None);
        assert_eq!(selected_identity_of(&specs, "missing"), None);

        // Publishing advances the generation and sends the selected identity, so
        // a stale mapping of a previously selected server is invalidated.
        let mut generation = 0u64;
        publish_selected_mapping(selected_identity_of(&specs, "a"), &mut generation, &tx);
        assert_eq!(generation, 1);
        assert_eq!(rx.try_recv().unwrap(), (1, Some(ident(5000, 1))));
        publish_selected_mapping(selected_identity_of(&specs, "b"), &mut generation, &tx);
        assert_eq!(generation, 2);
        assert_eq!(rx.try_recv().unwrap(), (2, None));
    }

    #[test]
    fn update_resolved_server_only_relabels() {
        let mut current = target("http://127.0.0.1:8081", Some(ident(5000, 1)), true);
        update_resolved_server(&mut current, target("http://127.0.0.1:9090", None, false));
        assert_eq!(current.endpoint, "http://127.0.0.1:9090");
        assert!(!current.auto);
        assert_eq!(current.identity, None);
    }

    #[test]
    fn stale_llm_sample_is_rejected_across_generations() {
        // A sample is accepted only under the current generation; the A→B→A
        // case is handled because the second A session has a new generation.
        let a_sample = LlmSample {
            server: "http://a:1/".to_string(),
            generation: 0,
            stats: LlmStats {
                model: "A".to_string(),
                ..Default::default()
            },
        };
        assert!(accepts_llm_sample(0, &a_sample));
        let b_sample = LlmSample {
            server: "http://b:1/".to_string(),
            generation: 1,
            stats: LlmStats {
                model: "B".to_string(),
                ..Default::default()
            },
        };
        assert!(accepts_llm_sample(1, &b_sample));
        // A delayed result from the first A session is rejected under B and
        // under the second A session (generation 2).
        assert!(!accepts_llm_sample(1, &a_sample));
        assert!(!accepts_llm_sample(2, &a_sample));
    }

    #[test]
    fn stale_gpu_mapping_is_not_applied_but_system_data_is() {
        let mapped_b = crate::domain::GpuMapping::Single(mapped("0000:02:00.0"));
        let mut snapshot = DashboardSnapshot {
            gpu_map: mapped_b.clone(),
            ..DashboardSnapshot::default()
        };
        let mut ui_state = UiState::default();

        // A snapshot from the previous mapping generation carries A's mapping
        // plus fresh system/GPU data. The mapping must be ignored; the rest
        // must still be applied.
        let stale = FastSnapshot {
            gpu: crate::domain::GpuStats {
                name: "fresh-gpu".to_string(),
                ..Default::default()
            },
            system: SystemStats {
                cpu_usage: 42.0,
                ..Default::default()
            },
            gpu_map: crate::domain::GpuMapping::Single(mapped("0000:01:00.0")),
            mapping_generation: 1,
        };
        apply_fast_snapshot(&mut snapshot, &mut ui_state, 2, stale);

        assert_eq!(
            snapshot.gpu_map, mapped_b,
            "stale GPU mapping must not overwrite the current one"
        );
        assert_eq!(snapshot.system.cpu_usage, 42.0, "system data still applies");
        assert_eq!(snapshot.gpu.name, "fresh-gpu", "GPU data still applies");
    }

    #[test]
    fn integrated_selection_switch_rejects_delayed_gpu_mapping() {
        use crate::domain::{DeviceId, GpuMapping};

        // Two locally verified servers on distinct GPUs. Everything below runs
        // through the production seams: `selected_identity_of` +
        // `publish_selected_mapping` (selection → generation) and
        // `map_server_identity` + `apply_fast_snapshot` (evidence → TUI), with
        // no GPU hardware and no HTTP.
        let gpus = vec![
            discovered_gpu("0000:01:00.0"),
            discovered_gpu("0000:02:00.0"),
        ];
        let ident_a = ident(5000, 1);
        let ident_b = ident(6000, 1);
        let specs = vec![
            ServerSpec {
                key: "a".to_string(),
                endpoint: "http://127.0.0.1:8081".to_string(),
                identity: Some(ident_a),
            },
            ServerSpec {
                key: "b".to_string(),
                endpoint: "http://127.0.0.1:8082".to_string(),
                identity: Some(ident_b),
            },
        ];
        let evidence_a = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
        let evidence_b = vec![DeviceId::new(Some("0000:02:00.0".into()), None)];

        let (tx, _rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
        let mut mapping_generation = 0u64;
        let mut selected = "a".to_string();
        let mut snapshot = DashboardSnapshot::default();
        let mut ui_state = UiState::default();
        let publish = |selected: &str, generation: &mut u64, snapshot: &mut DashboardSnapshot| {
            publish_selected_mapping(selected_identity_of(&specs, selected), generation, &tx);
            // The app clears the shown mapping synchronously on any selection
            // change so no stale device label survives the switch.
            snapshot.gpu_map = GpuMapping::None;
        };

        // --- Select A → GPU 1. ---
        publish(&selected, &mut mapping_generation, &mut snapshot);
        let generation_a = mapping_generation;
        let map_a = map_server_identity(ident_a, Some(1), evidence_a.clone(), Vec::new(), &gpus);
        assert!(matches!(map_a, GpuMapping::Single(_)), "A maps to one GPU");
        apply_fast_snapshot(
            &mut snapshot,
            &mut ui_state,
            mapping_generation,
            FastSnapshot {
                gpu_map: map_a.clone(),
                mapping_generation: generation_a,
                ..FastSnapshot::default()
            },
        );
        assert_eq!(snapshot.gpu_map, map_a, "GPU 1 displayed for A");

        // --- Select B → A's mapping cleared immediately. ---
        selected = "b".to_string();
        publish(&selected, &mut mapping_generation, &mut snapshot);
        assert!(
            snapshot.gpu_map.is_empty(),
            "switching servers must clear the previous attribution immediately"
        );

        // A delayed snapshot produced for A arrives under B → rejected.
        apply_fast_snapshot(
            &mut snapshot,
            &mut ui_state,
            mapping_generation,
            FastSnapshot {
                gpu_map: map_a.clone(),
                mapping_generation: generation_a,
                ..FastSnapshot::default()
            },
        );
        assert!(
            snapshot.gpu_map.is_empty(),
            "delayed A snapshot must not be applied under B"
        );

        // B's valid snapshot → GPU 2.
        let generation_b = mapping_generation;
        let map_b = map_server_identity(ident_b, Some(1), evidence_b.clone(), Vec::new(), &gpus);
        assert!(matches!(map_b, GpuMapping::Single(_)), "B maps to one GPU");
        apply_fast_snapshot(
            &mut snapshot,
            &mut ui_state,
            mapping_generation,
            FastSnapshot {
                gpu_map: map_b.clone(),
                mapping_generation: generation_b,
                ..FastSnapshot::default()
            },
        );
        assert_eq!(snapshot.gpu_map, map_b, "GPU 2 displayed for B");
        assert_ne!(map_a, map_b, "A and B are attributed to distinct GPUs");

        // --- Switch back to A. ---
        selected = "a".to_string();
        publish(&selected, &mut mapping_generation, &mut snapshot);
        assert!(
            snapshot.gpu_map.is_empty(),
            "B mapping cleared on return to A"
        );
        apply_fast_snapshot(
            &mut snapshot,
            &mut ui_state,
            mapping_generation,
            FastSnapshot {
                gpu_map: map_b.clone(),
                mapping_generation: generation_b,
                ..FastSnapshot::default()
            },
        );
        assert!(
            snapshot.gpu_map.is_empty(),
            "delayed B snapshot must not be applied under the second A selection"
        );
        apply_fast_snapshot(
            &mut snapshot,
            &mut ui_state,
            mapping_generation,
            FastSnapshot {
                gpu_map: map_a.clone(),
                mapping_generation,
                ..FastSnapshot::default()
            },
        );
        assert_eq!(snapshot.gpu_map, map_a, "fresh A snapshot re-shows GPU 1");
    }

    #[test]
    fn reused_pid_with_a_new_start_time_gets_no_gpu_mapping() {
        let gpus = vec![discovered_gpu("0000:01:00.0")];
        let evidence = vec![crate::domain::DeviceId::new(
            Some("0000:01:00.0".into()),
            None,
        )];
        let old = ident(4242, 1000);

        // Same PID number, different start time: a different process now owns
        // the PID. GPU evidence is present, but it must not be attributed to
        // the earlier server — the mapping is invalid immediately, not only
        // after the next resync.
        let stale = map_server_identity(old, Some(2000), evidence.clone(), Vec::new(), &gpus);
        assert!(
            !matches!(stale, GpuMapping::Single(_) | GpuMapping::Multi(_)),
            "reused PID must not map to the earlier server: {stale:?}"
        );
        assert!(stale.is_empty(), "expected no attribution, got {stale:?}");

        // An unreadable start time is also untrusted.
        let unreadable = map_server_identity(old, None, evidence.clone(), Vec::new(), &gpus);
        assert!(
            unreadable.is_empty(),
            "expected no attribution: {unreadable:?}"
        );

        // When discovery reports the genuine new llama.cpp process (same PID,
        // new start time), the mapping is re-enabled for it.
        let new = ident(4242, 2000);
        let fresh = map_server_identity(new, Some(2000), evidence, Vec::new(), &gpus);
        assert!(
            matches!(fresh, GpuMapping::Single(_)),
            "a genuine new server must map again: {fresh:?}"
        );
    }

    // --- Real-worker integration test for delayed results -------------------
    //
    // Unlike the pure `accepts_llm_sample` tests above, these drive the actual
    // `spawn_llm_worker` against isolated local mock HTTP servers: the worker
    // performs real `/props` + `/metrics` + `/slots` requests through reqwest,
    // the command channel is the production one, and the results pass through
    // the production acceptance helper. Synchronization is explicit (channels
    // and a condition variable); the timeouts only bound failure, they are
    // never the success condition.

    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Condvar, Mutex};

    fn read_request_path(stream: &mut TcpStream) -> Option<String> {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        text.lines()
            .next()?
            .split_whitespace()
            .nth(1)
            .map(str::to_string)
    }

    fn mock_body(
        path: &str,
        model: &str,
        n_ctx: u64,
        prompt_tokens: u64,
        predicted: u64,
    ) -> (String, &'static str) {
        if path.contains("/props") {
            (
                format!(
                    "{{\"model_path\":\"{model}\",\"total_slots\":1,\"default_generation_settings\":{{\"n_ctx\":{n_ctx}}}}}"
                ),
                "application/json",
            )
        } else if path.contains("/slots") {
            (
                format!("[{{\"id\":0,\"n_ctx\":{n_ctx},\"is_processing\":false}}]"),
                "application/json",
            )
        } else {
            (
                format!(
                    "llamacpp:prompt_tokens_total {prompt_tokens}\nllamacpp:tokens_predicted_total {predicted}\n"
                ),
                "text/plain",
            )
        }
    }

    fn write_http(stream: &mut TcpStream, body: &str, content_type: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// A plain mock server that answers every request immediately.
    fn spawn_plain_server(
        model: &'static str,
        n_ctx: u64,
        prompt_tokens: u64,
        predicted: u64,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Some(path) = read_request_path(&mut stream) {
                            let (body, content_type) =
                                mock_body(&path, model, n_ctx, prompt_tokens, predicted);
                            write_http(&mut stream, &body, content_type);
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    /// A mock server that signals when the first non-`/props` request arrives
    /// and then holds that response until `release` is flipped.
    fn spawn_gated_server(
        release: Arc<(Mutex<bool>, Condvar)>,
        started: mpsc::Sender<()>,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut signaled = false;
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        if path.contains("/props") {
                            let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                            write_http(&mut stream, &body, content_type);
                            continue;
                        }
                        if !signaled {
                            let _ = started.send(());
                            signaled = true;
                        }
                        // Hold the response until the test releases it.
                        let (lock, condvar) = &*release;
                        let mut released = lock.lock().unwrap();
                        while !*released {
                            released = condvar.wait(released).unwrap();
                        }
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_http(&mut stream, &body, content_type);
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    /// A mock server whose `/metrics` reports 501 (metrics disabled) while
    /// `/props` and `/slots` answer normally.
    fn spawn_metrics_off_server() -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        if path.contains("/metrics") {
                            let body = "metrics disabled";
                            let response = format!(
                                "HTTP/1.1 501 Not Implemented\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        } else {
                            let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                            write_http(&mut stream, &body, content_type);
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    /// Write a non-2xx HTTP response with a short body.
    fn write_http_status(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// A mock whose `/metrics` returns 501 on request `fail_on` and 200 with
    /// increasing counters otherwise; `/slots` is always 501 so the metric
    /// path (not the slot path) supplies the live rate.
    fn spawn_flaky_metrics_server(
        fail_on: usize,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let metrics_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        if path.contains("/metrics") {
                            let index = metrics_requests.fetch_add(1, Ordering::Relaxed) + 1;
                            if index == fail_on {
                                write_http_status(&mut stream, "501 Not Implemented", "disabled");
                            } else {
                                let prompt = 1000 * index as u64;
                                let predicted = 500 * index as u64;
                                let body = format!(
                                    "llamacpp:prompt_tokens_total {prompt}\nllamacpp:tokens_predicted_total {predicted}\n"
                                );
                                write_http(&mut stream, &body, "text/plain");
                            }
                        } else if path.contains("/slots") {
                            write_http_status(&mut stream, "501 Not Implemented", "no slots");
                        } else {
                            let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                            write_http(&mut stream, &body, content_type);
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    /// A mock whose `/metrics` returns 200 with an empty body while `/slots`
    /// answers normally.
    fn spawn_empty_metrics_server() -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        if path.contains("/metrics") {
                            write_http(&mut stream, "", "text/plain");
                        } else {
                            let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                            write_http(&mut stream, &body, content_type);
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    /// A mock whose `/metrics` returns 200 with the given body; `/slots` is 501.
    fn spawn_body_metrics_server(
        body: &'static str,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        if path.contains("/metrics") {
                            write_http(&mut stream, body, "text/plain");
                        } else if path.contains("/props") {
                            let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                            write_http(&mut stream, &body, content_type);
                        } else {
                            write_http_status(&mut stream, "501 Not Implemented", "no slots");
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    #[test]
    fn partial_metrics_payload_keeps_absent_metrics_unavailable_not_zero() {
        // Reachable /metrics with only a prompt counter and an explicit zero
        // request counter; every other metric must stay unavailable, not 0.
        let body = include_str!("../tests/fixtures/metrics_partial.prom");
        let (addr, stop, handle) = spawn_body_metrics_server(body);
        let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let stats = monitor.sample();
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        assert!(
            stats.metrics_available,
            "a partial payload is still available"
        );
        // Present metric -> real value; present zero -> real zero.
        assert_eq!(stats.prompt_total, Some(5000.0));
        assert_eq!(stats.active_requests, Some(0.0));
        // Absent metrics -> unavailable, never a fabricated zero.
        assert_eq!(stats.generated_total, None);
        assert_eq!(stats.prompt_seconds_total, None);
        assert_eq!(stats.prompt_avg_tps, None);
        assert_eq!(stats.generation_avg_tps, None);
        assert_eq!(stats.deferred_requests, None);
        assert_eq!(stats.context_high_watermark, None);
        assert_eq!(stats.spec_drafts_total, None);
        // No slot pairing and no aggregate counters -> live throughput —.
        assert_eq!((stats.prompt_tps, stats.generation_tps), (None, None));
    }

    #[test]
    fn metrics_disabled_still_exposes_slot_state() {
        // Regression: a 501 on /metrics must not discard a healthy /slots
        // response. The server is reachable; the slot/context state stays
        // available while the metric-derived values are marked unavailable.
        let (addr, stop, handle) = spawn_metrics_off_server();
        let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let stats = monitor.sample();
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        assert!(stats.connected, "a reachable /slots server is connected");
        assert!(
            !stats.metrics_available,
            "a 501 /metrics must mark metrics unavailable"
        );
        assert!(
            stats.slots_available,
            "/slots succeeded and must stay visible"
        );
        assert_eq!(stats.slot_count, 1);
        assert_eq!(stats.context_size, 4096);
        assert_eq!(stats.slot_overview.len(), 1);
        assert_eq!(stats.slot_overview[0].id, 0);
        assert!(
            stats.error.contains("--metrics"),
            "the metrics-disabled reason must still be surfaced: {:?}",
            stats.error
        );
    }

    #[test]
    fn empty_metrics_body_is_unavailable_not_a_fake_zero() {
        // A 2xx /metrics with no parseable sample must be treated as
        // unavailable, not as a real set of zeroes.
        let (addr, stop, handle) = spawn_empty_metrics_server();
        let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let stats = monitor.sample();
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        assert!(stats.connected, "/slots answered");
        assert!(
            !stats.metrics_available,
            "an empty /metrics body carries no metric data"
        );
        assert!(stats.slots_available);
        assert_eq!(stats.prompt_total, None);
        assert!(
            stats.error.contains("no metrics"),
            "the reason must be stated, got: {:?}",
            stats.error
        );
    }

    #[test]
    fn metrics_counter_baseline_resets_across_an_outage() {
        // Regression: a metrics outage must not leave a stale counter baseline
        // that turns the first post-outage sample into an average over the
        // whole outage. /slots is 501 throughout so the metric path decides.
        let (addr, stop, handle) = spawn_flaky_metrics_server(2);
        let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();

        let first = monitor.sample();
        assert!(first.metrics_available);
        // First sample establishes the baseline: no fabricated rate yet.
        assert_eq!((first.prompt_tps, first.generation_tps), (None, None));

        let outage = monitor.sample();
        assert!(!outage.metrics_available, "second /metrics request is 501");
        assert!(
            !outage.connected,
            "with /metrics and /slots both down the server is unreachable"
        );

        thread::sleep(Duration::from_millis(5));

        let recovered = monitor.sample();
        assert!(recovered.metrics_available);
        assert_eq!(
            (recovered.prompt_tps, recovered.generation_tps),
            (None, None),
            "the first sample after an outage must not average the delta over the outage"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    #[test]
    fn real_worker_tags_samples_per_server_across_a_set_change() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (a_addr, a_stop, a_handle) = spawn_gated_server(Arc::clone(&release), started_tx);
        let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 10_000, 20_000);

        let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
        let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(2_500));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(8);
        let (cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();

        let worker = spawn_llm_worker(
            vec![test_spec(&key_a, &format!("http://{a_addr}"))],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            cmd_rx,
        );

        // The real worker has an A request in flight, held by the mock.
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker must contact server A");

        // Replace the monitored set with B under a new generation.
        cmd_tx
            .send(LlmCommand::SetServers {
                generation: 1,
                servers: vec![test_spec(&key_b, &format!("http://{b_addr}"))],
            })
            .unwrap();

        // Release A's delayed response.
        {
            let (lock, condvar) = &*release;
            *lock.lock().unwrap() = true;
            condvar.notify_all();
        }

        // Consume published samples through the production acceptance path.
        let mut saw_delayed_a = false;
        let mut accepted_b = false;
        let mut saw_old_generation = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !(saw_delayed_a && accepted_b) {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                assert!(
                    sample.server == key_a || sample.server == key_b,
                    "every sample must carry a known server key"
                );
                if sample.server == key_a {
                    saw_delayed_a = true;
                    assert!(
                        !accepts_llm_sample(1, &sample),
                        "a delayed A result must be rejected under the new generation"
                    );
                }
                if accepts_llm_sample(1, &sample) && sample.server == key_b {
                    accepted_b = true;
                }
                if sample.generation != 1 {
                    saw_old_generation = true;
                }
            }
        }
        assert!(saw_delayed_a, "the delayed A result must be published");
        assert!(accepted_b, "B must be accepted under the new generation");
        assert!(
            saw_old_generation,
            "a pre-switch-generation sample must be observable"
        );

        // A buffered result from the previous generation stays rejected, and a
        // sample for a server no longer in the set is dropped by the membership
        // check (covered here for the generation half).
        let buffered_stale = LlmSample {
            server: key_a.clone(),
            generation: 0,
            stats: LlmStats {
                model: "model-A".to_string(),
                ..Default::default()
            },
        };
        assert!(!accepts_llm_sample(1, &buffered_stale));

        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
        a_stop.store(true, Ordering::Relaxed);
        b_stop.store(true, Ordering::Relaxed);
        let _ = a_handle.join();
        let _ = b_handle.join();
    }

    #[test]
    fn build_server_summaries_reflects_per_server_state() {
        let order = vec!["http://a:1/".to_string(), "http://b:2/".to_string()];
        let mut endpoints = BTreeMap::new();
        endpoints.insert("http://a:1/".to_string(), "http://a:1".to_string());
        endpoints.insert("http://b:2/".to_string(), "http://b:2".to_string());
        let mut registry = BTreeMap::new();
        registry.insert(
            "http://a:1/".to_string(),
            LlmStats {
                connected: true,
                model: "model-A".to_string(),
                slots_available: true,
                slot_count: 2,
                busy_slots: 1,
                context_used: Some(10),
                context_size: 100,
                ..Default::default()
            },
        );

        let summaries = build_server_summaries(&order, &endpoints, &registry);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].label, "model-A");
        assert!(summaries[0].connected);
        assert!(summaries[0].slots_available);
        assert_eq!(summaries[0].slot_count, 2);
        assert_eq!(summaries[0].busy_slots, 1);
        assert_eq!(summaries[0].context_used, Some(10));
        // Server B has no telemetry yet: offline with no fabricated zeros.
        assert!(!summaries[1].connected);
        assert!(!summaries[1].slots_available);
        assert_eq!(summaries[1].slot_count, 0);
        assert_eq!(summaries[1].context_used, None);
    }

    #[test]
    fn build_server_summaries_distinguishes_unavailable_slots_from_real_zero() {
        // The reported M4 bug: a connected server whose /slots endpoint is down
        // (but /metrics answers) must not collapse to "0/0" slots. Availability
        // is propagated explicitly and never confused with a real empty array.
        let order = vec![
            "http://metrics-only:1/".to_string(),
            "http://empty:2/".to_string(),
            "http://healthy:3/".to_string(),
            "http://dead:4/".to_string(),
        ];
        let mut endpoints = BTreeMap::new();
        for key in &order {
            endpoints.insert(key.clone(), key.trim_end_matches('/').to_string());
        }
        let mut registry = BTreeMap::new();
        // /metrics up, /slots down: connected, but the slot count is unknown.
        registry.insert(
            "http://metrics-only:1/".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                slots_available: false,
                props_slot_count: 3,
                ..Default::default()
            },
        );
        // /slots answered a valid, explicitly empty array: this is a real zero.
        registry.insert(
            "http://empty:2/".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                slots_available: true,
                slot_count: 0,
                busy_slots: 0,
                ..Default::default()
            },
        );
        registry.insert(
            "http://healthy:3/".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                slots_available: true,
                slot_count: 4,
                busy_slots: 2,
                ..Default::default()
            },
        );
        // No telemetry at all (unreachable): also unavailable, never zero.
        registry.insert(
            "http://dead:4/".to_string(),
            LlmStats {
                connected: false,
                error: "connection refused".to_string(),
                ..Default::default()
            },
        );

        let summaries = build_server_summaries(&order, &endpoints, &registry);
        assert!(!summaries[0].slots_available, "metrics-only slots unknown");
        assert!(summaries[0].connected, "metrics-only server is connected");
        assert_eq!(
            summaries[0].props_slot_count, 3,
            "a known /props total is preserved even while /slots is down"
        );
        assert!(
            summaries[1].slots_available,
            "an explicit empty /slots array is a real zero"
        );
        assert_eq!(summaries[1].slot_count, 0);
        assert!(summaries[2].slots_available);
        assert_eq!((summaries[2].busy_slots, summaries[2].slot_count), (2, 4));
        assert!(!summaries[3].connected);
        assert!(!summaries[3].slots_available);
        // No cross-server contamination: each summary reflects only its own key.
        assert_ne!(summaries[2].slot_count, summaries[1].slot_count);
    }

    #[test]
    fn real_worker_polls_multiple_servers_independently() {
        let (a_addr, a_stop, a_handle) = spawn_plain_server("model-A", 4096, 1000, 500);
        let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 2000, 600);
        let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
        let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(2_500));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            vec![
                test_spec(&key_a, &format!("http://{a_addr}")),
                test_spec(&key_b, &format!("http://{b_addr}")),
            ],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            cmd_rx,
        );

        let mut a_model = None;
        let mut b_model = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && (a_model.is_none() || b_model.is_none()) {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                if sample.server == key_a {
                    a_model = Some(sample.stats.model.clone());
                }
                if sample.server == key_b {
                    b_model = Some(sample.stats.model.clone());
                }
            }
        }
        assert_eq!(a_model.as_deref(), Some("model-A"));
        assert_eq!(b_model.as_deref(), Some("model-B"));

        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
        a_stop.store(true, Ordering::Relaxed);
        b_stop.store(true, Ordering::Relaxed);
        let _ = a_handle.join();
        let _ = b_handle.join();
    }

    #[test]
    fn an_unreachable_server_does_not_block_a_healthy_one() {
        let (a_addr, a_stop, a_handle) = spawn_plain_server("model-A", 4096, 1000, 500);
        // Bind then drop a port so connections are refused immediately.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
        let key_b = crate::domain::server_key(&format!("http://{dead_addr}"));

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(2_500));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            vec![
                test_spec(&key_b, &format!("http://{dead_addr}")),
                test_spec(&key_a, &format!("http://{a_addr}")),
            ],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            cmd_rx,
        );

        // Server A (listed after the dead one) must still report its model.
        let mut a_model = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && a_model.is_none() {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                if sample.server == key_a && sample.stats.connected {
                    a_model = Some(sample.stats.model.clone());
                }
            }
        }
        assert_eq!(a_model.as_deref(), Some("model-A"));

        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
        a_stop.store(true, Ordering::Relaxed);
        let _ = a_handle.join();
    }

    #[test]
    fn rebuild_resets_monitor_on_a_same_endpoint_restart() {
        let spec = ServerSpec {
            key: "http://h:1/".to_string(),
            endpoint: "http://h:1".to_string(),
            identity: Some(LocalServerIdentity {
                pid: 100,
                start_time: 5,
            }),
        };
        let mut monitor = new_server_monitor(spec.clone());
        // Simulate a warm cache (a previous good sample).
        monitor.last_good = Some((LlmStats::default(), Instant::now()));

        // Same identity: the monitor and its per-server caches are preserved.
        let kept = rebuild_server_monitors(vec![monitor], vec![spec.clone()]);
        assert_eq!(kept.len(), 1);
        assert!(
            kept[0].last_good.is_some(),
            "an unchanged server keeps its state"
        );

        // Same endpoint, new process (new start time): the monitor is rebuilt,
        // dropping the stale caches instead of keeping them until the next
        // /props refresh.
        let restarted = ServerSpec {
            identity: Some(LocalServerIdentity {
                pid: 100,
                start_time: 6,
            }),
            ..spec
        };
        let reset = rebuild_server_monitors(kept, vec![restarted]);
        assert_eq!(reset.len(), 1);
        assert!(
            reset[0].last_good.is_none(),
            "a same-endpoint restart must reset the per-server caches"
        );
    }

    #[test]
    fn changed_identity_keys_detects_only_restarted_servers() {
        let a = ServerSpec {
            key: "a".to_string(),
            endpoint: "http://a".to_string(),
            identity: Some(LocalServerIdentity {
                pid: 1,
                start_time: 1,
            }),
        };
        let b = ServerSpec {
            key: "b".to_string(),
            endpoint: "http://b".to_string(),
            identity: Some(LocalServerIdentity {
                pid: 2,
                start_time: 1,
            }),
        };
        // Only A restarted (same key, new start time).
        let a2 = ServerSpec {
            identity: Some(LocalServerIdentity {
                pid: 1,
                start_time: 2,
            }),
            ..a.clone()
        };
        assert_eq!(
            changed_identity_keys(&[a.clone(), b.clone()], &[a2, b.clone()]),
            vec!["a".to_string()]
        );
        // A new or removed key is not a restart.
        assert!(
            changed_identity_keys(std::slice::from_ref(&a), std::slice::from_ref(&b)).is_empty()
        );
        assert!(changed_identity_keys(&[], std::slice::from_ref(&a)).is_empty());
    }

    #[test]
    fn a_slow_server_does_not_delay_a_healthy_servers_delivery() {
        // A is gated (holds its response); B answers immediately. B's sample
        // must reach the app while A is still hanging.
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, _started_rx) = mpsc::channel::<()>();
        let (a_addr, a_stop, a_handle) = spawn_gated_server(Arc::clone(&release), started_tx);
        let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 10_000, 20_000);
        let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
        let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(2_500));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            vec![
                test_spec(&key_a, &format!("http://{a_addr}")),
                test_spec(&key_b, &format!("http://{b_addr}")),
            ],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            cmd_rx,
        );

        let mut b_model = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && b_model.is_none() {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                if sample.server == key_b && sample.stats.connected {
                    b_model = Some(sample.stats.model.clone());
                }
            }
        }
        assert_eq!(
            b_model.as_deref(),
            Some("model-B"),
            "healthy B must be delivered while A hangs"
        );

        // Release A so the worker's scope can finish and shut down.
        {
            let (lock, condvar) = &*release;
            *lock.lock().unwrap() = true;
            condvar.notify_all();
        }
        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
        a_stop.store(true, Ordering::Relaxed);
        b_stop.store(true, Ordering::Relaxed);
        let _ = a_handle.join();
        let _ = b_handle.join();
    }

    #[test]
    fn idle_repaint_is_skipped_until_the_tick() {
        let start = Instant::now();
        let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);

        // The first frame always paints; with no state change the view is then
        // left untouched until the idle tick elapses.
        assert!(scheduler.should_paint(start));
        assert!(!scheduler.should_paint(start + Duration::from_millis(50)));
        assert!(!scheduler.should_paint(start + Duration::from_millis(249)));

        // A real change paints immediately instead of waiting for the tick.
        scheduler.mark_dirty();
        assert!(scheduler.should_paint(start + Duration::from_millis(100)));
        assert!(!scheduler.should_paint(start + Duration::from_millis(101)));
    }

    #[test]
    fn poll_timeout_never_delays_a_repaint_and_bounds_the_drain_latency() {
        let start = Instant::now();
        let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);

        // Dirty view: `event::poll` must not block, so the frame is drawn now.
        assert_eq!(scheduler.poll_timeout(start), Duration::ZERO);
        assert!(scheduler.should_paint(start));
        // Clean view: the poll is capped, so newly arrived samples are drained
        // promptly even when the next idle tick is still far away.
        assert_eq!(scheduler.poll_timeout(start), MAX_POLL_INTERVAL);
        assert_eq!(
            scheduler.poll_timeout(start + Duration::from_millis(240)),
            Duration::from_millis(10)
        );
    }

    /// Simulates the production loop for one second and returns how many times
    /// it painted. The loop advances by `poll_timeout`, exactly as the real
    /// `event::poll` bounds an iteration; `snapshot_interval` models the fast
    /// worker pushing a (repaint-forcing) snapshot on that cadence, and `None`
    /// models a loop that receives no new data at all.
    fn simulated_idle_redraws(snapshot_interval: Option<Duration>) -> usize {
        let one_second = Duration::from_secs(1);
        let start = Instant::now();
        let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);
        let mut now = Duration::ZERO;
        let mut next_snapshot = snapshot_interval;
        let mut redraws = 0usize;
        while now < one_second {
            if let (Some(interval), Some(at)) = (snapshot_interval, next_snapshot) {
                if now >= at {
                    scheduler.mark_dirty();
                    next_snapshot = Some(at + interval);
                }
            }
            if scheduler.should_paint(start + now) {
                redraws += 1;
            }
            now += scheduler.poll_timeout(start + now);
        }
        redraws
    }

    #[test]
    fn idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick() {
        // Before: an idle iteration was bounded by `event::poll(50 ms)` and the
        // loop painted every iteration => 20 full redraws per second.
        let before =
            Duration::from_secs(1).as_millis() as usize / MAX_POLL_INTERVAL.as_millis() as usize;
        assert_eq!(before, 20, "documented pre-change idle redraw rate");

        // After: with no state change and no input, only the idle tick paints.
        let idle = simulated_idle_redraws(None);
        assert!(idle < before, "idle redraws {idle} not below {before}");
        assert!(idle <= 4, "idle redraws {idle}/s exceed the ~4/s budget");

        // A fast snapshot on the default 1000 ms cadence lands on the idle tick,
        // so it adds nothing; a 100 ms cadence roughly doubles the rate. These
        // bound the real idle rate (not just the scheduler floor).
        assert_eq!(
            simulated_idle_redraws(Some(Duration::from_millis(1_000))),
            idle
        );
        let fast = simulated_idle_redraws(Some(Duration::from_millis(100)));
        assert_eq!(fast, 10, "100 ms snapshots should yield ~10 redraws/s");
        assert!(fast < before);
    }
}
