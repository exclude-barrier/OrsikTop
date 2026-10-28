//! Server registry and worker-side monitor state.
//!
//! Owns endpoint reconciliation, generation-tagged set changes, per-server
//! summaries, monitored-server identity and the offline-grace sample hold.
//! The polling loop itself stays in `app::spawn_llm_worker`.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config;
use crate::discovery::DiscoveredGpu;
use crate::domain::{
    DashboardSnapshot, GpuMapping, LocalServerIdentity, ServerBackend, ServerSpec, ServerSummary,
};
use crate::gpu_map::map_server_gpus;
use crate::llama::{LlamaMonitor, LlmStats};
use crate::ui::UiState;

/// Which monitor-target fields changed. A URL-level comparison is insufficient:
/// only an endpoint change requires resetting the LLM HTTP session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ServerChange {
    pub(crate) endpoint: bool,
    pub(crate) pid: bool,
    pub(crate) auto: bool,
}

/// Compare two resolutions of the monitor target field by field, instead of
/// keying everything off the URL.
pub(crate) fn reconcile_server(
    previous: &crate::ResolvedServer,
    next: &crate::ResolvedServer,
) -> ServerChange {
    ServerChange {
        endpoint: previous.endpoint != next.endpoint,
        pid: previous.identity != next.identity,
        auto: previous.auto != next.auto,
    }
}

/// Build the compact per-server summaries for the overview, in `order`.
///
/// All new per-server fields are pure derivations from already-collected data:
/// the activity phase and backend come from that server's `LlmStats`, and the
/// measurement age / sample interval from its own per-server UI history — no
/// extra polling and no telemetry-semantics change.
pub(crate) fn build_server_summaries(
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

/// Command to the LLM worker.
pub(crate) enum LlmCommand {
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

/// Update the tracked primary `resolved` server (used for the header endpoint
/// and the `auto` label). It no longer drives GPU attribution: the association
/// follows the *selected* server via `publish_selected_mapping`.
pub(crate) fn update_resolved_server(
    current: &mut crate::ResolvedServer,
    next: crate::ResolvedServer,
) {
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
pub(crate) fn sync_server_set(
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
pub(crate) fn selected_identity_of(
    server_list: &[ServerSpec],
    selected: &str,
) -> Option<LocalServerIdentity> {
    server_list
        .iter()
        .find(|spec| spec.key == selected)
        .and_then(|spec| spec.identity)
}

/// Publish which process's GPU association the fast worker should resolve,
/// advancing the mapping generation so a stale mapping of a previously
/// selected server cannot be applied.
pub(crate) fn publish_selected_mapping(
    identity: Option<LocalServerIdentity>,
    mapping_generation: &mut u64,
    server_identity_tx: &mpsc::Sender<(u64, Option<LocalServerIdentity>)>,
) {
    *mapping_generation = mapping_generation.wrapping_add(1);
    let _ = server_identity_tx.send((*mapping_generation, identity));
}

/// Keys present in both sets whose process identity changed (a known
/// same-endpoint restart). A new or removed key is not a restart.
pub(crate) fn changed_identity_keys(old: &[ServerSpec], new: &[ServerSpec]) -> Vec<String> {
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
pub(crate) fn map_server_identity(
    expected: LocalServerIdentity,
    current_start_time: Option<u64>,
    render: Vec<crate::domain::DeviceId>,
    nvml: Vec<crate::domain::DeviceId>,
    gpus: &[DiscoveredGpu],
) -> GpuMapping {
    if current_start_time != Some(expected.start_time) {
        return GpuMapping::Unknown;
    }
    map_server_gpus(true, render, nvml, gpus)
}

/// One monitored server's worker-side state.
pub(crate) struct LlmServerMonitor {
    pub(crate) key: String,
    pub(crate) endpoint: String,
    /// Process identity of a locally discovered server (pid + start time); a
    /// change on the same key means the process restarted, so the monitor's
    /// caches are rebuilt.
    pub(crate) identity: Option<LocalServerIdentity>,
    pub(crate) monitor: Option<LlamaMonitor>,
    pub(crate) init_error: String,
    /// Per-server last good sample and its time, for the offline-grace hold.
    pub(crate) last_good: Option<(LlmStats, Instant)>,
}

pub(crate) fn new_server_monitor(spec: ServerSpec) -> LlmServerMonitor {
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
#[cfg(test)]
pub(crate) fn rebuild_server_monitors(
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

/// Shared monitor plus an in-flight flag. One poll per server may run at a
/// time, but a slow server does not block polls of the other servers.
pub(crate) struct LlmPollSlot {
    pub(crate) monitor: Arc<Mutex<LlmServerMonitor>>,
    pub(crate) in_flight: Arc<AtomicBool>,
}

impl LlmPollSlot {
    pub(crate) fn new(spec: ServerSpec) -> Self {
        Self {
            monitor: Arc::new(Mutex::new(new_server_monitor(spec))),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Rebuild poll slots for a new server set. An unchanged server keeps its
/// monitor and in-flight flag; a restart gets a fresh monitor and its old
/// in-flight sample is rejected by generation.
pub(crate) fn rebuild_poll_slots(
    existing: Vec<LlmPollSlot>,
    list: Vec<ServerSpec>,
) -> Vec<LlmPollSlot> {
    let mut old: BTreeMap<String, LlmPollSlot> = existing
        .into_iter()
        .map(|slot| {
            let key = slot
                .monitor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .key
                .clone();
            (key, slot)
        })
        .collect();
    list.into_iter()
        .map(|spec| match old.remove(&spec.key) {
            Some(existing) => {
                let unchanged = {
                    let monitor = existing
                        .monitor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    monitor.endpoint == spec.endpoint && monitor.identity == spec.identity
                };
                if unchanged {
                    existing
                } else {
                    LlmPollSlot::new(spec)
                }
            }
            None => LlmPollSlot::new(spec),
        })
        .collect()
}

pub(crate) fn stabilize_llm_sample(
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
