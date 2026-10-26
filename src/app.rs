use std::{
    collections::BTreeMap,
    io::Stdout,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::system::RealSys;
use crate::{
    config,
    cpu::{detect_cpu_topology, CpuTopology},
    cpu_sensors::CpuSensors,
    discovery::discover_gpus,
    discovery_llm::LocalServerIdentity,
    domain::{
        DashboardSnapshot, FastSnapshot, GpuMapping, GpuSelector, ProcessStats, SystemStats,
        MAX_MONITORED_SERVERS, MAX_REFRESH_MS, MIN_LLM_POLL_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
    drm::DrmSamplerState,
    gpu::new_gpu_provider,
    gpu_map::{nvml_compute_gpus, process_render_gpus},
    llama::LlmStats,
    providers::nvidia::discover_mig_children,
    ui::{self, UiState},
};
use nvml_wrapper::Nvml;

mod channels;
mod process;
mod server;

use channels::{drain_try_recv, join_bounded, try_send_snapshot};

pub(crate) use server::{
    build_server_summaries, map_server_identity, publish_selected_mapping, rebuild_poll_slots,
    selected_identity_of, stabilize_llm_sample, sync_server_set, update_resolved_server,
    LlmCommand, LlmPollSlot, ServerSpec,
};
#[cfg(test)]
pub(crate) use server::{
    changed_identity_keys, new_server_monitor, rebuild_server_monitors, reconcile_server,
    ServerChange,
};

pub(crate) use process::{
    collect_process_stats, io_wait_percent, per_cpu_usage_by_id, read_cpu_frequency_mhz,
    read_cpu_times, read_load_average, ProcessCache,
};
#[cfg(test)]
pub(crate) use process::{parse_cpu_times, read_process_thread_count, refresh_process_metadata};

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

/// Clears a server's in-flight flag even if the poll thread unwinds.
struct PollFlag(Arc<AtomicBool>);

impl Drop for PollFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
        let mut servers: Vec<LlmPollSlot> =
            initial_servers.into_iter().map(LlmPollSlot::new).collect();
        let (sample_tx, sample_rx) = mpsc::channel::<LlmSample>();

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            while let Ok(command) = server_rx.try_recv() {
                match command {
                    LlmCommand::SetServers {
                        generation: next_generation,
                        servers: list,
                    } => {
                        generation = next_generation;
                        servers = rebuild_poll_slots(servers, list);
                    }
                }
            }

            let offline_grace = Duration::from_millis(
                offline_grace_ms
                    .load(Ordering::Relaxed)
                    .min(config::MAX_OFFLINE_GRACE_MS),
            );
            // One poll per server at a time. A slow or hanging server keeps
            // its own in-flight slot and is simply skipped next cycle; other
            // servers continue on their own cadence.
            for slot in &servers {
                if slot
                    .in_flight
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                let monitor = Arc::clone(&slot.monitor);
                let in_flight = Arc::clone(&slot.in_flight);
                let sample_tx = sample_tx.clone();
                let generation = generation;
                thread::spawn(move || {
                    let _flag = PollFlag(in_flight);
                    let mut server = monitor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    let _ = sample_tx.send(LlmSample {
                        server: server.key.clone(),
                        generation,
                        stats,
                    });
                });
            }

            let mut disconnected = false;
            while let Ok(sample) = sample_rx.try_recv() {
                if !try_send_snapshot(&tx, sample, &drops) {
                    disconnected = true;
                    break;
                }
            }
            if disconnected {
                return;
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_LLM_POLL_MS, &stop);
        }

        // Do not let detached poll threads outlive the worker: bounded by the
        // request timeout plus margin. A truly wedged request is left for
        // process exit, as before.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && servers
                .iter()
                .any(|slot| slot.in_flight.load(Ordering::Acquire))
        {
            thread::sleep(Duration::from_millis(25));
        }
    })
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
mod tests;
