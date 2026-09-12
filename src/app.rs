use std::{
    fs,
    io::Stdout,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{
    cpu::{detect_cpu_topology, CpuTopology},
    gpu::{GpuMonitor, GpuStats},
    llama::{LlamaMonitor, LlmStats},
    ui::{
        self, ProcessStats, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
};

const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const PROCESS_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Clone, Debug, Default)]
struct DashboardSnapshot {
    llm: LlmStats,
    gpu: GpuStats,
    system: SystemStats,
}

#[derive(Clone, Debug, Default)]
struct FastSnapshot {
    gpu: GpuStats,
    system: SystemStats,
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    initial_refresh_ms: u64,
    gpu_index: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut refresh_ms = initial_refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);
    let refresh_shared = Arc::new(AtomicU64::new(refresh_ms));
    let stop = Arc::new(AtomicBool::new(false));
    let (fast_tx, fast_rx) = mpsc::sync_channel::<FastSnapshot>(2);
    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmStats>(2);

    spawn_fast_worker(
        gpu_index,
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        fast_tx,
    );
    spawn_llm_worker(
        server.to_string(),
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        llm_tx,
    );

    let mut snapshot = DashboardSnapshot::default();
    let mut ui_state = UiState::default();

    loop {
        while let Ok(next) = fast_rx.try_recv() {
            ui_state.push_sample(&next.gpu, &next.system);
            snapshot.gpu = next.gpu;
            snapshot.system = next.system;
        }
        while let Ok(next) = llm_rx.try_recv() {
            snapshot.llm = next;
        }

        terminal.draw(|frame| {
            ui::draw(
                frame,
                &snapshot.system,
                &snapshot.llm,
                &snapshot.gpu,
                &ui_state,
                server,
                refresh_ms,
            )
        })?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('-') | KeyCode::Char('[') => {
                        change_refresh(&mut refresh_ms, false, &refresh_shared);
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => {
                        change_refresh(&mut refresh_ms, true, &refresh_shared);
                    }
                    _ => {}
                },
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    let (width, _) = crossterm::terminal::size()?;
                    let header = Rect::new(0, 0, width, 3);
                    if let Some(controls) = ui::refresh_controls(header) {
                        if ui::rect_contains(controls.minus, mouse.column, mouse.row) {
                            change_refresh(&mut refresh_ms, false, &refresh_shared);
                        } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                            change_refresh(&mut refresh_ms, true, &refresh_shared);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
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
    gpu_index: u32,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<FastSnapshot>,
) {
    thread::spawn(move || {
        let gpu = GpuMonitor::new(gpu_index);
        let mut system = System::new();
        let mut system_stats = SystemStats::default();
        let mut last_system_refresh: Option<Instant> = None;
        let mut last_process_refresh: Option<Instant> = None;
        let mut process_stats = Vec::<ProcessStats>::new();
        let mut previous_cpu_times = read_cpu_times();
        let mut cpu_topology: Option<CpuTopology> = None;

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();

            if last_process_refresh
                .map(|at| at.elapsed() >= PROCESS_REFRESH_INTERVAL)
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
                process_stats = collect_process_stats(&system);
                last_process_refresh = Some(Instant::now());
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
                    cpu_frequency_mhz: read_cpu_frequency_mhz(),
                    cpu_temperature_c: read_cpu_temperature_c(),
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
            };

            match tx.try_send(snapshot) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, &stop);
        }
    });
}

fn collect_process_stats(system: &System) -> Vec<ProcessStats> {
    let mut processes = system
        .processes()
        .iter()
        .map(|(pid, process)| {
            let program = process.name().to_string_lossy().into_owned();
            let command = process
                .exe()
                .map(|path| path.display().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| program.clone());
            let pid_u32 = pid.as_u32();
            ProcessStats {
                pid: pid_u32,
                program,
                command,
                cpu_pct: process.cpu_usage() as f64,
                memory_bytes: process.memory(),
                threads: read_process_thread_count(pid_u32).unwrap_or(1),
            }
        })
        .collect::<Vec<_>>();

    processes.sort_by(|a, b| {
        b.cpu_pct
            .total_cmp(&a.cpu_pct)
            .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    processes.truncate(256);
    processes
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

fn read_cpu_frequency_mhz() -> Option<f64> {
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
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

fn read_cpu_temperature_c() -> Option<f64> {
    let mut preferred = Vec::new();
    let mut fallback = Vec::new();
    let hwmons = fs::read_dir("/sys/class/hwmon").ok()?;

    for entry in hwmons.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let cpu_sensor = name.contains("coretemp")
            || name.contains("k10temp")
            || name.contains("zenpower")
            || name.contains("cpu")
            || name.contains("x86_pkg");
        if !cpu_sensor {
            continue;
        }

        let Ok(sensors) = fs::read_dir(&path) else {
            continue;
        };
        for sensor in sensors.flatten() {
            let filename = sensor.file_name();
            let filename = filename.to_string_lossy();
            if !filename.starts_with("temp") || !filename.ends_with("_input") {
                continue;
            }

            let Ok(raw) = fs::read_to_string(sensor.path()) else {
                continue;
            };
            let Ok(millidegrees) = raw.trim().parse::<f64>() else {
                continue;
            };
            let celsius = millidegrees / 1000.0;
            if !(-20.0..=150.0).contains(&celsius) {
                continue;
            }

            let stem = filename.trim_end_matches("_input");
            let label = fs::read_to_string(path.join(format!("{stem}_label")))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if label.contains("package") || label.contains("tctl") || label.contains("cpu") {
                preferred.push(celsius);
            } else {
                fallback.push(celsius);
            }
        }
    }

    preferred
        .into_iter()
        .reduce(f64::max)
        .or_else(|| fallback.into_iter().reduce(f64::max))
}

fn spawn_llm_worker(
    server: String,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<LlmStats>,
) {
    thread::spawn(move || {
        let mut llama = LlamaMonitor::new(&server).ok();
        let llama_init_error = if llama.is_none() {
            "failed to initialize HTTP client".to_string()
        } else {
            String::new()
        };

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            let stats = match llama.as_mut() {
                Some(monitor) => monitor.sample(),
                None => LlmStats {
                    error: llama_init_error.clone(),
                    ..Default::default()
                },
            };

            match tx.try_send(stats) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, &stop);
        }
    });
}

fn sleep_until_next_cycle(started: Instant, refresh_ms: &AtomicU64, stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let target = Duration::from_millis(
            refresh_ms
                .load(Ordering::Relaxed)
                .clamp(MIN_REFRESH_MS, MAX_REFRESH_MS),
        );
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
}
