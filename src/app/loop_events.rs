//! Per-iteration work of the app loop: pumping the snapshot channels into the
//! dashboard and UI state, and handling key and mouse input.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::domain::{GpuMapping, GpuSelector};

use super::*;

use super::drain_try_recv;

/// Pump both snapshot channels into the dashboard and UI state.
///
/// Fast snapshots are applied unconditionally; an LLM sample is applied only
/// when it belongs to the current server set and a still-known server.
/// `Err(msg)` means a worker channel was closed: the worker stopped, which
/// must not be mistaken for a temporarily empty queue.
#[allow(clippy::too_many_arguments)]
pub(super) fn pump_snapshots(
    fast_rx: &Receiver<FastSnapshot>,
    llm_rx: &Receiver<LlmSample>,
    llm_generation: u64,
    mapping_generation: u64,
    order: &[String],
    endpoints: &BTreeMap<String, String>,
    registry: &mut BTreeMap<String, LlmStats>,
    selected: &str,
    fast_drops: &Arc<AtomicU64>,
    llm_drops: &Arc<AtomicU64>,
    snapshot: &mut DashboardSnapshot,
    ui_state: &mut UiState,
    scheduler: &mut RedrawScheduler,
) -> Result<(), String> {
    // Each applied fast snapshot can change the overview; repaint on the
    // next iteration rather than waiting for the idle tick. Fast snapshots
    // have no rejection path — only LLM samples are filtered (below).
    drain_try_recv(fast_rx, |next| {
        apply_fast_snapshot(snapshot, ui_state, mapping_generation, next);
        scheduler.mark_dirty();
    })
    .map_err(|()| "fast monitoring worker stopped unexpectedly".to_string())?;
    drain_try_recv(llm_rx, |next| {
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
        ui_state.record_llm_sample(&next.server, selected, &next.stats);
        registry.insert(next.server, next.stats.clone());
        if is_selected {
            snapshot.llm = next.stats;
        }
        // An accepted sample changes the overview and/or the active view.
        scheduler.mark_dirty();
    })
    .map_err(|()| "LLM monitoring worker stopped unexpectedly".to_string())?;
    snapshot.servers = build_server_summaries(order, endpoints, registry, ui_state, Instant::now());
    snapshot.selected_server = selected.to_string();
    ui_state.set_snapshot_drops(
        fast_drops.load(Ordering::Relaxed),
        llm_drops.load(Ordering::Relaxed),
    );
    Ok(())
}

/// Handle one key event against the current UI and settings state.
///
/// Returns `true` when the app loop must quit (`Esc` with no dialog open);
/// every other event is fully consumed here.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_key_event(
    key: KeyEvent,
    ui_state: &mut UiState,
    settings: &mut config::AppConfig,
    refresh_ms: &mut u64,
    refresh_shared: &Arc<AtomicU64>,
    process_refresh_shared: &Arc<AtomicU64>,
    offline_grace_shared: &Arc<AtomicU64>,
    gpu_selector_shared: &Arc<RwLock<GpuSelector>>,
    resolved: &mut crate::ResolvedServer,
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
    last_server_resync: &mut Instant,
) -> bool {
    match key {
        _ if key.kind == KeyEventKind::Repeat
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
        _ if key.kind == KeyEventKind::Press && ui_state.is_settings_open() => {
            // Field navigation, editing and the server-list editor
            // live in `UiState`; only an actual save request touches
            // the runtime here.
            if ui_state.settings_handle_key(key.code) {
                match ui_state.settings_config() {
                    Ok(next_settings) => match config::save(&next_settings) {
                        Ok(()) => {
                            *settings = next_settings;
                            *refresh_ms = settings.refresh_ms;
                            refresh_shared.store(*refresh_ms, Ordering::Relaxed);
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
                            let next = crate::resolve_monitor_target(settings);
                            update_resolved_server(resolved, next);
                            sync_server_set(
                                settings,
                                server_list,
                                overflow,
                                order,
                                endpoints,
                                registry,
                                selected,
                                llm_generation,
                                mapping_generation,
                                server_tx,
                                server_identity_tx,
                                snapshot,
                                ui_state,
                            );
                            *last_server_resync = Instant::now();
                            ui_state.close_settings();
                        }
                        Err(err) => {
                            ui_state.set_settings_error(format!("Could not save settings: {err}"))
                        }
                    },
                    Err(err) => ui_state.set_settings_error(err),
                }
            }
        }
        _ if key.kind == KeyEventKind::Press && ui_state.is_process_search_open() => {
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
        _ if key.kind == KeyEventKind::Press && ui_state.is_server_selector_open() => {
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
                            *selected = key.clone();
                            snapshot.llm = registry.get(key).cloned().unwrap_or_default();
                            // Show this server's own accumulated history;
                            // every other server's history is preserved.
                            ui_state.select_llm_history(key);
                            // Re-attribute the GPU association to this
                            // server's verified process and drop the
                            // previous server's mapping so it is never
                            // shown for the newly selected one.
                            publish_selected_mapping(
                                selected_identity_of(server_list, selected),
                                mapping_generation,
                                server_identity_tx,
                            );
                            snapshot.gpu_map = GpuMapping::None;
                        }
                    }
                    ui_state.close_server_selector();
                }
                _ => {}
            }
        }
        _ if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('s') => ui_state
                .open_server_selector(order.iter().position(|key| key == &*selected).unwrap_or(0)),
            KeyCode::Char('/') => ui_state.open_process_search(),
            KeyCode::Char('h') => ui_state.toggle_help(),
            KeyCode::Esc if ui_state.is_help_open() => ui_state.close_help(),
            _ if ui_state.is_help_open() => {}
            KeyCode::Char('q') => ui_state.open_settings(&resolved.endpoint, settings),
            KeyCode::Esc => return true,
            KeyCode::Char('-') | KeyCode::Char('[') => {
                change_refresh(refresh_ms, false, refresh_shared);
                settings.refresh_ms = *refresh_ms;
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => {
                change_refresh(refresh_ms, true, refresh_shared);
                settings.refresh_ms = *refresh_ms;
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
        _ => {}
    };
    false
}

/// Handle one mouse event against the current UI and settings state.
///
/// Returns `Ok(true)` when the loop iteration must be skipped (a process row
/// was clicked); otherwise `Ok(false)`. `Err` propagates a terminal size read
/// failure to the caller.
pub(super) fn handle_mouse_event(
    mouse: MouseEvent,
    ui_state: &mut UiState,
    refresh_ms: &mut u64,
    refresh_shared: &Arc<AtomicU64>,
    settings: &mut config::AppConfig,
) -> Result<bool, std::io::Error> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if ui_state.click_process_row(mouse.column, mouse.row) {
                return Ok(true);
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
                        change_refresh(refresh_ms, false, refresh_shared);
                        settings.refresh_ms = *refresh_ms;
                        handled = true;
                    } else if ui::rect_contains(controls.plus, mouse.column, mouse.row) {
                        change_refresh(refresh_ms, true, refresh_shared);
                        settings.refresh_ms = *refresh_ms;
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
        MouseEventKind::ScrollUp if ui_state.process_pane_contains(mouse.column, mouse.row) => {
            ui_state.scroll_processes(-3);
        }
        MouseEventKind::ScrollDown if ui_state.process_pane_contains(mouse.column, mouse.row) => {
            ui_state.scroll_processes(3);
        }
        _ => {}
    };
    Ok(false)
}
