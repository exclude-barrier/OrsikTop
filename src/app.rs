use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Stdout,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
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
        ServerBackend, ServerSummary, SystemStats, MAX_MONITORED_SERVERS, MAX_REFRESH_MS,
        MIN_LLM_POLL_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
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
    let fast_drops = Arc::new(AtomicU64::new(0));
    let llm_drops = Arc::new(AtomicU64::new(0));
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
        Arc::clone(&fast_drops),
        server_identity_rx,
    );
    let llm_worker = spawn_llm_worker(
        server_list.clone(),
        Arc::clone(&refresh_shared),
        Arc::clone(&offline_grace_shared),
        Arc::clone(&stop),
        llm_tx,
        Arc::clone(&llm_drops),
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
        drain_try_recv(&fast_rx, |next| {
            apply_fast_snapshot(&mut snapshot, &mut ui_state, mapping_generation, next);
            scheduler.mark_dirty();
        })
        .map_err(|()| "fast monitoring worker stopped unexpectedly".to_string())?;
        drain_try_recv(&llm_rx, |next| {
            // Drop results produced for a server set we already replaced, or
            // for a server that is no longer known: they must not appear under
            // a different server's state.
            if !accepts_llm_sample(llm_generation, &next) {
                return;
            }
            if !endpoints.contains_key(&next.server) {
                return;
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
        })
        .map_err(|()| "LLM monitoring worker stopped unexpectedly".to_string())?;
        snapshot.servers =
            build_server_summaries(&order, &endpoints, &registry, &ui_state, Instant::now());
        snapshot.selected_server = selected.clone();
        ui_state.set_snapshot_drops(
            fast_drops.load(Ordering::Relaxed),
            llm_drops.load(Ordering::Relaxed),
        );

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
                    // Field navigation, editing and the server-list editor
                    // live in `UiState`; only an actual save request touches
                    // the runtime here.
                    if ui_state.settings_handle_key(key.code) {
                        match ui_state.settings_config() {
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
                        }
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
    let llm_join = join_bounded(llm_worker, "LLM monitoring", Duration::from_secs(5));
    let fast_join = join_bounded(fast_worker, "fast monitoring", Duration::from_secs(5));
    llm_join.and(fast_join)?;
    Ok(())
}

/// Drain every currently queued value without blocking.
///
/// `Ok(())` means the queue is empty. `Err(())` means the sender is gone: the
/// worker stopped, which must not be mistaken for a temporarily empty queue.
fn drain_try_recv<T>(rx: &Receiver<T>, mut apply: impl FnMut(T)) -> Result<(), ()> {
    loop {
        match rx.try_recv() {
            Ok(value) => apply(value),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
}

/// Send without blocking. A full channel keeps the newest UI state and counts
/// the dropped update; `false` means the receiver is gone and the worker must
/// stop.
fn try_send_snapshot<T>(tx: &SyncSender<T>, value: T, drops: &AtomicU64) -> bool {
    match tx.try_send(value) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            drops.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Wait up to `timeout` for a worker and preserve its actual outcome.
///
/// A panicked worker, a shutdown timeout and a broken watcher are distinct
/// errors. The watcher keeps a wedged worker from blocking the process
/// indefinitely; on timeout it is left detached.
fn join_bounded(
    handle: thread::JoinHandle<()>,
    name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let (done_tx, done_rx) = mpsc::channel::<thread::Result<()>>();
    thread::spawn(move || {
        let _ = done_tx.send(handle.join());
    });
    match done_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(format!("{name} worker panicked")),
        Err(RecvTimeoutError::Timeout) => Err(format!("{name} worker shutdown timed out")),
        Err(RecvTimeoutError::Disconnected) => Err(format!("{name} join watcher disconnected")),
    }
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
///
/// All new per-server fields are pure derivations from already-collected data:
/// the activity phase and backend come from that server's `LlmStats`, and the
/// measurement age / sample interval from its own per-server UI history — no
/// extra polling and no telemetry-semantics change.
fn build_server_summaries(
    order: &[String],
    endpoints: &BTreeMap<String, String>,
    registry: &BTreeMap<String, LlmStats>,
    ui_state: &UiState,
    now: Instant,
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
                // Absent telemetry leaves the phase unknown (`—`), never a
                // guessed `IDLE`; a server with evidence carries its phase.
                phase: stats.and_then(crate::llama::activity_phase),
                age_ms: ui_state.server_sample_age_ms(key, now),
                sample_interval_ms: ui_state.server_sample_interval_ms(key),
                backend: stats.map_or(ServerBackend::Unknown, |stats| stats.backend),
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
    drops: Arc<AtomicU64>,
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

            if !try_send_snapshot(&tx, snapshot, &drops) {
                break;
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
    drops: Arc<AtomicU64>,
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
                    if !try_send_snapshot(&tx, sample, &drops) {
                        disconnected = true;
                        break;
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

    mod soak;

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
    fn held_llm_sample_expires_after_the_offline_grace() {
        // The held sample is bounded: once the grace window has passed, the
        // failed sample is reported as offline and no held value survives.
        let now = Instant::now();
        let grace = Duration::from_millis(2500);
        let mut last_good = None;
        let good = LlmStats {
            connected: true,
            generation_tps: Some(55.0),
            context_used: Some(1000),
            ..LlmStats::default()
        };
        stabilize_llm_sample(good, &mut last_good, now, grace);
        let failed = || LlmStats {
            error: "cannot reach llama.cpp: timeout".to_string(),
            ..LlmStats::default()
        };
        let held = stabilize_llm_sample(failed(), &mut last_good, now + grace, grace);
        assert!(held.connected && held.reconnecting);
        assert_eq!(held.generation_tps, Some(55.0));

        let expired = stabilize_llm_sample(
            failed(),
            &mut last_good,
            now + grace + Duration::from_millis(1),
            grace,
        );
        assert!(!expired.connected && !expired.reconnecting);
        assert_eq!(expired.generation_tps, None);
        assert_eq!(expired.context_used, None);
        assert_eq!(crate::llama::activity_phase(&expired), None);
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
            Arc::new(AtomicU64::new(0)),
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

        let summaries = build_server_summaries(
            &order,
            &endpoints,
            &registry,
            &UiState::default(),
            Instant::now(),
        );
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

        let summaries = build_server_summaries(
            &order,
            &endpoints,
            &registry,
            &UiState::default(),
            Instant::now(),
        );
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
    fn build_server_summaries_keep_unevidenced_activity_unknown_and_match_the_panel() {
        use crate::domain::ActivityPhase;

        // Connected with /metrics only: no /slots, no request gauge and no
        // server-reported phase. The overview must say UNKNOWN (not IDLE), and
        // a held reconnecting sample must say RECONNECTING — in both cases the
        // same phase the selected-server panel derives (CFA-88).
        let order = vec!["bare".to_string(), "held".to_string(), "off".to_string()];
        let mut endpoints = BTreeMap::new();
        for key in &order {
            endpoints.insert(key.clone(), format!("http://{key}:8080"));
        }
        let bare = LlmStats {
            connected: true,
            metrics_available: true,
            ..Default::default()
        };
        let held = LlmStats {
            connected: true,
            reconnecting: true,
            metrics_available: true,
            slots_available: true,
            slot_count: 1,
            ..Default::default()
        };
        let off = LlmStats::default();
        let mut registry = BTreeMap::new();
        registry.insert("bare".to_string(), bare.clone());
        registry.insert("held".to_string(), held.clone());
        registry.insert("off".to_string(), off.clone());

        let summaries = build_server_summaries(
            &order,
            &endpoints,
            &registry,
            &UiState::default(),
            Instant::now(),
        );
        assert_eq!(summaries[0].phase, Some(ActivityPhase::Unknown));
        assert_eq!(summaries[1].phase, Some(ActivityPhase::Reconnecting));
        assert!(summaries[1].reconnecting && !summaries[1].connected);
        assert_eq!(summaries[2].phase, None);
        for (summary, stats) in summaries.iter().zip([&bare, &held, &off]) {
            assert_eq!(summary.phase, crate::llama::activity_phase(stats));
            let panel = crate::ui::llm_phase_label(stats);
            let overview = summary.phase.map_or("OFFLINE", ActivityPhase::label);
            assert_eq!(panel, overview, "panel and selector disagree");
        }
    }

    #[test]
    fn build_server_summaries_derives_phase_age_and_backend() {
        use crate::domain::ActivityPhase;
        use crate::strata::StrataPhase;

        let order = vec![
            "gen".to_string(),
            "quiet".to_string(),
            "strata".to_string(),
            "offline_reg".to_string(),
            "off".to_string(),
        ];
        let mut endpoints = BTreeMap::new();
        for key in &order {
            endpoints.insert(key.clone(), format!("http://{key}:8080"));
        }
        let mut registry = BTreeMap::new();
        registry.insert(
            "gen".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                generation_tps: Some(20.0),
                backend: ServerBackend::LlamaCpp,
                ..Default::default()
            },
        );
        // IDLE needs evidence: here a valid /slots array with no busy slot.
        registry.insert(
            "quiet".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                slots_available: true,
                slot_count: 1,
                backend: ServerBackend::LlamaCpp,
                ..Default::default()
            },
        );
        registry.insert(
            "strata".to_string(),
            LlmStats {
                connected: true,
                metrics_available: true,
                strata_phase: Some(StrataPhase::Reading),
                backend: ServerBackend::Strata,
                ..Default::default()
            },
        );
        // A registry entry that is currently not answering: its phase must be
        // unknown (`None`), never a guessed `IDLE`.
        registry.insert(
            "offline_reg".to_string(),
            LlmStats {
                connected: false,
                ..Default::default()
            },
        );
        // "off" has no registry entry at all: never sampled.

        let mut ui_state = UiState::default();
        for key in ["gen", "quiet", "strata"] {
            ui_state.record_llm_sample(
                key,
                "gen",
                &LlmStats {
                    connected: true,
                    metrics_available: true,
                    ..Default::default()
                },
            );
        }
        let now = Instant::now();
        let summaries = build_server_summaries(&order, &endpoints, &registry, &ui_state, now);

        assert_eq!(summaries[0].phase, Some(ActivityPhase::Generating));
        assert_eq!(summaries[1].phase, Some(ActivityPhase::Idle));
        assert_eq!(summaries[2].phase, Some(ActivityPhase::Prefill));
        assert_eq!(
            summaries[3].phase, None,
            "a not-answering registry entry stays unknown, not IDLE"
        );
        assert_eq!(
            summaries[4].phase, None,
            "never sampled stays unknown, not IDLE"
        );

        assert_eq!(summaries[0].backend, ServerBackend::LlamaCpp);
        assert_eq!(summaries[2].backend, ServerBackend::Strata);
        assert_eq!(summaries[4].backend, ServerBackend::Unknown);

        assert!(summaries[0].age_ms.is_some(), "sampled server has an age");
        assert!(
            summaries[3].age_ms.is_none(),
            "a not-answering server has no fresh age"
        );
        assert!(
            summaries[4].age_ms.is_none(),
            "a server that never answered has no age, never a fake 0"
        );
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
            Arc::new(AtomicU64::new(0)),
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
            Arc::new(AtomicU64::new(0)),
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
            Arc::new(AtomicU64::new(0)),
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

    // ------------------------------------------------------------------
    // CFA-35 — reproducible runtime performance baseline
    // ------------------------------------------------------------------
    //
    // Two benchmarks drive the *real* runtime paths (the synchronous
    // `LlamaMonitor` HTTP poll, the ratatui render closure over a full
    // 16-server dashboard, the production `RedrawScheduler` model and the
    // `/proc` process collector) and emit machine-readable
    // `ORSKTOP_BENCH|<key>=<value>` lines on the raw process stdout: libtest
    // captures `println!`, but a direct `stdout().write_all` still reaches the
    // `cargo test` log.
    //
    //  * `perf_baseline_benchmark_for_dirty_only_redraw_and_polling` — the cheap
    //    in-gate part. It exercises the poll path against blocking-accept mock
    //    servers, the render closure and TUI input latency, reports thread-local
    //    CPU, and reads *no* `/proc` process tables. It is small enough (~0.5 s)
    //    to run in every sanctioned gate, which is exactly what lets an
    //    independent `pc-project cargo test --all-targets --locked` run
    //    reproduce its numbers.
    //
    //  * `perf_baseline_soak_and_process_collection_benchmark` — `#[ignore]`d. It
    //    holds the expensive parts (the full host `/proc`+DRM process-table
    //    collection and a longer-operation window) that must not run in every
    //    gate. It needs a targeted, single-threaded invocation:
    //      cargo test --all-targets --locked -- --ignored --test-threads=1 \
    //        perf_baseline_soak_and_process_collection_benchmark
    //    The sanctioned `pc-project cargo test` gate only accepts the fixed
    //    argument list and cannot forward those flags, so running it needs an
    //    operator-approved benchmark runner; the limitation is recorded in
    //    `docs/PERFORMANCE_BASELINE.md`.
    //
    // Every timing is *reported*, never asserted: the tests assert only
    // structural invariants, so they cannot flake and they do not turn the
    // shared suite into a brittle wall-clock gate. The wall-clock figures are a
    // per-machine baseline (see `docs/PERFORMANCE_BASELINE.md`), not a CI
    // threshold.

    /// Assumed Linux `USER_HZ` for converting `/proc/.../stat` jiffies to
    /// microseconds. 100 Hz is the near-universal default; it is documented
    /// rather than probed so the benchmark has no extra dependency.
    const BENCH_CLK_TCK: f64 = 100.0;

    fn bench_emit(key: &str, value: impl std::fmt::Display) {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "ORSKTOP_BENCH|{key}={value}");
        let _ = out.flush();
    }

    /// `utime + stime` of the *calling thread* in clock ticks
    /// (`/proc/thread-self/stat`, fields 14+15). Reading `/proc/self/stat`
    /// instead would sum every thread of the parallel libtest process and report
    /// CPU this test did not spend; thread-self isolates it. Parsing resumes
    /// after the final `)` so a `comm` with spaces or parentheses cannot shift
    /// the fields.
    fn thread_cpu_ticks() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        Some(utime + stime)
    }

    fn bench_ticks_to_us(ticks: u64) -> u128 {
        (u128::from(ticks) * 1_000_000) / BENCH_CLK_TCK as u128
    }

    /// First numeric field of a `/proc/self/status` line (e.g. `VmRSS:`,
    /// `Threads:`).
    fn self_status_field(name: &str) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
    }

    /// Nearest-rank percentile of an ascending-sorted slice.
    fn bench_percentile(sorted: &[Duration], q: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let idx = (((sorted.len() - 1) as f64) * q).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn bench_mean(durations: &[Duration]) -> Duration {
        if durations.is_empty() {
            return Duration::ZERO;
        }
        let total: Duration = durations.iter().sum();
        total / durations.len() as u32
    }

    /// A mock server with a **blocking** `accept`. Unlike `spawn_plain_server`
    /// (non-blocking listener + 2 ms `WouldBlock` sleep), it answers as soon as a
    /// request arrives, so a measured poll cycle reflects OrsikTop's HTTP client
    /// work instead of the mock's sleep quantum. Stop it with
    /// `stop_blocking_mock`, which wakes the blocked `accept` with one
    /// connection.
    fn spawn_blocking_mock(
        model: &'static str,
        n_ctx: u64,
        prompt_tokens: u64,
        predicted: u64,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // The stop wake-up connects without sending a request;
                        // break before the 2 s read timeout would stall the join.
                        if stop_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Some(path) = read_request_path(&mut stream) {
                            let (body, content_type) =
                                mock_body(&path, model, n_ctx, prompt_tokens, predicted);
                            write_http(&mut stream, &body, content_type);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, stop, handle)
    }

    fn stop_blocking_mock(
        stop: &Arc<AtomicBool>,
        addr: SocketAddr,
        handle: thread::JoinHandle<()>,
    ) {
        stop.store(true, Ordering::Relaxed);
        // Wake the blocked `accept`; the loop then observes the stop flag.
        let _ = TcpStream::connect(addr);
        let _ = handle.join();
    }

    /// A representative full dashboard: `server_count` servers and a
    /// `process_count`-row process table (the "large process list" scenario).
    fn bench_dashboard(server_count: usize, process_count: u32) -> DashboardSnapshot {
        let processes: Vec<ProcessStats> = (0..process_count)
            .map(|i| ProcessStats {
                pid: 1000 + i,
                program: format!("worker-{i}"),
                command: format!("/usr/bin/worker-{i} --slot {i}"),
                cpu_pct: f64::from(i % 100),
                memory_bytes: u64::from(i + 1) * 1_048_576,
                threads: Some(((i % 16) + 1) as usize),
                start_time: 1_700_000_000 + u64::from(i),
                gpu: Vec::new(),
            })
            .collect();

        let order: Vec<String> = (0..server_count)
            .map(|i| format!("http://10.0.0.{i}:8080/"))
            .collect();
        let mut endpoints = BTreeMap::new();
        let mut registry = BTreeMap::new();
        for (i, key) in order.iter().enumerate() {
            endpoints.insert(key.clone(), key.trim_end_matches('/').to_string());
            registry.insert(
                key.clone(),
                LlmStats {
                    connected: true,
                    metrics_available: true,
                    slots_available: true,
                    slot_count: 4,
                    busy_slots: (i % 5) as u64,
                    context_used: Some(1000 + i as u64),
                    context_size: 8192,
                    model: format!("model-{i}"),
                    generation_tps: Some(20.0),
                    backend: ServerBackend::LlamaCpp,
                    ..Default::default()
                },
            );
        }
        let servers = build_server_summaries(
            &order,
            &endpoints,
            &registry,
            &UiState::default(),
            Instant::now(),
        );
        DashboardSnapshot {
            system: SystemStats {
                memory_used_bytes: 32 * 1024 * 1024 * 1024,
                memory_total_bytes: 64 * 1024 * 1024 * 1024,
                load_one: Some(1.0),
                load_five: Some(0.8),
                load_fifteen: Some(0.5),
                processes,
                ..Default::default()
            },
            servers,
            selected_server: order[0].clone(),
            llm: registry[&order[0]].clone(),
            ..Default::default()
        }
    }

    #[test]
    fn perf_baseline_benchmark_for_dirty_only_redraw_and_polling() {
        let wall_start = Instant::now();
        let cpu_start = thread_cpu_ticks();

        // Redraw frequency is a deterministic *model* of the production
        // `RedrawScheduler`, not a wall-clock measurement. Keys carry a `model_`
        // prefix so the document cannot present them as measured data; the real
        // assertion lives in
        // `idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick`.
        bench_emit("model_redraw_pre_optimization_per_s", 20);
        bench_emit("model_redraw_idle_per_s", simulated_idle_redraws(None));
        for interval_ms in [100u64, 250, 1000] {
            bench_emit(
                &format!("model_redraw_per_s_interval_{interval_ms}ms"),
                simulated_idle_redraws(Some(Duration::from_millis(interval_ms))),
            );
        }

        let snapshot = bench_dashboard(16, 400);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
        let mut ui_state = UiState::default();
        let render_frame = |terminal: &mut Terminal<ratatui::backend::TestBackend>,
                            ui_state: &mut UiState| {
            terminal
                .draw(|frame| {
                    ui::draw(
                        frame,
                        &snapshot.system,
                        &snapshot.llm,
                        &snapshot.gpu,
                        &snapshot.gpu_map,
                        &snapshot.servers,
                        &snapshot.selected_server,
                        &[],
                        ui_state,
                        "http://10.0.0.0:8080",
                        1000,
                        false,
                    )
                })
                .unwrap();
        };

        // First frame *in this test process* — not full program start: GPU
        // discovery, NVML init and sensor discovery run in `spawn_fast_worker`
        // before any frame is drawn and are not part of this measurement.
        let cold_start = Instant::now();
        render_frame(&mut terminal, &mut ui_state);
        bench_emit(
            "render_first_frame_in_process_us",
            cold_start.elapsed().as_micros(),
        );

        // Warm steady state. CPU comes from *this thread*, not the process: a
        // single ≤6 ms frame is below the 10 ms jiffy resolution, so the
        // per-frame CPU is derived from the sum over the whole loop.
        let warm_iterations = 60usize;
        let mut warm_render = Vec::with_capacity(warm_iterations);
        let render_cpu_before = thread_cpu_ticks();
        for _ in 0..warm_iterations {
            let frame_start = Instant::now();
            render_frame(&mut terminal, &mut ui_state);
            warm_render.push(frame_start.elapsed());
        }
        let render_cpu = render_cpu_before
            .zip(thread_cpu_ticks())
            .map(|(before, after)| after.saturating_sub(before));
        warm_render.sort();
        let warm_mean = bench_mean(&warm_render);
        bench_emit("render_warm_mean_us", warm_mean.as_micros());
        bench_emit(
            "render_warm_p95_us",
            bench_percentile(&warm_render, 0.95).as_micros(),
        );
        bench_emit(
            "render_warm_max_us",
            warm_render.last().map_or(0, |d| d.as_micros()),
        );
        bench_emit(
            "render_warm_max_fps",
            1_000_000u128 / warm_mean.as_micros().max(1),
        );
        if let Some(ticks) = render_cpu {
            // Thread CPU of this thread only, over the whole 60-frame loop: a
            // single ≤6 ms frame is below the 10 ms jiffy resolution, so the
            // per-frame CPU is *derived* from the sum (no per-frame jiffy
            // percentage is reported — at this window it would only show
            // quantisation).
            let cpu_us_per_frame = bench_ticks_to_us(ticks) as f64 / warm_iterations as f64;
            bench_emit("render_thread_cpu_ticks_total", ticks);
            bench_emit(
                "render_cpu_us_per_frame_derived",
                format!("{cpu_us_per_frame:.0}"),
            );
            // Derived idle-TUI CPU: redraws/s × CPU/frame. The pre-optimization
            // loop painted every ~50 ms poll iteration (20/s); the scheduler now
            // paints ~4/s idle. Labelled `derived_` because it multiplies a
            // *modelled* rate by the measured per-frame CPU.
            let idle_per_s = simulated_idle_redraws(None) as f64;
            bench_emit(
                "derived_idle_cpu_pct",
                format!("{:.3}", idle_per_s * cpu_us_per_frame / 1_000_000.0 * 100.0),
            );
            bench_emit(
                "derived_pre_optimization_cpu_pct",
                format!("{:.3}", 20.0 * cpu_us_per_frame / 1_000_000.0 * 100.0),
            );
        }
        if let Some(rss_kb) = self_status_field("VmRSS:") {
            bench_emit("rss_kb_after_render", rss_kb);
        }
        if let Some(threads) = self_status_field("Threads:") {
            bench_emit("threads_after_render", threads);
        }

        // TUI input latency: a selection change plus the immediate repaint it
        // triggers, repeated so a single scheduling hiccup cannot define the
        // figure. The loop's own terminal event poll (`event::poll`) is not
        // reproducible in a `TestBackend` harness, so this is the repaint share
        // only — documented in `docs/PERFORMANCE_BASELINE.md`.
        let input_reps = 30usize;
        let mut input_latencies = Vec::with_capacity(input_reps);
        for _ in 0..input_reps {
            let input_start = Instant::now();
            ui_state.move_process_selection(1, &snapshot.system.processes);
            render_frame(&mut terminal, &mut ui_state);
            input_latencies.push(input_start.elapsed());
        }
        input_latencies.sort();
        bench_emit(
            "input_to_frame_mean_us",
            bench_mean(&input_latencies).as_micros(),
        );
        bench_emit(
            "input_to_frame_p95_us",
            bench_percentile(&input_latencies, 0.95).as_micros(),
        );
        bench_emit(
            "input_to_frame_max_us",
            input_latencies.last().map_or(0, |d| d.as_micros()),
        );

        // HTTP poll latency for 1, 4 and 16 blocking-accept mock servers. One
        // cycle polls every server sequentially through `LlamaMonitor::sample`;
        // the mock answers on arrival, so the value is OrsikTop's client cost,
        // not a sleep quantum.
        for server_count in [1usize, 4, 16] {
            let mut addrs = Vec::new();
            let mut stops = Vec::new();
            let mut handles = Vec::new();
            for _ in 0..server_count {
                let (addr, stop, handle) = spawn_blocking_mock("model-A", 4096, 1000, 500);
                addrs.push(addr);
                stops.push(stop);
                handles.push(handle);
            }
            let mut monitors: Vec<LlamaMonitor> = addrs
                .iter()
                .map(|addr| LlamaMonitor::new(&format!("http://{addr}")).unwrap())
                .collect();
            // Discard one warm-up cycle (connection setup, first parse).
            for monitor in &mut monitors {
                let _ = monitor.sample();
            }
            let cycles = 8usize;
            let mut cycle_times = Vec::with_capacity(cycles);
            for _ in 0..cycles {
                let cycle_start = Instant::now();
                for monitor in &mut monitors {
                    let _ = monitor.sample();
                }
                cycle_times.push(cycle_start.elapsed());
            }
            cycle_times.sort();
            let cycle_mean = bench_mean(&cycle_times);
            bench_emit(
                &format!("poll_{server_count}_servers_cycle_mean_us"),
                cycle_mean.as_micros(),
            );
            bench_emit(
                &format!("poll_{server_count}_servers_cycle_max_us"),
                cycle_times.last().map_or(0, |d| d.as_micros()),
            );
            bench_emit(
                &format!("poll_{server_count}_servers_per_server_us"),
                cycle_mean.as_micros() / server_count as u128,
            );

            for ((stop, addr), handle) in stops.into_iter().zip(addrs).zip(handles) {
                stop_blocking_mock(&stop, addr, handle);
            }
        }

        // Whole-process footprint for this measurement window. RSS and thread
        // count are inherently process properties; they describe this test
        // process, not the production TUI (see the document's limitations).
        let wall = wall_start.elapsed();
        bench_emit("measurement_wall_ms", wall.as_millis());
        if let Some(rss_kb) = self_status_field("VmRSS:") {
            bench_emit("process_rss_kb", rss_kb);
        }
        if let Some(threads) = self_status_field("Threads:") {
            bench_emit("process_threads", threads);
        }
        if let (Some(cpu0), Some(cpu1)) = (cpu_start, thread_cpu_ticks()) {
            let ticks = cpu1.saturating_sub(cpu0);
            bench_emit("thread_cpu_ticks_total", ticks);
            if !wall.is_zero() {
                bench_emit(
                    "thread_cpu_pct_avg",
                    format!(
                        "{:.2}",
                        ticks as f64 / (wall.as_secs_f64() * BENCH_CLK_TCK) * 100.0
                    ),
                );
            }
        }

        // Host / test conditions, read from inside the run: the sanctioned
        // test-desktop helper only reports host name, toolchain and GPU state, so
        // the CPU model, memory, kernel and CPU count are captured here.
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            if let Some(model) = cpuinfo
                .lines()
                .find_map(|line| line.strip_prefix("model name"))
                .map(|rest| rest.trim_start_matches([':', ' ', '\t']).trim())
            {
                bench_emit("host_cpu_model", model);
            }
        }
        bench_emit(
            "host_logical_cpus",
            std::thread::available_parallelism().map_or(0, |n| n.get()),
        );
        if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            bench_emit("host_kernel", release.trim());
        }
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(kb) = meminfo
                .lines()
                .find(|line| line.starts_with("MemTotal:"))
                .and_then(|line| line.split_whitespace().nth(1))
            {
                bench_emit("host_mem_total_kb", kb);
            }
        }

        // Structural invariants only: the scenarios produced real data and the
        // dashboard is the intended size. No wall-clock threshold here.
        assert_eq!(snapshot.servers.len(), 16, "16-server scenario is built");
        assert_eq!(snapshot.system.processes.len(), 400);
        assert!(!warm_render.is_empty());
    }

    /// The heavy half of the CFA-35 baseline: the real `/proc`+DRM
    /// process-collector cycle and a longer-operation window that samples RSS
    /// and thread count over time. `#[ignore]`d on purpose — reading the whole
    /// host process table on every gate is too costly for the shared suite.
    /// Run it through an approved single-threaded benchmark runner:
    ///   cargo test --all-targets --locked -- --ignored --test-threads=1 \
    ///     perf_baseline_soak_and_process_collection_benchmark
    #[test]
    #[ignore = "heavy: reads the whole host /proc table + DRM fdinfo; run via an approved single-threaded runner, not in the shared gate"]
    fn perf_baseline_soak_and_process_collection_benchmark() {
        // Production fast-worker cycle: `refresh_processes_specifics` followed
        // by `collect_process_stats`. Both are timed, together and apart, so the
        // report does not understate the cycle by measuring only the collector.
        let mut system = System::new();
        let mut process_cache = ProcessCache::default();
        let mut drm_state = DrmSamplerState::default();
        let refresh_system = |system: &mut System| -> Duration {
            let start = Instant::now();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_memory()
                    .with_cpu()
                    .with_exe(UpdateKind::Always),
            );
            start.elapsed()
        };

        let mut refresh_times = Vec::new();
        let mut collect_times = Vec::new();
        let mut cycle_times = Vec::new();
        let mut process_count = 0usize;
        for _ in 0..3 {
            let refresh = refresh_system(&mut system);
            let collect_start = Instant::now();
            let processes = collect_process_stats(&system, &mut process_cache, &mut drm_state);
            let collect = collect_start.elapsed();
            process_count = processes.len();
            refresh_times.push(refresh);
            collect_times.push(collect);
            cycle_times.push(refresh + collect);
        }
        refresh_times.sort();
        collect_times.sort();
        cycle_times.sort();
        bench_emit("process_count", process_count);
        bench_emit(
            "process_refresh_mean_us",
            bench_mean(&refresh_times).as_micros(),
        );
        bench_emit(
            "process_collect_mean_us",
            bench_mean(&collect_times).as_micros(),
        );
        bench_emit(
            "process_collect_max_us",
            collect_times.last().map_or(0, |d| d.as_micros()),
        );
        bench_emit(
            "process_cycle_mean_us",
            bench_mean(&cycle_times).as_micros(),
        );
        bench_emit(
            "process_cycle_max_us",
            cycle_times.last().map_or(0, |d| d.as_micros()),
        );

        // Component split of the collector (CFA-36): the collector cycle is the
        // sum of the per-process thread-count read and the per-process DRM fd
        // scan. Both phases are re-timed here (over the same process table) so
        // an optimization targets the dominant one. Reported, never asserted.
        let mut thread_read_total = Duration::ZERO;
        let mut thread_read_failures = 0usize;
        for pid in system.processes().keys() {
            let start = Instant::now();
            if read_process_thread_count(pid.as_u32()).is_none() {
                thread_read_failures += 1;
            }
            thread_read_total += start.elapsed();
        }
        bench_emit(
            "process_threads_read_total_us",
            thread_read_total.as_micros(),
        );
        bench_emit("process_threads_read_failures", thread_read_failures);

        let mut drm_scan_total = Duration::ZERO;
        let mut drm_procs_with_gpu = 0usize;
        let mut drm_scan_state = DrmSamplerState::default();
        let drm_now = Instant::now();
        for (pid, process) in system.processes().iter() {
            let identity = ProcessIdentity::new(pid.as_u32(), process.start_time());
            let start = Instant::now();
            let gpus = sample_process_gpus(&RealSys, drm_now, identity, &mut drm_scan_state);
            drm_scan_total += start.elapsed();
            if !gpus.is_empty() {
                drm_procs_with_gpu += 1;
            }
        }
        bench_emit("process_drm_scan_total_us", drm_scan_total.as_micros());
        bench_emit("process_drm_procs_with_gpu", drm_procs_with_gpu);

        // Longer-operation window: render the full dashboard continuously and
        // sample RSS / threads once per second to expose growth over time. This
        // is the closest a unit harness gets to "longer monitoring operation"; a
        // full soak belongs to EPIC 4.1.
        let snapshot = bench_dashboard(16, 400);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
        let mut ui_state = UiState::default();
        let soak_window = Duration::from_secs(10);
        let soak_start = Instant::now();
        let mut soak_frames = 0u64;
        let mut next_sample_s = 1u64;
        while soak_start.elapsed() < soak_window {
            terminal
                .draw(|frame| {
                    ui::draw(
                        frame,
                        &snapshot.system,
                        &snapshot.llm,
                        &snapshot.gpu,
                        &snapshot.gpu_map,
                        &snapshot.servers,
                        &snapshot.selected_server,
                        &[],
                        &mut ui_state,
                        "http://10.0.0.0:8080",
                        1000,
                        false,
                    )
                })
                .unwrap();
            soak_frames += 1;
            let elapsed_s = soak_start.elapsed().as_secs();
            if elapsed_s >= next_sample_s {
                if let Some(rss_kb) = self_status_field("VmRSS:") {
                    bench_emit(&format!("soak_rss_kb_at_{elapsed_s}s"), rss_kb);
                }
                if let Some(threads) = self_status_field("Threads:") {
                    bench_emit(&format!("soak_threads_at_{elapsed_s}s"), threads);
                }
                next_sample_s = elapsed_s + 1;
            }
        }
        bench_emit("soak_frames_total", soak_frames);
        bench_emit("soak_window_s", soak_window.as_secs());

        assert!(process_count > 0, "host process table is non-empty");
        assert!(soak_frames > 0, "soak rendered at least one frame");
    }

    // -----------------------------------------------------------------
    // EPIC 4.1 (CFA-37): mock-based soak / failure-mode harness
    // -----------------------------------------------------------------
    //
    // A bounded endurance harness that drives the *real* `spawn_llm_worker`
    // against programmable loopback mocks. Success is signalled by observed
    // samples, never by a wall-clock threshold; the deadlines only bound
    // failure. Each mock carries a distinct model, so a sample that reports a
    // foreign model is caught as cross-server mixing. No real inference server
    // is contacted.

    /// Failure modes a `ProgrammableMock` can be switched into at runtime. The
    /// mode is stored as the enum (behind a `Mutex`), never as its raw
    /// discriminant, so reordering this enum cannot silently mis-decode a mode.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum MockMode {
        Healthy,
        /// Accept the request, then close without answering (connection drop).
        Disconnect,
        /// Answer correctly, but only after `SLOW_RESPONSE` — a slow server
        /// still inside the client's request timeout.
        Slow,
        /// Accept and read the request, then hold the connection open until the
        /// mode changes or `stop` is set, so the client aborts with a
        /// **timeout** error rather than a connection drop (the real "reconnect
        /// after timeout" path). `LLM_REQUEST_TIMEOUT_MS` (1200 ms,
        /// `src/llama.rs`) is the only thing that ends the request in practice;
        /// `HANG_SAFETY_LIMIT` is a last-resort cap.
        Hang,
        /// 200 with a malformed JSON body (or unrecognised metrics text).
        BadJson,
        /// 200 with an empty body.
        Empty,
        /// 501 on `/metrics` (metrics disabled), healthy elsewhere.
        MetricsOff,
    }

    /// Delayed-but-successful response, kept well below the client's request
    /// timeout: a slow server, not a hung one.
    const SLOW_RESPONSE: Duration = Duration::from_millis(150);
    /// Safety cap on how long `MockMode::Hang` holds a connection. The hold is
    /// released as soon as the mode changes or `stop` is set, so in practice
    /// only the *client's* request timeout (`LLM_REQUEST_TIMEOUT_MS` = 1200 ms
    /// in `src/llama.rs`) ends a request — a fixed self-close would let a client
    /// with a longer timeout pass by seeing a connection drop instead of a real
    /// timeout. This cap only keeps a mis-wired test from hanging forever.
    const HANG_SAFETY_LIMIT: Duration = Duration::from_secs(30);

    /// A loopback mock whose behaviour can be changed while it runs.
    struct ProgrammableMock {
        addr: SocketAddr,
        mode: Arc<Mutex<MockMode>>,
        /// Monotonic request counter driving the fake `/metrics` counters.
        /// `restart_counters()` resets it to zero to model a server restart
        /// whose cumulative counters begin again at zero.
        requests: Arc<AtomicU64>,
        /// Number of requests currently held open by `MockMode::Hang`. Lets a
        /// test wait until a request is genuinely in flight before it stops the
        /// worker, instead of racing the next poll cycle.
        hang_active: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl ProgrammableMock {
        fn base(&self) -> String {
            let addr = self.addr;
            format!("http://{addr}")
        }

        fn set_mode(&self, mode: MockMode) {
            *self.mode.lock().expect("mock mode lock") = mode;
        }

        /// Number of requests the mock is currently holding open in
        /// `MockMode::Hang` (0 once the mode changes or `stop` is set).
        fn hang_active(&self) -> u64 {
            self.hang_active.load(Ordering::SeqCst)
        }

        /// Reset the fake cumulative counters to zero **without** changing the
        /// endpoint, modelling a server whose counters restart while the model
        /// stays the same. A monitor that keeps polling this endpoint must then
        /// see a reset (rate unavailable), never a negative or absurd rate.
        fn restart_counters(&self) {
            self.requests.store(0, Ordering::SeqCst);
        }
    }

    impl Drop for ProgrammableMock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake a blocked `accept`; the loop then observes `stop` and exits.
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve_healthy(
        stream: &mut TcpStream,
        path: &str,
        model: &str,
        n_ctx: u64,
        counter_base: u64,
        index: u64,
    ) {
        if path.contains("/props") {
            let body = format!(
                "{{\"model_path\":\"{model}\",\"total_slots\":1,\"default_generation_settings\":{{\"n_ctx\":{n_ctx}}}}}"
            );
            write_http(stream, &body, "application/json");
        } else if path.contains("/slots") {
            let body = format!("[{{\"id\":0,\"n_ctx\":{n_ctx},\"is_processing\":false}}]");
            write_http(stream, &body, "application/json");
        } else {
            let prompt = counter_base + index * 100;
            let predicted = counter_base + index * 50;
            let body =
                format!("llamacpp:prompt_tokens_total {prompt}\nllamacpp:tokens_predicted_total {predicted}\n");
            write_http(stream, &body, "text/plain");
        }
    }

    fn serve_bad_json(stream: &mut TcpStream, path: &str) {
        if path.contains("/metrics") {
            // Not a recognised metrics sample -> metrics unavailable.
            write_http(stream, "this is not a metrics body", "text/plain");
        } else {
            // Malformed JSON for /props and /slots.
            write_http(stream, "{ this is not json", "application/json");
        }
    }

    fn programmable_mock(model: String, n_ctx: u64, counter_base: u64) -> ProgrammableMock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind programmable mock");
        let addr = listener.local_addr().expect("addr");
        let mode = Arc::new(Mutex::new(MockMode::Healthy));
        let requests = Arc::new(AtomicU64::new(0));
        let hang_active = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let mode_flag = Arc::clone(&mode);
        let requests_flag = Arc::clone(&requests);
        let hang_flag = Arc::clone(&hang_active);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                // The stop wake-up connects without a request; break before the
                // read timeout would stall the join.
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let Some(path) = read_request_path(&mut stream) else {
                    continue;
                };
                let current = *mode_flag.lock().expect("mock mode lock");
                match current {
                    MockMode::Disconnect => drop(stream),
                    MockMode::Hang => {
                        // Hold the connection open for as long as the mode stays
                        // `Hang` and `stop` is unset, so only the *client's*
                        // request timeout can end the request. Waking on `stop`
                        // and re-checking the mode keep teardown and recovery
                        // bounded; `HANG_SAFETY_LIMIT` is a last-resort cap for a
                        // mis-wired test, never the normal exit. No response is
                        // ever written.
                        hang_flag.fetch_add(1, Ordering::SeqCst);
                        let safety = Instant::now() + HANG_SAFETY_LIMIT;
                        while !stop_flag.load(Ordering::SeqCst)
                            && Instant::now() < safety
                            && *mode_flag.lock().expect("mock mode lock") == MockMode::Hang
                        {
                            thread::sleep(Duration::from_millis(25));
                        }
                        hang_flag.fetch_sub(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    mode => {
                        let index = requests_flag.fetch_add(1, Ordering::SeqCst) + 1;
                        match mode {
                            MockMode::Slow => {
                                thread::sleep(SLOW_RESPONSE);
                                serve_healthy(
                                    &mut stream,
                                    &path,
                                    &model,
                                    n_ctx,
                                    counter_base,
                                    index,
                                );
                            }
                            MockMode::BadJson => serve_bad_json(&mut stream, &path),
                            MockMode::Empty => {
                                write_http(&mut stream, "", "application/json");
                            }
                            MockMode::MetricsOff => {
                                if path.contains("/metrics") {
                                    write_http_status(
                                        &mut stream,
                                        "501 Not Implemented",
                                        "disabled",
                                    );
                                } else {
                                    serve_healthy(
                                        &mut stream,
                                        &path,
                                        &model,
                                        n_ctx,
                                        counter_base,
                                        index,
                                    );
                                }
                            }
                            MockMode::Healthy => {
                                serve_healthy(
                                    &mut stream,
                                    &path,
                                    &model,
                                    n_ctx,
                                    counter_base,
                                    index,
                                );
                            }
                            // Handled above.
                            MockMode::Disconnect | MockMode::Hang => unreachable!(),
                        }
                    }
                }
            }
        });
        ProgrammableMock {
            addr,
            mode,
            requests,
            hang_active,
            stop,
            handle: Some(handle),
        }
    }

    /// A sample for `key` must never carry another server's model or context
    /// size. `model` is empty or the explicit unknown label until `/props`
    /// succeeds, so only a *known* model is checked; a known model implies
    /// `/props` answered, so its `context_size` is checked alongside it (`n_ctx`
    /// is a monotonic identity of the mock, unlike the model name alone).
    fn assert_no_cross_mixing(
        sample: &LlmSample,
        key: &str,
        expected_model: &str,
        expected_ctx: u64,
    ) {
        if sample.server != key {
            return;
        }
        let model = sample.stats.model.as_str();
        if model.is_empty() || model == crate::llama::UNKNOWN_MODEL_LABEL {
            return;
        }
        assert_eq!(
            model, expected_model,
            "server {key} reported the foreign model {model:?} (cross-server mixing)"
        );
        assert_eq!(
            sample.stats.context_size, expected_ctx,
            "server {key} reported the foreign context size {} (cross-server mixing)",
            sample.stats.context_size
        );
    }

    /// Start the real worker against `specs` with the given offline grace and a
    /// 250 ms poll floor; returns the handles a soak test drives.
    #[allow(clippy::type_complexity)]
    fn start_soak_worker(
        specs: Vec<ServerSpec>,
        offline_ms: u64,
    ) -> (
        Arc<AtomicBool>,
        mpsc::Receiver<LlmSample>,
        mpsc::Sender<LlmCommand>,
        thread::JoinHandle<()>,
    ) {
        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(offline_ms));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
        let (cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            specs,
            refresh,
            offline,
            Arc::clone(&stop),
            sample_tx,
            Arc::new(AtomicU64::new(0)),
            cmd_rx,
        );
        (stop, sample_rx, cmd_tx, worker)
    }

    /// Join `worker` off-thread so a hang cannot wedge the test, returning
    /// whether it finished within `bound`.
    fn join_within(worker: thread::JoinHandle<()>, bound: Duration) -> bool {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let joiner = thread::spawn(move || {
            let _ = worker.join();
            let _ = done_tx.send(());
        });
        let joined = done_rx.recv_timeout(bound).is_ok();
        let _ = joiner.join();
        joined
    }

    /// Drain samples until `key` reports connected with its own `model`, or
    /// panic at the deadline (a timeout bounds failure, never success).
    fn wait_for_connected(
        samples: &mpsc::Receiver<LlmSample>,
        key: &str,
        model: &str,
        deadline: Duration,
    ) {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(200)) {
                if sample.server == key && sample.stats.connected && sample.stats.model == model {
                    return;
                }
            }
        }
        panic!("server {key} never reported connected with model {model}");
    }

    #[test]
    fn soak_worker_survives_malformed_json_and_recovers_without_mixing() {
        // A answers malformed JSON, B is healthy. B's telemetry must stay its
        // own while A is broken, and A must recover to its own model once the
        // endpoint is healthy again — never borrowing B's.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let b = programmable_mock("model-B".to_string(), 8192, 7_000);
        a.set_mode(MockMode::BadJson);

        let key_a = crate::domain::server_key(&a.base());
        let key_b = crate::domain::server_key(&b.base());

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(300));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            Arc::new(AtomicU64::new(0)),
            cmd_rx,
        );

        // Phase 1: B reports its own model while A is malformed.
        let mut b_ok = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !b_ok {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                let server = sample.server.clone();
                assert!(
                    sample.server == key_a || sample.server == key_b,
                    "unknown server key published: {server}"
                );
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                if sample.server == key_b
                    && sample.stats.connected
                    && sample.stats.model == "model-B"
                {
                    b_ok = true;
                }
            }
        }
        assert!(
            b_ok,
            "the healthy server must report its own model while A is malformed"
        );

        // Phase 2: A recovers to its own model.
        a.set_mode(MockMode::Healthy);
        let mut a_ok = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !a_ok {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                if sample.server == key_a
                    && sample.stats.connected
                    && sample.stats.model == "model-A"
                {
                    a_ok = true;
                }
            }
        }
        assert!(
            a_ok,
            "A must recover to its own model after the malformed period"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = worker.join();
    }

    #[test]
    fn soak_polls_the_maximum_monitored_server_count_independently() {
        // Every one of MAX_MONITORED_SERVERS servers must be polled, each with
        // its own model — the cap must not silently drop or cross-wire a server.
        let mocks: Vec<ProgrammableMock> = (0..MAX_MONITORED_SERVERS)
            .map(|i| programmable_mock(format!("model-{i}"), 4096, 1_000 * (i as u64 + 1)))
            .collect();
        let specs: Vec<ServerSpec> = mocks
            .iter()
            .map(|mock| {
                let key = crate::domain::server_key(&mock.base());
                test_spec(&key, &mock.base())
            })
            .collect();
        let keys: Vec<String> = specs.iter().map(|spec| spec.key.clone()).collect();

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(300));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            specs,
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            Arc::new(AtomicU64::new(0)),
            cmd_rx,
        );

        // Only a sample that is actually *connected* and carries this server's
        // own model counts as "polled", so a worker that published nothing but
        // error samples for every server cannot pass; each mock has a unique
        // model, and all share the same n_ctx.
        let mut seen: HashSet<String> = HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && seen.len() < keys.len() {
            if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
                let server = sample.server.clone();
                let idx = keys
                    .iter()
                    .position(|key| *key == sample.server)
                    .unwrap_or_else(|| panic!("unknown server key published: {server}"));
                assert_no_cross_mixing(&sample, &keys[idx], &format!("model-{idx}"), 4096);
                if sample.stats.connected && sample.stats.model == format!("model-{idx}") {
                    seen.insert(sample.server);
                }
            }
        }
        let seen_len = seen.len();
        let key_count = keys.len();
        assert_eq!(
            seen_len, key_count,
            "all {MAX_MONITORED_SERVERS} servers must be polled, saw {seen_len} of {key_count}"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = worker.join();
    }

    #[test]
    fn soak_clean_shutdown_is_bounded_while_a_server_hangs() {
        // The critical blocking-shutdown case is a *hung* server whose request
        // is in flight for the whole client timeout, not a merely slow one:
        // joining the worker after `stop` must still complete within a bound,
        // because the in-flight request is cut off by `LLM_REQUEST_TIMEOUT_MS`.
        // The mock is held open until `stop`, so `hang_active()` proves a
        // request is genuinely in flight when shutdown begins — no race against
        // the next poll cycle.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        a.set_mode(MockMode::Hang);
        let key_a = crate::domain::server_key(&a.base());

        let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
        let offline = Arc::new(AtomicU64::new(300));
        let stop = Arc::new(AtomicBool::new(false));
        let (sample_tx, _sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
        let worker = spawn_llm_worker(
            vec![test_spec(&key_a, &a.base())],
            Arc::clone(&refresh),
            Arc::clone(&offline),
            Arc::clone(&stop),
            sample_tx,
            Arc::new(AtomicU64::new(0)),
            cmd_rx,
        );

        // Wait until the mock actually holds a request open.
        let deadline = Instant::now() + Duration::from_secs(5);
        while a.hang_active() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            a.hang_active() > 0,
            "the mock never held a request open before shutdown"
        );

        stop.store(true, Ordering::SeqCst);
        assert!(
            join_within(worker, Duration::from_secs(5)),
            "the worker did not shut down within 5 s (blocking shutdown)"
        );
    }

    #[test]
    fn soak_reconnects_after_request_timeout() {
        // A server that holds the connection open past the client's request
        // timeout must fail (not merely answer late), and must recover to
        // connected with its own model once it answers again — the real
        // "reconnect after timeout" path. `MockMode::Slow` is below the timeout,
        // so only `Hang` actually triggers a client timeout. `Hang` holds the
        // connection until the mode changes, so the disconnect seen in phase 1
        // can only be the client's own `LLM_REQUEST_TIMEOUT_MS` firing — a client
        // with no or a longer timeout would stay blocked here, not observe a
        // connection drop.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        a.set_mode(MockMode::Hang);
        let key_a = crate::domain::server_key(&a.base());

        let (stop, samples, _cmds, worker) =
            start_soak_worker(vec![test_spec(&key_a, &a.base())], 300);

        // Phase 1: the hung server must not report a (stale) connected sample.
        let mut saw_timeout = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !saw_timeout {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                if sample.server == key_a && !sample.stats.connected {
                    saw_timeout = true;
                }
            }
        }
        assert!(
            saw_timeout,
            "a request held past the timeout must be reported as disconnected"
        );

        // Phase 2: once the server answers, it must recover to its own model.
        a.set_mode(MockMode::Healthy);
        wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
    }

    #[test]
    fn soak_counter_reset_is_reported_as_unavailable_not_a_fantasy_rate() {
        // A restarted llama.cpp restarts its cumulative counters at zero. The
        // monitor must treat that as a reset (live rate unavailable), never as a
        // negative or absurd delta, and re-establish a fresh baseline. The
        // endpoint (and identity) stays the same, so the monitor observes the
        // backwards counters directly.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let key_a = crate::domain::server_key(&a.base());

        let (stop, samples, _cmds, worker) =
            start_soak_worker(vec![test_spec(&key_a, &a.base())], 300);

        // Establish a live generation rate first.
        let mut saw_rate = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !saw_rate {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                if sample.stats.connected && sample.stats.generation_tps.is_some() {
                    saw_rate = true;
                }
            }
        }
        assert!(saw_rate, "a live generation rate must be established first");

        // Restart the counters at zero without changing the endpoint.
        a.restart_counters();
        let mut saw_reset = false;
        let mut saw_fresh = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !(saw_reset && saw_fresh) {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                for (label, value) in [
                    ("prompt_tps", sample.stats.prompt_tps),
                    ("generation_tps", sample.stats.generation_tps),
                ] {
                    if let Some(value) = value {
                        assert!(
                            value.is_finite() && (0.0..1e9).contains(&value),
                            "a counter reset produced a fantasy {label}={value}"
                        );
                    }
                }
                if sample.stats.connected && sample.stats.generation_tps.is_none() {
                    saw_reset = true;
                }
                // Only a *post-reset* live rate proves a fresh baseline was
                // re-established. A rate fetched before the reset (still in
                // flight when `restart_counters()` ran) must not count, or the
                // test could pass without ever observing the reset recovery.
                if saw_reset && sample.stats.connected && sample.stats.generation_tps.is_some() {
                    saw_fresh = true;
                }
            }
        }
        assert!(
            saw_reset,
            "the counter reset must report an unavailable rate"
        );
        assert!(saw_fresh, "a fresh baseline must re-establish a live rate");

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
    }

    #[test]
    fn soak_set_servers_churn_keeps_generations_and_polling_bounded() {
        // Repeated `SetServers` rounds: the generation must never go backwards
        // (a stale sample from a replaced set must not surface after a newer
        // one), the added server must be polled under the new generation, a
        // removed server must stop, and a same-key restart (new identity + a
        // counter reset) must resume with the server's own values.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let b = programmable_mock("model-B".to_string(), 8192, 2_000);
        let c = programmable_mock("model-C".to_string(), 16384, 3_000);
        let key_a = crate::domain::server_key(&a.base());
        let key_b = crate::domain::server_key(&b.base());
        let key_c = crate::domain::server_key(&c.base());

        let (stop, samples, cmds, worker) = start_soak_worker(
            vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
            300,
        );

        let mut last_generation = 0u64;
        // Phase 1: settle the initial A+B set at generation 0.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut established = false;
        while Instant::now() < deadline && !established {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                assert!(
                    sample.generation >= last_generation,
                    "generation went backwards ({} after {})",
                    sample.generation,
                    last_generation
                );
                last_generation = sample.generation;
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                if sample.server == key_b
                    && sample.stats.connected
                    && sample.stats.model == "model-B"
                {
                    established = true;
                }
            }
        }
        assert!(established, "the initial A+B set must be polled");

        // Phase 2: switch to a new set {A (new identity), C} at generation 1.
        // A keeps its key but its identity changed, so its monitor is rebuilt;
        // its counters restart at zero. B is removed, C is added.
        a.restart_counters();
        cmds.send(LlmCommand::SetServers {
            generation: 1,
            servers: vec![
                ServerSpec {
                    identity: Some(LocalServerIdentity {
                        pid: 999,
                        start_time: 7,
                    }),
                    ..test_spec(&key_a, &a.base())
                },
                test_spec(&key_c, &c.base()),
            ],
        })
        .unwrap();

        let mut a_gen1 = false;
        let mut c_gen1 = false;
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline && !(a_gen1 && c_gen1) {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                assert!(
                    sample.generation >= last_generation,
                    "generation went backwards ({} after {})",
                    sample.generation,
                    last_generation
                );
                last_generation = last_generation.max(sample.generation);
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_c, "model-C", 16384);
                if sample.generation == 1
                    && sample.server == key_a
                    && sample.stats.connected
                    && sample.stats.model == "model-A"
                {
                    a_gen1 = true;
                }
                if sample.generation == 1
                    && sample.server == key_c
                    && sample.stats.connected
                    && sample.stats.model == "model-C"
                {
                    c_gen1 = true;
                }
            }
        }
        assert!(
            a_gen1,
            "the restarted server A must resume with its own values under generation 1"
        );
        assert!(
            c_gen1,
            "the newly added server C must be polled under generation 1"
        );

        // Once the switch has settled, the removed server B must not be polled
        // under the new generation.
        thread::sleep(Duration::from_millis(600));
        let mut b_gen1 = 0usize;
        while let Ok(sample) = samples.try_recv() {
            assert!(sample.generation >= last_generation);
            if sample.server == key_b && sample.generation == 1 {
                b_gen1 += 1;
            }
        }
        assert_eq!(
            b_gen1, 0,
            "a server removed from the set must not be polled under the new generation"
        );

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
    }

    #[test]
    fn soak_selection_switch_keeps_each_servers_own_history() {
        // Partial integration coverage: drive the real worker and feed its
        // samples through the production selection seam
        // (`UiState::record_llm_sample` + `select_llm_history`) while the
        // selected server switches A -> B, and check that both servers keep an
        // independently measured sample interval (via the public accessor) after
        // the switch. It does **not** assert the *active* history's content, nor
        // prove no cross-mixing: the interval accessor cannot tell whose samples
        // built a history, and background servers accumulate their interval
        // regardless of selection. Those properties are covered at the unit
        // level by `ui::tests::per_server_histories_are_independent_and_survive_selection`
        // and the `assert_no_cross_mixing` checks in the failure-mode tests here.
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let b = programmable_mock("model-B".to_string(), 8192, 9_000);
        let key_a = crate::domain::server_key(&a.base());
        let key_b = crate::domain::server_key(&b.base());

        let (stop, samples, _cmds, worker) = start_soak_worker(
            vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
            300,
        );

        let mut ui = UiState::default();
        let mut selected = key_a.clone();
        ui.select_llm_history(&selected);

        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline
            && !(ui.server_sample_interval_ms(&key_a).is_some()
                && ui.server_sample_interval_ms(&key_b).is_some())
        {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                ui.record_llm_sample(&sample.server, &selected, &sample.stats);
                // Once A has its own measured interval, switch the selection.
                if selected == key_a && ui.server_sample_interval_ms(&key_a).is_some() {
                    selected = key_b.clone();
                    ui.select_llm_history(&key_b);
                }
            }
        }
        assert!(
            ui.server_sample_interval_ms(&key_a).is_some(),
            "A must keep its own history across the selection switch"
        );
        assert!(
            ui.server_sample_interval_ms(&key_b).is_some(),
            "B must accumulate its own history after the switch"
        );

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
    }

    /// The longer half of EPIC 4.1: drives every failure mode across two servers
    /// over a sustained window, then asserts resilience (samples keep flowing,
    /// no cross-mixing), a real stale-state transition (a failure phase outlasts
    /// the grace hold, so the server is published as disconnected rather than
    /// holding a last-good forever), and recovery to each server's own values.
    /// Resource footprint is deliberately **not measured** here: `/proc/self` is
    /// process-global and the shared gate runs tests in parallel, so such a bound
    /// is meaningless, and this test asserts no resource values at all; the
    /// strict probe lives in the `#[ignore]`d single-threaded test below.
    #[test]
    fn soak_endurance_all_failure_modes_without_mixing_and_recovers() {
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let b = programmable_mock("model-B".to_string(), 8192, 2_000);

        let key_a = crate::domain::server_key(&a.base());
        let key_b = crate::domain::server_key(&b.base());
        let known = [key_a.clone(), key_b.clone()];

        // A short grace so a failure phase visibly outlasts the held last-good;
        // B flips between modes so the two servers run under mismatched
        // conditions (a mismatch cannot cross-wire them).
        let (stop, samples, _cmds, worker) = start_soak_worker(
            vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
            200,
        );

        let script = [
            (MockMode::Healthy, MockMode::Healthy, 500u64),
            (MockMode::Disconnect, MockMode::BadJson, 1_200),
            (MockMode::Empty, MockMode::MetricsOff, 500),
            (MockMode::Slow, MockMode::Disconnect, 500),
            (MockMode::Healthy, MockMode::Healthy, 1_200),
        ];

        let mut observed = 0usize;
        let mut saw_a_disconnected = false;
        for (a_mode, b_mode, hold_ms) in script {
            a.set_mode(a_mode);
            b.set_mode(b_mode);
            let phase_end = Instant::now() + Duration::from_millis(hold_ms);
            while Instant::now() < phase_end {
                if let Ok(sample) = samples.recv_timeout(Duration::from_millis(50)) {
                    let server = sample.server.clone();
                    assert!(
                        known.contains(&sample.server),
                        "unknown server key published: {server}"
                    );
                    assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                    assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                    if sample.server == key_a && !sample.stats.connected {
                        saw_a_disconnected = true;
                    }
                    observed += 1;
                }
            }
        }
        assert!(
            observed > 0,
            "the soak must observe samples (no endless/spinning cycle)"
        );
        assert!(
            saw_a_disconnected,
            "a failure phase longer than the grace must publish A as disconnected, not hold last-good forever"
        );

        // Recovery: both servers healthy again report their own values.
        a.set_mode(MockMode::Healthy);
        b.set_mode(MockMode::Healthy);
        let mut a_ok = false;
        let mut b_ok = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !(a_ok && b_ok) {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                if sample.server == key_a
                    && sample.stats.connected
                    && sample.stats.model == "model-A"
                {
                    a_ok = true;
                }
                if sample.server == key_b
                    && sample.stats.connected
                    && sample.stats.model == "model-B"
                {
                    b_ok = true;
                }
            }
        }
        assert!(
            a_ok && b_ok,
            "both servers must recover to their own values (a={a_ok}, b={b_ok})"
        );

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
    }

    /// Strict, deterministic leak probe for EPIC 4.1's "keine unkontrollierte
    /// Ressourcenentwicklung". `#[ignore]`d on purpose: `/proc/self/status`
    /// (`Threads:`, `VmRSS:`) is **process-global**, so in the shared parallel
    /// gate the values are dominated by the other concurrently running tests and
    /// no bound on them can detect a worker leak (this is why the in-gate
    /// endurance test measures no resources at all). Run it single-threaded via
    /// an approved runner so `/proc/self` is this test alone and the bounds are
    /// meaningful:
    ///   cargo test --all-targets --locked -- --ignored --test-threads=1 \
    ///     soak_many_set_servers_rounds_bound_resources_single_threaded
    ///
    /// It churns the worker's monitor set through `MANY` `SetServers` rounds,
    /// each a fresh process identity (i.e. a fresh `LlamaMonitor`/HTTP client)
    /// plus an add/remove of a second server, then asserts the footprint does not
    /// grow with the round count. A per-round leak of a client's threads or
    /// buffers blows the bound, so the probe is red if one is introduced.
    #[test]
    #[ignore = "heavy/leak probe: run single-threaded via an approved runner (see doc comment); process-global /proc is meaningless in the parallel gate"]
    fn soak_many_set_servers_rounds_bound_resources_single_threaded() {
        const MANY: u64 = 60;
        let a = programmable_mock("model-A".to_string(), 4096, 1_000);
        let b = programmable_mock("model-B".to_string(), 8192, 2_000);
        let key_a = crate::domain::server_key(&a.base());
        let key_b = crate::domain::server_key(&b.base());

        let (stop, samples, cmds, worker) =
            start_soak_worker(vec![test_spec(&key_a, &a.base())], 200);

        // Settle with one monitor so the baseline includes the worker's steady
        // thread set; every mock thread is already running.
        wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));
        let threads_baseline = self_status_field("Threads:").unwrap_or(0);
        let rss_baseline = self_status_field("VmRSS:").unwrap_or(0);

        // High churn: a new identity every round forces a fresh monitor (HTTP
        // client) each round; B is added/removed, so monitors are also dropped.
        for round in 1..=MANY {
            let mut servers = vec![ServerSpec {
                identity: Some(LocalServerIdentity {
                    pid: 2_000 + round as u32,
                    start_time: round,
                }),
                ..test_spec(&key_a, &a.base())
            }];
            if round % 2 == 0 {
                servers.push(test_spec(&key_b, &b.base()));
            }
            cmds.send(LlmCommand::SetServers {
                generation: round,
                servers,
            })
            .unwrap();
            // Pace one poll cycle per round so the command is actually applied
            // and a monitor rebuilds; drain the bounded channel meanwhile.
            let cycle_end = Instant::now() + Duration::from_millis(MIN_LLM_POLL_MS + 20);
            while Instant::now() < cycle_end {
                let _ = samples.recv_timeout(Duration::from_millis(20));
            }
        }

        // The worker must still be polling after the churn (no wedge).
        wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));

        stop.store(true, Ordering::SeqCst);
        assert!(join_within(worker, Duration::from_secs(5)));
        // The worker (and its monitors/clients) is joined, but a just-finished
        // reqwest blocking runtime can take a moment to tear down its worker
        // threads; this sleep lets that settle before counting. The two mock
        // threads are still running — they are joined only when `a`/`b` drop at
        // the end of this function — and were already running when
        // `threads_baseline` was taken, so they cancel out of the comparison.
        thread::sleep(Duration::from_millis(200));

        let threads_after = self_status_field("Threads:").unwrap_or(0);
        let rss_after = self_status_field("VmRSS:").unwrap_or(0);
        bench_emit("soak_churn_rounds", MANY);
        bench_emit("soak_churn_threads_baseline", threads_baseline);
        bench_emit("soak_churn_threads_after", threads_after);
        bench_emit(
            "soak_churn_rss_growth_kb",
            rss_after.saturating_sub(rss_baseline),
        );

        // Single-threaded: after the worker is joined the footprint must return
        // to (roughly) the baseline taken with the worker running; a per-round
        // leak over 60 rounds would be far above this slack.
        assert!(
            threads_after <= threads_baseline + 8,
            "threads grew over {MANY} SetServers rounds ({threads_baseline} -> {threads_after}); a monitor/client leak?"
        );
        let rss_growth = rss_after.saturating_sub(rss_baseline);
        assert!(
            rss_growth < 64 * 1024,
            "RSS grew by {rss_growth} KiB over {MANY} SetServers rounds (uncontrolled growth?)"
        );
    }

    #[test]
    fn drain_try_recv_distinguishes_empty_from_disconnected() {
        let (tx, rx) = mpsc::channel::<u8>();
        let mut seen = Vec::new();

        assert!(
            drain_try_recv(&rx, |value| seen.push(value)).is_ok(),
            "an empty but connected queue is healthy"
        );

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        drop(tx);

        assert!(
            drain_try_recv(&rx, |value| seen.push(value)).is_err(),
            "a disconnected worker must be surfaced"
        );
        assert_eq!(seen, [1, 2], "queued values are applied before the error");
    }

    #[test]
    fn try_send_snapshot_counts_full_channel_and_stops_on_disconnect() {
        let (tx, rx) = mpsc::sync_channel::<u8>(1);
        let drops = AtomicU64::new(0);

        assert!(try_send_snapshot(&tx, 1, &drops));
        assert!(try_send_snapshot(&tx, 2, &drops));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_recv().unwrap(), 1, "the pending snapshot is kept");

        drop(rx);
        assert!(!try_send_snapshot(&tx, 3, &drops));
        assert_eq!(
            drops.load(Ordering::Relaxed),
            1,
            "a disconnect must not be counted as a dropped snapshot"
        );
    }

    #[test]
    fn join_bounded_reports_clean_exit() {
        let worker = thread::spawn(|| {});
        assert_eq!(join_bounded(worker, "test", Duration::from_secs(2)), Ok(()));
    }

    #[test]
    fn join_bounded_reports_panicked_worker() {
        let worker = thread::spawn(|| panic!("simulated worker panic"));
        let error = join_bounded(worker, "test", Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("panicked"), "{error}");
    }

    #[test]
    fn join_bounded_reports_timeout() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker = thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let error = join_bounded(worker, "test", Duration::from_millis(50)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        release_tx.send(()).unwrap();
    }
}
