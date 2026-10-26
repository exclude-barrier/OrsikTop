//! `UiState` LLM history, sample bookkeeping and server-selector state.
use super::*;

#[derive(Copy, Clone, Debug)]
pub(super) struct TimedSample {
    pub(super) at: Instant,
    pub(super) value: f64,
}

/// Bounded in-memory LLM telemetry history and connection state for one
/// server. Kept per server (keyed by stable identity) so every monitored
/// server accumulates independently and the history survives switching the
/// selected server. Host-level CPU/GPU histories are separate and unaffected.
#[derive(Clone, Default)]
pub(super) struct LlmHistory {
    pub(super) prefill: VecDeque<TimedSample>,
    pub(super) decode: VecDeque<TimedSample>,
    pub(super) last_fresh_at: Option<Instant>,
    pub(super) sample_interval_ema_ms: Option<f64>,
    pub(super) connected_since: Option<Instant>,
    pub(super) connected_flash_until: Option<Instant>,
    pub(super) was_connected: bool,
}

impl UiState {
    pub fn push_sample(&mut self, gpu: &GpuStats, system: &SystemStats) {
        let now = Instant::now();
        let vram = match (gpu.memory_used_mib, gpu.memory_total_mib) {
            (Some(used), Some(total)) if total > 0.0 => Some(percent(used, total)),
            _ => None,
        };
        let ram = if system.memory_total_bytes > 0 {
            Some(percent(
                system.memory_used_bytes as f64,
                system.memory_total_bytes as f64,
            ))
        } else {
            None
        };

        if let Some(utilization) = gpu.utilization {
            push_history_at(&mut self.gpu_history, utilization, now);
        }
        if let Some(vram) = vram {
            push_history_at(&mut self.vram_history, vram, now);
        }
        push_history_at(&mut self.cpu_history, system.cpu_usage, now);
        if let Some(ram) = ram {
            push_history_at(&mut self.ram_history, ram, now);
        }
    }

    /// Record one LLM sample for `key`, mirroring it into the active history
    /// when `key` is the selected server. Called exactly once per received
    /// sample, so a UI redraw never creates a duplicate history point.
    pub fn record_llm_sample(&mut self, key: &str, selected: &str, llm: &LlmStats) {
        let now = Instant::now();
        if key == selected {
            if self.llm_history_key != key {
                self.llm_history = LlmHistory::default();
                self.llm_history_key = key.to_string();
            }
            observe_llm_into(&mut self.llm_history, llm, now);
            self.llm_histories
                .insert(key.to_string(), self.llm_history.clone());
        } else {
            observe_llm_into(
                self.llm_histories.entry(key.to_string()).or_default(),
                llm,
                now,
            );
        }
    }

    /// Milliseconds since this server's last successful measurement, from the
    /// same per-server history the selected-server panel uses. `None` when the
    /// server has never answered, so the overview shows `—` rather than `0` or
    /// a generic connection age.
    pub fn server_sample_age_ms(&self, key: &str, now: Instant) -> Option<u64> {
        self.llm_histories
            .get(key)
            .and_then(|history| history.last_fresh_at)
            .map(|at| now.saturating_duration_since(at).as_millis() as u64)
    }

    /// Smoothed sample interval (ms) for a server while it is connected, from
    /// its own per-server history (the same value the selected-server panel
    /// shows). `None` until a second sample has been seen or while offline, so
    /// the overview falls back to the measurement age instead of a fake value.
    pub fn server_sample_interval_ms(&self, key: &str) -> Option<f64> {
        self.llm_histories
            .get(key)
            .and_then(|history| history.sample_interval_ema_ms)
    }

    /// Make `key`'s own history active (the selected server changed). Other
    /// servers' histories are left untouched.
    pub fn select_llm_history(&mut self, key: &str) {
        if self.llm_history_key == key {
            return;
        }
        self.llm_history = self.llm_histories.get(key).cloned().unwrap_or_default();
        self.llm_history_key = key.to_string();
    }

    /// Drop one server's history (e.g. a proven process restart at the same
    /// endpoint), so an incompatible session is not drawn as continuous.
    pub fn reset_llm_history(&mut self, key: &str) {
        self.llm_histories.remove(key);
        if self.llm_history_key == key {
            self.llm_history = LlmHistory::default();
        }
    }

    /// Keep histories only for servers that still exist, bounding memory and
    /// avoiding tombstone growth when servers disappear.
    pub fn retain_llm_histories(&mut self, keys: &HashSet<String>) {
        self.llm_histories.retain(|key, _| keys.contains(key));
        if !self.llm_history_key.is_empty() && !keys.contains(&self.llm_history_key) {
            self.llm_history = LlmHistory::default();
            self.llm_history_key.clear();
        }
    }

    pub fn is_server_selector_open(&self) -> bool {
        self.server_selector_open
    }

    /// Open the selector with the highlight on `selected_index` (the currently
    /// selected server), so opening and confirming does not switch away.
    pub fn open_server_selector(&mut self, selected_index: usize) {
        self.server_selector_open = true;
        self.server_selector_index = selected_index;
        self.help_open = false;
        self.process_search_open = false;
        self.settings_open = false;
    }

    pub fn close_server_selector(&mut self) {
        self.server_selector_open = false;
    }

    /// Move the highlight within `len` servers, wrapping around. No server is
    /// selected by moving; `Enter` commits the highlight.
    pub fn move_server_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.server_selector_index = 0;
            return;
        }
        let current = self.server_selector_index.min(len - 1) as isize;
        self.server_selector_index = (current + delta).rem_euclid(len as isize) as usize;
    }

    /// The highlighted index, clamped to the current list length.
    pub fn server_selector_index(&self, len: usize) -> Option<usize> {
        (len > 0).then(|| self.server_selector_index.min(len - 1))
    }
}
