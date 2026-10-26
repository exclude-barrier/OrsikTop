//! `UiState` shell behavior: snapshot drop counters and the help popup.
use super::*;

impl UiState {
    /// Publish the worker drop counters for the footer. Called every loop
    /// iteration, so a busy or stalled UI becomes visible without waiting for
    /// the next successfully delivered snapshot.
    pub(crate) fn set_snapshot_drops(&mut self, fast: u64, llm: u64) {
        self.fast_snapshot_drops = fast;
        self.llm_snapshot_drops = llm;
    }

    /// Compact footer warning, absent while no snapshot has been dropped.
    pub(crate) fn snapshot_drop_text(&self) -> Option<String> {
        (self.fast_snapshot_drops > 0 || self.llm_snapshot_drops > 0).then(|| {
            format!(
                "DROPS f{} l{}",
                self.fast_snapshot_drops, self.llm_snapshot_drops
            )
        })
    }

    pub fn toggle_help(&mut self) {
        self.settings_open = false;
        self.process_search_open = false;
        self.help_open = !self.help_open;
    }

    pub fn close_help(&mut self) {
        self.help_open = false;
    }

    pub fn is_help_open(&self) -> bool {
        self.help_open
    }
}
