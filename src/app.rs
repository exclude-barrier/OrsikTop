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
    domain::{
        DashboardSnapshot, FastSnapshot, GpuMapping, GpuSelector, ProcessIdentity, ProcessStats,
        SystemStats, MAX_REFRESH_MS, MIN_LLM_POLL_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
    drm::{sample_process_gpus, DrmSamplerState},
    gpu::new_gpu_provider,
    gpu_map::{map_server_gpus, nvml_compute_gpus, process_render_gpus},
    llama::{LlamaMonitor, LlmStats},
    ui::{self, UiState},
};
use nvml_wrapper::Nvml;
const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
/// S12: CPU temperature and power are slow-changing; they are sampled on this
/// slower cadence (not the 250 ms system cadence) and reused in between.
/// Power additionally blocks ~50 ms for the RAPL two-read window. Frequency
/// stays on the fast cadence — it is cheap and changes quickly.
const SLOW_SENSOR_REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    // Local PID of the discovered server process (S16), when present.
    server_pid: Option<u32>,
    initial_settings: config::AppConfig,
    // True when `server` was resolved via local auto-discovery.
    server_auto: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = server.to_string();
    let mut server_auto = server_auto;
    let mut settings = initial_settings.sanitized();
    let mut refresh_ms = settings.refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);
    settings.refresh_ms = refresh_ms;
    let refresh_shared = Arc::new(AtomicU64::new(refresh_ms));
    let process_refresh_shared = Arc::new(AtomicU64::new(settings.process_refresh_ms));
    let offline_grace_shared = Arc::new(AtomicU64::new(settings.offline_grace_ms));
    let gpu_selector_shared = Arc::new(RwLock::new(settings.gpu_selector.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let (fast_tx, fast_rx) = mpsc::sync_channel::<FastSnapshot>(2);
    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmStats>(2);
    let (server_tx, server_rx) = mpsc::channel::<String>();
    let (server_pid_tx, server_pid_rx) = mpsc::channel::<Option<u32>>();
    if let Some(pid) = server_pid {
        let _ = server_pid_tx.send(Some(pid));
    }

    let fast_worker = spawn_fast_worker(
        Arc::clone(&gpu_selector_shared),
        Arc::clone(&process_refresh_shared),
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        fast_tx,
        server_pid_rx,
    );
    let llm_worker = spawn_llm_worker(
        server.clone(),
        Arc::clone(&refresh_shared),
        Arc::clone(&offline_grace_shared),
        Arc::clone(&stop),
        llm_tx,
        server_rx,
    );

    let mut snapshot = DashboardSnapshot::default();
    let mut ui_state = UiState::default();

    loop {
        while let Ok(next) = fast_rx.try_recv() {
            ui_state.clamp_process_selection(&next.system.processes);
            ui_state.push_sample(&next.gpu, &next.system);
            snapshot.gpu = next.gpu;
            snapshot.system = next.system;
            snapshot.gpu_map = next.gpu_map;
        }
        while let Ok(next) = llm_rx.try_recv() {
            ui_state.observe_llm_sample(&next);
            snapshot.llm = next;
        }

        terminal.draw(|frame| {
            ui::draw(
                frame,
                &snapshot.system,
                &snapshot.llm,
                &snapshot.gpu,
                &snapshot.gpu_map,
                &mut ui_state,
                &server,
                refresh_ms,
                server_auto,
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
                                    let (next_server, next_pid) =
                                        crate::resolve_server_full(&next_settings);
                                    let server_changed = next_server != server;
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
                                    if server_changed {
                                        server = next_server.clone();
                                        server_auto = crate::server_is_auto_discovered(&settings);
                                        snapshot.llm = LlmStats::default();
                                        ui_state.reset_llm_connection_state();
                                        let _ = server_tx.send(next_server);
                                        let _ = server_pid_tx.send(next_pid);
                                    }
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
                    KeyCode::Char('q') => ui_state.open_settings(&server, &settings),
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

fn spawn_fast_worker(
    gpu_selector: Arc<RwLock<GpuSelector>>,
    process_refresh_ms: Arc<AtomicU64>,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<FastSnapshot>,
    server_pid_rx: Receiver<Option<u32>>,
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
        let mut server_pid: Option<u32> = None;
        let nvml_handle = Nvml::init().ok();
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
            // Drain any server→PID updates (initial + settings edits).
            while let Ok(pid) = server_pid_rx.try_recv() {
                server_pid = pid;
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
                // A live local process appears in the table with a start_time;
                // that is what makes the fd/NVML evidence trustworthy.
                if let Some(pid) = server_pid {
                    let server_running = process_stats
                        .iter()
                        .any(|p| p.pid == pid && p.start_time > 0);
                    let render = process_render_gpus(&RealSys, pid, &static_gpus);
                    let nvml = nvml_compute_gpus(nvml_handle.as_ref(), pid);
                    gpu_map = map_server_gpus(server_running, render, nvml, &static_gpus);
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
            };

            match tx.try_send(snapshot) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_REFRESH_MS, &stop);
        }
    })
}

/// Cached stable process metadata, keyed by [`ProcessIdentity`].
///
/// The kernel recycles PIDs, so identity is `pid + start_time`. Once a
/// process has been seen, its stable metadata (program name, executable
/// path, thread count) is cached and reused; only the dynamic counters
/// (CPU, memory) are resampled on each process refresh. The thread count
/// in particular is read from `/proc/<pid>/status`, which is the expensive
/// per-PID filesystem access — caching it means it is read once per process
/// instance instead of once per process refresh.
#[derive(Default)]
struct ProcessCache {
    entries: std::collections::HashMap<ProcessIdentity, CachedProcess>,
}

struct CachedProcess {
    program: String,
    command: String,
    threads: usize,
}

impl ProcessCache {
    fn stable(
        &mut self,
        identity: ProcessIdentity,
        program: &str,
        command: &str,
    ) -> (String, String, usize) {
        if let Some(cached) = self.entries.get(&identity) {
            return (
                cached.program.clone(),
                cached.command.clone(),
                cached.threads,
            );
        }
        let threads = read_process_thread_count(identity.pid).unwrap_or(1);
        let program = program.to_string();
        let command = command.to_string();
        self.entries.insert(
            identity,
            CachedProcess {
                program: program.clone(),
                command: command.clone(),
                threads,
            },
        );
        (program, command, threads)
    }

    /// Drop entries for identities that no longer exist (exited processes).
    fn retain(&mut self, live: impl Iterator<Item = ProcessIdentity>) {
        let live: std::collections::HashSet<_> = live.collect();
        self.entries.retain(|identity, _| live.contains(identity));
    }
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

        let (program, command, threads) = cache.stable(identity, &program, &command);

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
    tx: SyncSender<LlmStats>,
    server_rx: Receiver<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut current_server = server;
        let mut llama = LlamaMonitor::new(&current_server).ok();
        let mut llama_init_error = if llama.is_none() {
            "failed to initialize HTTP client".to_string()
        } else {
            String::new()
        };
        let mut last_good_llm: Option<(LlmStats, Instant)> = None;

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            while let Ok(next_server) = server_rx.try_recv() {
                current_server = next_server;
                llama = LlamaMonitor::new(&current_server).ok();
                llama_init_error = if llama.is_none() {
                    "failed to initialize HTTP client".to_string()
                } else {
                    String::new()
                };
                last_good_llm = None;
            }
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

            match tx.try_send(stats) {
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
        let (_, command, _) = cache.stable(new, "new-prog", "/bin/new");
        assert_eq!(command, "/bin/new");
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
}
