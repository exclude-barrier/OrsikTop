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
    ui::{
        self, SystemStats, UiState, MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
};

#[derive(Clone, Debug, Default)]
struct TelemetrySnapshot {
    llm: LlmStats,
    gpu: GpuStats,
    system: SystemStats,
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    initial_refresh_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut refresh_ms = initial_refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);
    let refresh_shared = Arc::new(AtomicU64::new(refresh_ms));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel::<TelemetrySnapshot>(2);

    spawn_telemetry_worker(
        server.to_string(),
        Arc::clone(&refresh_shared),
        Arc::clone(&stop),
        tx,
    );

    let mut snapshot = TelemetrySnapshot::default();
    let mut ui_state = UiState::default();

    loop {
        while let Ok(next) = rx.try_recv() {
            ui_state.push_gpu_sample(&next.gpu);
            snapshot = next;
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

fn spawn_telemetry_worker(
    server: String,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<TelemetrySnapshot>,
) {
    thread::spawn(move || {
        let mut llama = LlamaMonitor::new(&server).ok();
        let llama_init_error = if llama.is_none() {
            "failed to initialize HTTP client".to_string()
        } else {
            String::new()
        };
        let gpu = GpuMonitor::new();
        let mut system = System::new_all();

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();

            system.refresh_all();
            let system_stats = SystemStats {
                cpu_usage: system.global_cpu_usage() as f64,
                memory_used_bytes: system.used_memory(),
                memory_total_bytes: system.total_memory(),
            };

            let llm = match llama.as_mut() {
                Some(monitor) => monitor.sample(),
                None => LlmStats {
                    error: llama_init_error.clone(),
                    ..Default::default()
                },
            };
            let gpu_stats = gpu.sample();

            let snapshot = TelemetrySnapshot {
                llm,
                gpu: gpu_stats,
                system: system_stats,
            };

            match tx.try_send(snapshot) {
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
