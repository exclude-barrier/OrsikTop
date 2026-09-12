use std::{
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
use sysinfo::System;

use crate::{
    gpu::{GpuMonitor, GpuStats},
    llama::{LlamaMonitor, LlmStats},
    ui::{self, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS},
};

const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

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

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();

            if last_system_refresh
                .map(|at| at.elapsed() >= SYSTEM_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                system.refresh_cpu_usage();
                system.refresh_memory();
                system_stats = SystemStats {
                    cpu_usage: system.global_cpu_usage() as f64,
                    memory_used_bytes: system.used_memory(),
                    memory_total_bytes: system.total_memory(),
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
}
