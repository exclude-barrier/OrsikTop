//! CFA-99 (SEC-01): the settings dialog must never paint URL credentials.
//!
//! The configured endpoint may carry userinfo or a query token (see
//! `ResolvedServer::endpoint`). The dialog keeps that value untouched for
//! saving, but its endpoint preview and the editable host field are display
//! paths and must not show userinfo, path, query or fragment. The preview goes
//! through `crate::redact::safe_endpoint`; the host field is cleaned at the
//! source in `endpoint_parts`.
//!
//! All values are synthetic. Assertion messages name a label, never the
//! secret itself, so a failure cannot echo a credential into the test log.
//! Deterministic: pure `TestBackend`, no real server.

use super::{draw_settings_popup, UiState};
use crate::config::AppConfig;
use ratatui::{backend::TestBackend, layout::Rect, Terminal};

const WITH_CREDENTIALS: &str = "http://demo-user:demo-password@10.0.0.7:8080/v1?token=demo_secret";

/// Synthetic confidential parts, paired with a label for failure messages.
const SECRETS: [(&str, &str); 3] = [
    ("demo-user", "username"),
    ("demo-password", "password"),
    ("demo_secret", "query token"),
];

fn opened(server: &str) -> UiState {
    let settings = AppConfig {
        server: Some(server.to_string()),
        auto_discovery: false,
        ..AppConfig::default()
    };
    let mut state = UiState::default();
    state.open_settings(server, &settings);
    state
}

/// Render the settings popup on an 80×24 terminal and join the buffer into a
/// single string (rows separated by newlines).
fn render(state: &UiState) -> String {
    let area = Rect::new(0, 0, 80, 24);
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    terminal
        .draw(|frame| draw_settings_popup(frame, area, state))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_secrets(text: &str, place: &str) {
    for (secret, label) in SECRETS {
        assert!(!text.contains(secret), "{label} leaked into the {place}");
    }
}

#[test]
fn preview_redacts_credentials_of_an_unchanged_endpoint() {
    let state = opened(WITH_CREDENTIALS);
    let screen = render(&state);
    assert_no_secrets(&screen, "settings popup");
    assert_no_secrets(&state.settings_host, "host field");
    assert!(
        screen.contains("http://10.0.0.7:8080/…?…"),
        "preview must keep scheme, host and port and mark path and query"
    );
}

#[test]
fn preview_redacts_credentials_after_a_host_edit_while_saving_keeps_them() {
    let mut state = opened(WITH_CREDENTIALS);
    state.settings_host = "10.0.0.8".to_string();
    let screen = render(&state);
    assert_no_secrets(&screen, "settings popup");
    assert!(
        screen.contains("http://10.0.0.8:8080/…?…"),
        "preview must show the edited host"
    );
    // Storage is unchanged: only the display is redacted.
    let saved = state.settings_config().unwrap().server.unwrap();
    for (secret, label) in SECRETS {
        assert!(saved.contains(secret), "saved endpoint lost its {label}");
    }
}

#[test]
fn preview_keeps_the_https_scheme_without_credentials() {
    let state = opened("https://demo-user:demo-password@example.invalid");
    let screen = render(&state);
    assert_no_secrets(&screen, "settings popup");
    assert!(
        screen.contains("https://example.invalid"),
        "preview must keep the https scheme"
    );
}

#[test]
fn scheme_less_endpoints_leak_neither_into_preview_nor_host_field() {
    for (server, host) in [
        ("demo-user:demo-password@10.0.0.7:8080", "10.0.0.7"),
        ("localhost:8080?token=demo_secret", "localhost"),
        ("10.0.0.7:8080/v1?token=demo_secret", "10.0.0.7"),
        // Out-of-range port: the URL parser rejects it, so the textual
        // fallback in `endpoint_parts` has to strip the userinfo itself.
        ("demo-user:demo-password@10.0.0.7:99999", "10.0.0.7:99999"),
    ] {
        let state = opened(server);
        assert_no_secrets(&state.settings_host, "host field");
        assert_eq!(state.settings_host, host);
        assert_no_secrets(&render(&state), "settings popup");
    }
}

#[test]
fn plain_scheme_less_endpoint_preview_shows_host_and_port() {
    let screen = render(&opened("localhost:8080"));
    let preview_row = screen
        .lines()
        .find(|row| row.contains(" Endpoint "))
        .expect("endpoint preview row");
    assert!(
        preview_row.contains("localhost:8080"),
        "a scheme-less endpoint without secrets stays readable"
    );
}
