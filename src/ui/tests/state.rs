//! `UiState` behavior tests: histories, server selector, settings, server list, refresh.
use super::super::*;
use super::{focus_server_list, press, settings_for, type_keys};

#[test]
fn llm_rate_history_idle_requires_real_activity() {
    let mut history = VecDeque::new();
    history.push_back(TimedSample {
        at: Instant::now(),
        value: 0.0,
    });
    assert!(!llm_rate_history_has_activity(&history));
    history.push_back(TimedSample {
        at: Instant::now(),
        value: 12.5,
    });
    assert!(llm_rate_history_has_activity(&history));
}

#[test]
fn help_popup_state_toggles_and_closes() {
    let mut state = UiState::default();
    assert!(!state.is_help_open());
    state.toggle_help();
    assert!(state.is_help_open());
    state.toggle_help();
    assert!(!state.is_help_open());
    state.toggle_help();
    state.close_help();
    assert!(!state.is_help_open());
}

#[test]
fn history_uses_a_fixed_time_window() {
    let now = Instant::now();
    let mut history = VecDeque::new();
    push_history_at(
        &mut history,
        10.0,
        now - HISTORY_WINDOW - Duration::from_secs(1),
    );
    push_history_at(&mut history, 20.0, now);
    assert_eq!(history.len(), 1);
    assert_eq!(history.back().unwrap().value, 20.0);
}

#[test]
fn llm_history_ignores_held_reconnect_samples() {
    let mut state = UiState::default();
    state.record_llm_sample(
        "srv",
        "srv",
        &LlmStats {
            connected: true,
            metrics_available: true,
            prompt_tps: Some(1200.0),
            generation_tps: Some(75.0),
            ..LlmStats::default()
        },
    );
    assert_eq!(state.llm_history.prefill.len(), 1);
    assert_eq!(state.llm_history.decode.len(), 1);
    assert!(state.llm_history.last_fresh_at.is_some());
    assert!(state.llm_history.connected_since.is_some());

    state.record_llm_sample(
        "srv",
        "srv",
        &LlmStats {
            connected: true,
            reconnecting: true,
            prompt_tps: Some(9999.0),
            generation_tps: Some(9999.0),
            ..LlmStats::default()
        },
    );
    assert_eq!(state.llm_history.prefill.len(), 1);
    assert_eq!(state.llm_history.decode.len(), 1);
}

#[test]
fn llm_history_skips_samples_without_metrics() {
    // A reachable server whose /metrics is unavailable has unknown
    // throughput: no point may enter the rate history (that would draw a
    // fabricated flat line).
    let mut state = UiState::default();
    state.record_llm_sample(
        "srv",
        "srv",
        &LlmStats {
            connected: true,
            metrics_available: false,
            prompt_tps: Some(9999.0),
            generation_tps: Some(9999.0),
            ..LlmStats::default()
        },
    );
    assert!(state.llm_history.prefill.is_empty());
    assert!(state.llm_history.decode.is_empty());
    assert!(
        state.llm_history.last_fresh_at.is_some(),
        "the link is still fresh even without metrics"
    );
}

#[test]
fn per_server_histories_are_independent_and_survive_selection() {
    let mut state = UiState::default();
    // A accumulates while it is the selected server.
    state.record_llm_sample("A", "A", &llm_with(1200.0, 60.0));
    state.record_llm_sample("A", "A", &llm_with(1300.0, 70.0));
    assert_eq!(state.llm_history.prefill.len(), 2);

    // B is monitored in the background while A stays selected: B's sample
    // must not touch A's active view, but must be recorded for B.
    state.record_llm_sample("B", "A", &llm_with(10.0, 1.0));
    assert_eq!(
        state.llm_history.prefill.len(),
        2,
        "a background sample must not alter the selected server's history"
    );

    // Switch to B: B's own accumulated history is shown immediately.
    state.select_llm_history("B");
    assert_eq!(state.llm_history.prefill.len(), 1);
    // Switch back to A: A's history is preserved, not reset.
    state.select_llm_history("A");
    assert_eq!(state.llm_history.prefill.len(), 2);
}

#[test]
fn reset_and_retain_bound_per_server_history() {
    let mut state = UiState::default();
    state.record_llm_sample("A", "A", &llm_with(1.0, 1.0));
    state.record_llm_sample("B", "A", &llm_with(1.0, 1.0));

    // A proven restart of B resets only B.
    state.reset_llm_history("B");
    state.select_llm_history("B");
    assert!(state.llm_history.prefill.is_empty());
    state.select_llm_history("A");
    assert_eq!(state.llm_history.prefill.len(), 1, "A is untouched");

    // Retaining only the live keys drops the servers that are gone.
    state.record_llm_sample("B", "A", &llm_with(2.0, 2.0));
    let live: HashSet<String> = ["A".to_string()].into_iter().collect();
    state.retain_llm_histories(&live);
    assert!(state.llm_histories.contains_key("A"));
    assert!(!state.llm_histories.contains_key("B"));
}

#[test]
fn server_selector_navigation_wraps_and_clamps() {
    let mut state = UiState::default();
    assert!(!state.is_server_selector_open());
    state.open_server_selector(2);
    assert!(state.is_server_selector_open());
    assert_eq!(state.server_selector_index(3), Some(2), "opens on selected");
    state.move_server_selection(1, 3);
    assert_eq!(state.server_selector_index(3), Some(0), "wraps forward");
    state.move_server_selection(-1, 3);
    assert_eq!(state.server_selector_index(3), Some(2), "wraps backward");
    state.move_server_selection(1, 0);
    assert_eq!(state.server_selector_index(0), None);
    state.close_server_selector();
    assert!(!state.is_server_selector_open());
}

#[test]
fn settings_endpoint_parses_and_rebuilds_ipv4() {
    assert_eq!(
        endpoint_parts("http://10.0.0.7:9090"),
        ("10.0.0.7".to_string(), 9090)
    );
    assert_eq!(
        build_endpoint("10.0.0.7", "9090").unwrap(),
        "http://10.0.0.7:9090"
    );
}

#[test]
fn expanded_settings_build_runtime_config() {
    let mut state = UiState::default();
    let settings = AppConfig {
        server: Some("http://127.0.0.1:8081".to_string()),
        servers: Vec::new(),
        gpu_selector: GpuSelector::PciBusId("0000:41:00.0".to_string()),
        refresh_ms: 200,
        process_refresh_ms: 1500,
        offline_grace_ms: 3000,
        auto_discovery: false,
    };
    state.open_settings("http://127.0.0.1:8081", &settings);
    assert_eq!(state.settings_config().unwrap(), settings);
}

#[test]
fn settings_auto_discovery_toggle_changes_saved_value() {
    let mut state = UiState {
        settings_field: SettingsField::AutoDiscovery,
        ..UiState::default()
    };
    let before = state.settings_auto_discovery;
    state.settings_toggle_selected();
    assert_ne!(state.settings_auto_discovery, before);
}

#[test]
fn settings_endpoint_supports_ipv6() {
    assert_eq!(build_endpoint("::1", "8081").unwrap(), "http://[::1]:8081");
}

#[test]
fn opening_and_saving_an_https_endpoint_preserves_it() {
    let settings = settings_for("https://example.invalid:8443");
    let mut state = UiState::default();
    state.open_settings("https://example.invalid:8443", &settings);
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some("https://example.invalid:8443")
    );
}

#[test]
fn changing_only_refresh_keeps_endpoint_scheme_and_path() {
    let server = "https://example.invalid:8443/v1?x=1";
    let settings = settings_for(server);
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    // Change only the refresh interval.
    state.settings_field = SettingsField::Refresh;
    state.settings_refresh_ms = "500".to_string();
    let saved = state.settings_config().unwrap();
    assert_eq!(saved.refresh_ms, 500);
    assert_eq!(saved.server.as_deref(), Some(server));
}

#[test]
fn https_without_an_explicit_port_round_trips() {
    let settings = settings_for("https://example.invalid");
    let mut state = UiState::default();
    state.open_settings("https://example.invalid", &settings);
    assert_eq!(state.settings_port, "443");
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some("https://example.invalid")
    );
}

#[test]
fn editing_host_preserves_scheme_port_and_path() {
    let mut state = UiState::default();
    let settings = settings_for("https://old.invalid:8443/v1");
    state.open_settings("https://old.invalid:8443/v1", &settings);
    state.settings_host = "new.invalid".to_string();
    let endpoint = state.settings_config().unwrap().server.unwrap();
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("new.invalid"));
    assert_eq!(url.port(), Some(8443));
    assert_eq!(url.path(), "/v1");
}

#[test]
fn ipv6_and_hostname_endpoints_round_trip() {
    for server in ["https://[::1]:8443", "http://llama.local:9090"] {
        let settings = settings_for(server);
        let mut state = UiState::default();
        state.open_settings(server, &settings);
        assert_eq!(
            state.settings_config().unwrap().server.as_deref(),
            Some(server),
            "endpoint {server} changed"
        );
    }
}

#[test]
fn server_list_add_persists_and_round_trips() {
    let dir = std::env::temp_dir().join(format!("orsiktop-ui-servers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config");

    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    state.settings_field = SettingsField::ServerList;
    add_server(&mut state, "http://10.0.0.5:9090");
    add_server(&mut state, "https://other.invalid:8443/v1");

    let saved = state.settings_config().unwrap();
    assert_eq!(saved.server.as_deref(), Some("http://127.0.0.1:8080"));
    assert_eq!(
        saved.servers,
        vec![
            "http://10.0.0.5:9090".to_string(),
            "https://other.invalid:8443/v1".to_string()
        ]
    );

    // Real persistence round-trip through the atomic config writer.
    crate::config::save_to_path(&path, &saved).unwrap();
    let loaded = crate::config::load_from_path(&path).unwrap();
    assert_eq!(loaded.server.as_deref(), Some("http://127.0.0.1:8080"));
    assert_eq!(loaded.servers, saved.servers);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn server_list_rejects_duplicates_against_primary_and_extras() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    state.settings_field = SettingsField::ServerList;
    add_server(&mut state, "http://10.0.0.5:9090");
    assert_eq!(state.settings_server_entries().len(), 1);

    // A different surface form of the primary endpoint is a duplicate.
    add_server(&mut state, "http://127.0.0.1:8080/");
    assert!(state.settings_server_error().is_some());
    assert_eq!(state.settings_server_entries().len(), 1);

    // A different surface form of an existing extra is a duplicate.
    add_server(&mut state, "http://10.0.0.5:9090/");
    assert!(state.settings_server_error().is_some());
    assert_eq!(state.settings_server_entries().len(), 1);
}

#[test]
fn server_list_duplicate_error_redacts_credentials() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    add_server(&mut state, "http://user:secretpw@10.0.0.5:9090");
    // Same endpoint through different credentials: a duplicate whose error
    // must not echo either secret.
    add_server(&mut state, "http://other:topsecret@10.0.0.5:9090");
    let error = state.settings_server_error().expect("duplicate error");
    assert!(!error.contains("secret"), "secret leaked: {error}");
    assert!(!error.contains("topsecret"), "secret leaked: {error}");
    assert!(!error.contains("secretpw"), "secret leaked: {error}");
}

#[test]
fn server_list_edit_and_delete_update_the_set() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    add_server(&mut state, "http://a:1");
    add_server(&mut state, "http://b:2");

    // Edit the second entry in place.
    state.settings_server_index = 1;
    state.settings_server_begin_edit();
    assert_eq!(state.settings_server_draft(), Some("http://b:2"));
    clear_draft(&mut state);
    type_into_draft(&mut state, "http://c:3");
    state.settings_server_draft_commit();
    assert_eq!(
        state.settings_server_entries(),
        ["http://a:1".to_string(), "http://c:3".to_string()]
    );

    // Delete the first entry.
    state.settings_server_index = 0;
    state.settings_server_delete();
    assert_eq!(state.settings_server_entries(), ["http://c:3".to_string()]);

    // Deleting the last entry clears the list and clamps the highlight.
    state.settings_server_delete();
    assert!(state.settings_server_entries().is_empty());
    assert_eq!(state.settings_server_index, 0);
}

#[test]
fn server_list_rejects_an_invalid_endpoint_without_changing_the_list() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    add_server(&mut state, "not a url");
    assert!(state.settings_server_error().is_some());
    assert!(state.settings_server_entries().is_empty());
}

#[test]
fn server_list_marks_entries_beyond_the_cap_as_not_monitored() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    // Primary plus 16 additional = 17 configured; exactly one is beyond the
    // 16-server monitoring cap.
    for index in 0..16u16 {
        add_server(&mut state, &format!("http://extra{index}:1"));
    }
    assert_eq!(state.settings_server_entries().len(), 16);
    let status = state.settings_server_monitoring();
    assert_eq!((status.monitored, status.overflow), (16, 1));
    assert_eq!(status.flags.iter().filter(|flag| **flag).count(), 15);
    assert!(
        !status.flags[15],
        "the 17th configured endpoint is not monitored"
    );
    // Saving keeps the unmonitored entry: nothing is silently dropped.
    assert_eq!(state.settings_config().unwrap().servers.len(), 16);
}

#[test]
fn settings_keys_add_edit_delete_servers_and_save() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    focus_server_list(&mut state);

    // Add two entries by keyboard.
    press(&mut state, &[KeyCode::Char('a')]);
    type_keys(&mut state, "http://10.0.0.5:9090");
    assert!(
        !state.settings_handle_key(KeyCode::Enter),
        "Enter commits the draft, not the dialog"
    );
    press(&mut state, &[KeyCode::Insert]);
    type_keys(&mut state, "http://10.0.0.6:9090");
    press(&mut state, &[KeyCode::Enter]);
    assert_eq!(state.settings_server_entries().len(), 2);

    // Edit the first one: ↑ to it, `e`, replace the port.
    press(&mut state, &[KeyCode::Up, KeyCode::Char('e')]);
    assert_eq!(state.settings_server_draft(), Some("http://10.0.0.5:9090"));
    press(&mut state, &[KeyCode::Backspace; 4]);
    type_keys(&mut state, "9191");
    press(&mut state, &[KeyCode::Enter]);
    assert_eq!(
        state.settings_server_entries(),
        [
            "http://10.0.0.5:9191".to_string(),
            "http://10.0.0.6:9090".to_string()
        ]
    );

    // Esc in a draft only discards the draft; the dialog stays open.
    press(&mut state, &[KeyCode::Char('a')]);
    type_keys(&mut state, "http://discard.me");
    press(&mut state, &[KeyCode::Esc]);
    assert!(state.is_settings_open());
    assert_eq!(state.settings_server_entries().len(), 2);

    // Delete the highlighted (first) entry, then Enter requests a save.
    press(&mut state, &[KeyCode::Char('d')]);
    assert_eq!(
        state.settings_server_entries(),
        ["http://10.0.0.6:9090".to_string()]
    );
    assert!(
        state.settings_handle_key(KeyCode::Enter),
        "Enter on the list saves"
    );
    assert_eq!(
        state.settings_config().unwrap().servers,
        vec!["http://10.0.0.6:9090".to_string()]
    );
}

#[test]
fn settings_keys_on_other_fields_are_unchanged_by_the_server_list() {
    let settings = settings_for("http://127.0.0.1:8080");
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    // `a`, `e`, `d` still type into a text field outside the list.
    press(&mut state, &[KeyCode::Tab, KeyCode::Tab]); // GPU
    press(&mut state, &[KeyCode::Backspace]);
    type_keys(&mut state, "ade");
    assert_eq!(state.settings_gpu, "ade");
    assert!(state.settings_server_entries().is_empty());
    assert!(!state.settings_server_is_editing());
    // ↓ from Auto discovery enters the list; ↓ on an empty list leaves it
    // for the first field again, ↑ from Host enters the list from the top.
    state.settings_field = SettingsField::AutoDiscovery;
    press(&mut state, &[KeyCode::Char(' ')]);
    assert!(state.settings_auto_discovery);
    press(&mut state, &[KeyCode::Down]);
    assert!(state.settings_server_list_focused());
    press(&mut state, &[KeyCode::Down]);
    assert_eq!(state.settings_field, SettingsField::Host);
    press(&mut state, &[KeyCode::Up]);
    assert!(state.settings_server_list_focused());
    press(&mut state, &[KeyCode::Up]);
    assert_eq!(state.settings_field, SettingsField::AutoDiscovery);
    // Esc outside a draft still closes the dialog.
    press(&mut state, &[KeyCode::Esc]);
    assert!(!state.is_settings_open());
}

#[test]
fn scheme_less_endpoint_round_trips_unchanged() {
    // Regression: a legacy `host:port` endpoint used to fail to save
    // because it parsed as a cannot-be-a-base URL, leaving `set_host` with
    // an empty base host.
    let server = "localhost:8080";
    let settings = settings_for(server);
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    assert_eq!(state.settings_host, "localhost");
    assert_eq!(state.settings_port, "8080");
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some(server)
    );
}

#[test]
fn scheme_less_endpoint_without_port_round_trips_unchanged() {
    // The field shows the legacy default port 8080 while the parsed
    // `http://…` base would report 80; that must not count as an edit.
    let server = "localhost";
    let settings = settings_for(server);
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    assert_eq!(state.settings_port, "8080");
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some(server)
    );
    state.settings_host = "other.local".to_string();
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some("http://other.local:8080/")
    );
}

#[test]
fn changing_only_refresh_saves_a_scheme_less_endpoint() {
    let server = "10.0.0.7:9090";
    let settings = settings_for(server);
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    state.settings_field = SettingsField::Refresh;
    state.settings_refresh_ms = "500".to_string();
    let saved = state.settings_config().unwrap();
    assert_eq!(saved.refresh_ms, 500);
    assert_eq!(saved.server.as_deref(), Some(server));
}

#[test]
fn editing_a_scheme_less_endpoint_adds_http_and_keeps_credentials() {
    let server = "demo-user:demo-password@localhost:8080";
    let settings = settings_for(server);
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    // The host field must hold the bare host, not the userinfo prefix.
    assert_eq!(state.settings_host, "localhost");
    assert_eq!(state.settings_port, "8080");
    // Unchanged, the endpoint (including its credentials) round-trips.
    assert_eq!(
        state.settings_config().unwrap().server.as_deref(),
        Some(server)
    );
    // Editing the host adds the http scheme and keeps host, port and
    // userinfo instead of dropping the credentials.
    state.settings_host = "other.local".to_string();
    let endpoint = state.settings_config().unwrap().server.unwrap();
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("other.local"));
    assert_eq!(url.port(), Some(8080));
    assert_eq!(url.username(), "demo-user");
    assert_eq!(url.password(), Some("demo-password"));
}

#[test]
fn llm_sample_interval_ema_smooths_refresh_jitter() {
    let mut average = None;
    for sample in [100.0, 130.0, 70.0, 115.0, 85.0] {
        average = smooth_llm_sample_interval(average, sample);
    }
    let average = average.unwrap();
    assert!((95.0..=105.0).contains(&average));
}

#[test]
fn llm_last_uses_smoothed_interval_while_online() {
    let state = UiState {
        llm_history: LlmHistory {
            sample_interval_ema_ms: Some(101.4),
            last_fresh_at: Some(Instant::now()),
            ..LlmHistory::default()
        },
        ..UiState::default()
    };
    let llm = LlmStats {
        connected: true,
        ..LlmStats::default()
    };
    assert_eq!(llm_last_sample_text(&state, &llm), "~101 ms avg");
}

fn llm_with(prompt: f64, generation: f64) -> LlmStats {
    LlmStats {
        connected: true,
        metrics_available: true,
        prompt_tps: Some(prompt),
        generation_tps: Some(generation),
        ..LlmStats::default()
    }
}

fn type_into_draft(state: &mut UiState, text: &str) {
    for ch in text.chars() {
        state.settings_server_draft_insert_char(ch);
    }
}

fn add_server(state: &mut UiState, endpoint: &str) {
    state.settings_server_begin_add();
    type_into_draft(state, endpoint);
    state.settings_server_draft_commit();
}

fn clear_draft(state: &mut UiState) {
    while matches!(state.settings_server_draft(), Some(d) if !d.is_empty()) {
        state.settings_server_draft_backspace();
    }
}
