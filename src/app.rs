use std::{
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
        SystemStats, MAX_REFRESH_MS, MIN_LLM_POLL_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
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

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    // Stable identity (PID + start time) of the discovered server process
    // (S16), when present.
    server_identity: Option<LocalServerIdentity>,
    initial_settings: config::AppConfig,
    // True when `server` was resolved via local auto-discovery.
    server_auto: bool,
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
    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmSample>(2);
    // Endpoint/PID/origin changes are carried with a monotonically increasing
    // generation so a result produced for a previous target can be rejected
    // instead of being shown under the new one (see `apply_server_resolution`).
    let (server_tx, server_rx) = mpsc::channel::<LlmCommand>();
    let (server_identity_tx, server_identity_rx) =
        mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
    let mut llm_generation: u64 = 0;
    let mut mapping_generation: u64 = 0;
    if let Some(identity) = server_identity {
        let _ = server_identity_tx.send((mapping_generation, Some(identity)));
    }

    let fast_worker = spawn_fast_worker(
        Arc::clone(&gpu_selector_shared),
        Arc::clone(&process_refresh_shared),
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        fast_tx,
        server_identity_rx,
    );
    let llm_worker = spawn_llm_worker(
        resolved.endpoint.clone(),
        Arc::clone(&refresh_shared),
        Arc::clone(&offline_grace_shared),
        Arc::clone(&stop),
        llm_tx,
        server_rx,
    );

    let mut snapshot = DashboardSnapshot::default();
    let mut ui_state = UiState::default();
    let mut last_server_resync = Instant::now();

    loop {
        while let Ok(next) = fast_rx.try_recv() {
            apply_fast_snapshot(&mut snapshot, &mut ui_state, mapping_generation, next);
        }
        while let Ok(next) = llm_rx.try_recv() {
            // Drop results produced for a server we already switched away from:
            // they can still arrive after the channel drained (a sample was
            // already in flight) and must not appear under the new endpoint.
            if !accepts_llm_sample(llm_generation, &next) {
                continue;
            }
            ui_state.observe_llm_sample(&next.stats);
            snapshot.llm = next.stats;
        }

        // S16: an auto-discovered server is dynamic — it can restart on the
        // same port (new PID) or exit entirely during a run. Re-resolve on a
        // bounded cadence so the local GPU mapping and the auto label cannot
        // stay pinned to a stale PID. Manual/remote endpoints are static, so
        // this is skipped for them. The UI thread never scans `/proc` per
        // frame; the worst-case staleness is `SERVER_RESYNC_INTERVAL`.
        if settings.auto_discovery && last_server_resync.elapsed() >= SERVER_RESYNC_INTERVAL {
            let next = crate::resolve_monitor_target(&settings);
            apply_server_resolution(
                &mut resolved,
                next,
                &mut llm_generation,
                &mut mapping_generation,
                &server_tx,
                &server_identity_tx,
                &mut snapshot,
                &mut ui_state,
            );
            last_server_resync = Instant::now();
        }

        terminal.draw(|frame| {
            ui::draw(
                frame,
                &snapshot.system,
                &snapshot.llm,
                &snapshot.gpu,
                &snapshot.gpu_map,
                &mut ui_state,
                &resolved.endpoint,
                refresh_ms,
                resolved.auto,
            )
        })?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Repeat
                        && !ui_state.is_help_open()
                        && !ui_state.is_settings_open()
                        && !ui_state.is_process_search_open() =>
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
                                    apply_server_resolution(
                                        &mut resolved,
                                        next,
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
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
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
                        && !ui_state.is_process_search_open() =>
                {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if ui_state.click_process_row(mouse.column, mouse.row) {
                                continue;
                            }

                            // Any left-click away from a process row releases the pinned process.
                            ui_state.clear_process_selection();

                            let (width, _) = crossterm::terminal::size()?;
                            let header = Rect::new(0, 0, width, 3);
                            let mut handled = false;
                            if let Some(controls) = ui::refresh_controls(header) {
                                if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                                    change_refresh(&mut refresh_ms, false, &refresh_shared);
                                    settings.refresh_ms = refresh_ms;
                                    handled = true;
                                } else if ui::rect_contains(controls.plus, mouse.column, mouse.row)
                                {
                                    change_refresh(&mut refresh_ms, true, &refresh_shared);
                                    settings.refresh_ms = refresh_ms;
                                    handled = true;
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
    generation: u64,
    stats: LlmStats,
}

/// True when an LLM sample belongs to the currently selected LLM generation.
fn accepts_llm_sample(current_generation: u64, sample: &LlmSample) -> bool {
    sample.generation == current_generation
}

/// Command to the LLM worker.
enum LlmCommand {
    /// Poll a new endpoint: start a new LLM session (new generation).
    Switch { generation: u64, endpoint: String },
    /// The process behind the current endpoint was replaced (e.g. a llama.cpp
    /// restart on the same port). A new process is not a cosmetic label
    /// change: its /metrics counters and /props start over, so the
    /// server-dependent caches (counter baselines, cached props, slot
    /// baselines) are dropped without starting a new LLM session.
    ResetServer,
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

/// Apply an independently-detected endpoint/PID/origin change to the running
/// app:
/// * an endpoint change resets the LLM HTTP session (a new server to poll) and
///   advances the LLM generation, so late results of the previous session are
///   rejected;
/// * a PID-only change updates the local GPU mapping without touching the
///   monitor or its history (a pure local re-attribution), advancing the
///   mapping generation so a stale mapping cannot overwrite the new one;
/// * an origin-only change just relabels the display.
#[allow(clippy::too_many_arguments)]
fn apply_server_resolution(
    current: &mut crate::ResolvedServer,
    next: crate::ResolvedServer,
    llm_generation: &mut u64,
    mapping_generation: &mut u64,
    server_tx: &mpsc::Sender<LlmCommand>,
    server_identity_tx: &mpsc::Sender<(u64, Option<LocalServerIdentity>)>,
    snapshot: &mut DashboardSnapshot,
    ui_state: &mut UiState,
) {
    let change = reconcile_server(current, &next);
    if change.endpoint {
        *llm_generation = llm_generation.wrapping_add(1);
        snapshot.llm = LlmStats::default();
        ui_state.reset_llm_connection_state();
        let _ = server_tx.send(LlmCommand::Switch {
            generation: *llm_generation,
            endpoint: next.endpoint.clone(),
        });
    } else if change.pid {
        // Same endpoint, new process: reset the monitor's server-dependent
        // caches (counter baselines, /props) so a restart cannot produce
        // spurious deltas or carry stale speculative parameters.
        let _ = server_tx.send(LlmCommand::ResetServer);
    }
    if change.pid {
        // The fast worker recomputes the server→GPU mapping from this identity;
        // a `None` clears it so a vanished server leaves no stale attribution.
        *mapping_generation = mapping_generation.wrapping_add(1);
        let _ = server_identity_tx.send((*mapping_generation, next.identity));
    }
    if change.endpoint || change.pid || change.auto {
        *current = next;
    }
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
                        .with_exe(UpdateKind::OnlyIfNotSet),
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

                let (load_one, load_five, load_fifteen) =
                    read_load_average().unwrap_or((0.0, 0.0, 0.0));
                let per_cpu_usage = system
                    .cpus()
                    .iter()
                    .map(|cpu| cpu.cpu_usage() as f64)
                    .collect::<Vec<_>>();
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
                    load_one,
                    load_five,
                    load_fifteen,
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
        if let Some(cached) = self.entries.get(&identity) {
            return (cached.program.clone(), cached.command.clone());
        }
        let program = program.to_string();
        let command = command.to_string();
        self.entries.insert(
            identity,
            CachedProcess {
                program: program.clone(),
                command: command.clone(),
            },
        );
        (program, command)
    }

    /// Drop entries for identities that no longer exist (exited processes).
    fn retain(&mut self, live: impl Iterator<Item = ProcessIdentity>) {
        let live: std::collections::HashSet<_> = live.collect();
        self.entries.retain(|identity, _| live.contains(identity));
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

fn spawn_llm_worker(
    server: String,
    refresh_ms: Arc<AtomicU64>,
    offline_grace_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<LlmSample>,
    server_rx: Receiver<LlmCommand>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut current_server = server;
        // Generation of the LLM session this worker is currently configured
        // for; every published sample is tagged with it so the app can reject
        // results of a session it already switched away from.
        let mut current_generation: u64 = 0;
        let mut llama = LlamaMonitor::new(&current_server).ok();
        let mut llama_init_error = if llama.is_none() {
            "failed to initialize HTTP client".to_string()
        } else {
            String::new()
        };
        let mut last_good_llm: Option<(LlmStats, Instant)> = None;

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            while let Ok(command) = server_rx.try_recv() {
                match command {
                    LlmCommand::Switch {
                        generation,
                        endpoint,
                    } => {
                        current_generation = generation;
                        current_server = endpoint;
                    }
                    LlmCommand::ResetServer => {
                        // Same endpoint, new process: keep the generation (and
                        // thus the UI history) but drop every server-dependent
                        // cache by rebuilding the monitor.
                    }
                }
                llama = LlamaMonitor::new(&current_server).ok();
                llama_init_error = if llama.is_none() {
                    "failed to initialize HTTP client".to_string()
                } else {
                    String::new()
                };
                last_good_llm = None;
            }
            // `sample` is synchronous; `current_generation` cannot change
            // while it runs, so the tag is the session this sample belongs to.
            let raw_stats = match llama.as_mut() {
                Some(monitor) => monitor.sample(),
                None => LlmStats {
                    error: llama_init_error.clone(),
                    ..Default::default()
                },
            };
            let offline_grace = Duration::from_millis(
                offline_grace_ms
                    .load(Ordering::Relaxed)
                    .min(config::MAX_OFFLINE_GRACE_MS),
            );
            let stats =
                stabilize_llm_sample(raw_stats, &mut last_good_llm, Instant::now(), offline_grace);

            let sample = LlmSample {
                generation: current_generation,
                stats,
            };
            match tx.try_send(sample) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
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
            prompt_tps: 123.0,
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
        assert_eq!(held.prompt_tps, 123.0);
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
    fn apply_resets_llm_only_on_endpoint_change_and_forwards_identity() {
        let (server_tx, server_rx) = mpsc::channel::<LlmCommand>();
        let (identity_tx, identity_rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
        let mut snapshot = DashboardSnapshot::default();
        snapshot.llm.model = "kept".to_string();
        let mut ui_state = UiState::default();
        let mut llm_generation = 0u64;
        let mut mapping_generation = 0u64;
        let mut current = target("http://127.0.0.1:8081", Some(ident(5000, 1)), true);

        // An identity-only change (server restart, same URL): the GPU mapping
        // is refreshed, but the LLM session and its history are preserved.
        apply_server_resolution(
            &mut current,
            target("http://127.0.0.1:8081", Some(ident(6000, 1)), true),
            &mut llm_generation,
            &mut mapping_generation,
            &server_tx,
            &identity_tx,
            &mut snapshot,
            &mut ui_state,
        );
        assert_eq!(snapshot.llm.model, "kept");
        assert_eq!(
            llm_generation, 0,
            "identity-only change keeps the LLM session"
        );
        assert_eq!(mapping_generation, 1);
        assert_eq!(identity_rx.try_recv().unwrap(), (1, Some(ident(6000, 1))));
        // A same-endpoint process change resets the server-dependent caches
        // but keeps the LLM session (generation unchanged).
        assert!(matches!(
            server_rx.try_recv().unwrap(),
            LlmCommand::ResetServer
        ));
        assert_eq!(current.identity, Some(ident(6000, 1)));

        // An endpoint change resets the LLM session and re-targets the worker;
        // the identity is cleared along with the local attribution.
        apply_server_resolution(
            &mut current,
            target("http://127.0.0.1:9090", None, false),
            &mut llm_generation,
            &mut mapping_generation,
            &server_tx,
            &identity_tx,
            &mut snapshot,
            &mut ui_state,
        );
        assert_eq!(
            snapshot.llm.model, "",
            "endpoint change resets the LLM session"
        );
        assert_eq!(llm_generation, 1);
        assert_eq!(mapping_generation, 2);
        assert!(matches!(
            server_rx.try_recv().unwrap(),
            LlmCommand::Switch { generation: 1, endpoint } if endpoint == "http://127.0.0.1:9090"
        ));
        assert_eq!(identity_rx.try_recv().unwrap(), (2, None));
        assert!(!current.auto);
    }

    #[test]
    fn stale_llm_sample_is_rejected_across_a_b_a() {
        let (server_tx, _server_rx) = mpsc::channel::<LlmCommand>();
        let (identity_tx, _identity_rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
        let mut snapshot = DashboardSnapshot::default();
        let mut ui_state = UiState::default();
        let mut llm_generation = 0u64;
        let mut mapping_generation = 0u64;
        let mut current = target("http://127.0.0.1:8081", Some(ident(1, 1)), true);

        let a_sample = LlmSample {
            generation: 0,
            stats: LlmStats {
                model: "A".to_string(),
                ..Default::default()
            },
        };
        assert!(accepts_llm_sample(llm_generation, &a_sample));

        let switch = |current: &mut crate::ResolvedServer,
                      next,
                      llm_generation: &mut u64,
                      mapping_generation: &mut u64,
                      snapshot: &mut DashboardSnapshot,
                      ui_state: &mut UiState| {
            apply_server_resolution(
                current,
                next,
                llm_generation,
                mapping_generation,
                &server_tx,
                &identity_tx,
                snapshot,
                ui_state,
            );
        };

        // Switch A → B: the endpoint change advances the LLM generation, so
        // A's in-flight result must be rejected under B.
        switch(
            &mut current,
            target("http://127.0.0.1:9090", Some(ident(2, 1)), true),
            &mut llm_generation,
            &mut mapping_generation,
            &mut snapshot,
            &mut ui_state,
        );
        assert_eq!(llm_generation, 1);
        assert!(!accepts_llm_sample(llm_generation, &a_sample));
        let b_sample = LlmSample {
            generation: 1,
            stats: LlmStats {
                model: "B".to_string(),
                ..Default::default()
            },
        };
        assert!(accepts_llm_sample(llm_generation, &b_sample));

        // Back to A: a *new* generation. A late result from the first A
        // session (generation 0) is still rejected even though the URL matches
        // A again — a pure URL comparison could not distinguish them.
        switch(
            &mut current,
            target("http://127.0.0.1:8081", Some(ident(3, 1)), true),
            &mut llm_generation,
            &mut mapping_generation,
            &mut snapshot,
            &mut ui_state,
        );
        assert_eq!(llm_generation, 2);
        assert!(!accepts_llm_sample(llm_generation, &a_sample));
        let a2_sample = LlmSample {
            generation: 2,
            stats: LlmStats {
                model: "A".to_string(),
                ..Default::default()
            },
        };
        assert!(accepts_llm_sample(llm_generation, &a2_sample));
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
}
