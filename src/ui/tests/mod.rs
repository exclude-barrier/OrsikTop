//! Split of the former inline `mod tests` of `ui.rs`, grouped by unit under test.
//!
//! Shared test fixtures/helpers used by more than one theme file live here.
use super::*;

mod layout_shell;
mod llm_panel;
mod process_table;
mod state;

fn render_dashboard(
    width: u16,
    height: u16,
    refresh_ms: u64,
    llm: &LlmStats,
    state: &mut UiState,
) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let system = SystemStats::default();
    let gpu = GpuStats::default();
    let servers = vec![summary("only", true, true, 1, 4)];
    terminal
        .draw(|frame| {
            draw(
                frame,
                &system,
                llm,
                &gpu,
                &GpuMapping::None,
                &servers,
                "only",
                &[],
                state,
                "http://127.0.0.1:8080",
                refresh_ms,
                false,
            )
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn settings_for(server: &str) -> AppConfig {
    AppConfig {
        server: Some(server.to_string()),
        auto_discovery: false,
        ..AppConfig::default()
    }
}

fn press(state: &mut UiState, keys: &[KeyCode]) -> bool {
    let mut saved = false;
    for key in keys {
        saved |= state.settings_handle_key(*key);
    }
    saved
}

fn type_keys(state: &mut UiState, text: &str) {
    for ch in text.chars() {
        assert!(!state.settings_handle_key(KeyCode::Char(ch)));
    }
}

/// Focus the server list from a freshly opened dialog (Host field).
fn focus_server_list(state: &mut UiState) {
    assert!(!state.settings_handle_key(KeyCode::BackTab));
    assert!(state.settings_server_list_focused());
}

fn render_llm_panel(llm: &LlmStats, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_llm(frame, area, llm, &UiState::default(), &GpuMapping::None))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn summary(
    key: &str,
    connected: bool,
    slots_available: bool,
    busy: u64,
    total: u64,
) -> ServerSummary {
    ServerSummary {
        key: key.to_string(),
        endpoint: format!("http://127.0.0.1:{}", 8080 + key.len()),
        label: key.to_string(),
        connected,
        slots_available,
        busy_slots: busy,
        slot_count: total,
        ..Default::default()
    }
}
