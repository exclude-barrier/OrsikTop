use std::{
    collections::BTreeMap,
    io::Stdout,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, RwLock,
    },
    time::{Duration, Instant},
};

use crossterm::event::{self, Event};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::{
    config,
    domain::{
        DashboardSnapshot, FastSnapshot, LocalServerIdentity, MAX_MONITORED_SERVERS,
        MAX_REFRESH_MS, MIN_REFRESH_MS, REFRESH_STEP_MS,
    },
    llama::LlmStats,
    ui::{self, UiState},
};

// The monitored-server spec is a domain type (the UI and diagnostics consume
// the same description); the app owns only the reconciliation of the set.
// Re-exported so the app's own code and its test modules keep one import path.
pub(crate) use crate::domain::ServerSpec;

#[cfg(test)]
use crate::domain::{GpuMapping, ProcessStats, SystemStats, MIN_LLM_POLL_MS};
#[cfg(test)]
use crate::drm::DrmSamplerState;
#[cfg(test)]
use crate::system::RealSys;

mod channels;
mod loop_events;
mod process;
mod server;
mod workers;

use channels::{drain_try_recv, join_bounded, try_send_snapshot};
use loop_events::{handle_key_event, handle_mouse_event, pump_snapshots};
#[cfg(test)]
use workers::next_cycle_target;
use workers::{spawn_fast_worker, spawn_llm_worker};

pub(crate) use server::{
    build_server_summaries, map_server_identity, publish_selected_mapping, rebuild_poll_slots,
    selected_identity_of, stabilize_llm_sample, sync_server_set, update_resolved_server,
    LlmCommand, LlmPollSlot,
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
        pump_snapshots(
            &fast_rx,
            &llm_rx,
            llm_generation,
            mapping_generation,
            &order,
            &endpoints,
            &mut registry,
            &selected,
            &fast_drops,
            &llm_drops,
            &mut snapshot,
            &mut ui_state,
            &mut scheduler,
        )?;

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
                Event::Key(key) => {
                    if handle_key_event(
                        key,
                        &mut ui_state,
                        &mut settings,
                        &mut refresh_ms,
                        &refresh_shared,
                        &process_refresh_shared,
                        &offline_grace_shared,
                        &gpu_selector_shared,
                        &mut resolved,
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
                        &mut last_server_resync,
                    ) {
                        break;
                    }
                }
                Event::Mouse(mouse)
                    if !ui_state.is_help_open()
                        && !ui_state.is_settings_open()
                        && !ui_state.is_process_search_open()
                        && !ui_state.is_server_selector_open()
                        && handle_mouse_event(
                            mouse,
                            &mut ui_state,
                            &mut refresh_ms,
                            &refresh_shared,
                            &mut settings,
                        )? =>
                {
                    continue;
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

#[cfg(test)]
mod tests;
