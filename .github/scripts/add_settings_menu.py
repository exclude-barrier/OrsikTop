from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"anchor not found: {label}")
    return text.replace(old, new, 1)

# config.rs
Path("src/config.rs").write_text(r'''use std::{env, fs, io, path::PathBuf};

const SERVER_KEY: &str = "server";

pub fn load_server() -> Option<String> {
    let path = config_path()?;
    let text = fs::read_to_string(path).ok()?;
    parse_server(&text)
}

pub fn save_server(server: &str) -> io::Result<()> {
    let path = config_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME/XDG_CONFIG_HOME is unavailable")
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{SERVER_KEY}={}\n", server.trim()))
}

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("orsiktop/config"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/orsiktop/config"))
}

fn parse_server(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != SERVER_KEY {
            return None;
        }
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_server() {
        assert_eq!(
            parse_server("server=http://10.0.0.7:9090\n"),
            Some("http://10.0.0.7:9090".to_string())
        );
    }
}
''')

# main.rs
path = Path("src/main.rs")
text = path.read_text()
text = replace_once(text, "mod app;\n", "mod app;\nmod config;\n", "main module list")
text = replace_once(
    text,
    '''    explicit\n        .filter(|value| !value.trim().is_empty())\n        .or_else(discover_local_llama_server)\n        .unwrap_or_else(|| DEFAULT_SERVER.to_string())\n''',
    '''    explicit\n        .filter(|value| !value.trim().is_empty())\n        .or_else(config::load_server)\n        .or_else(discover_local_llama_server)\n        .unwrap_or_else(|| DEFAULT_SERVER.to_string())\n''',
    "server precedence",
)
path.write_text(text)

# app.rs
path = Path("src/app.rs")
text = path.read_text()
text = replace_once(
    text,
    '''        mpsc::{self, SyncSender, TrySendError},\n''',
    '''        mpsc::{self, Receiver, SyncSender, TrySendError},\n''',
    "receiver import",
)
text = replace_once(
    text,
    '''use crate::{\n    cpu::{detect_cpu_topology, CpuTopology},\n''',
    '''use crate::{\n    config,\n    cpu::{detect_cpu_topology, CpuTopology},\n''',
    "config import",
)
text = replace_once(
    text,
    '''    let mut refresh_ms = initial_refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);\n''',
    '''    let mut server = server.to_string();\n    let mut refresh_ms = initial_refresh_ms.clamp(MIN_REFRESH_MS, MAX_REFRESH_MS);\n''',
    "mutable server",
)
text = replace_once(
    text,
    '''    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmStats>(2);\n''',
    '''    let (llm_tx, llm_rx) = mpsc::sync_channel::<LlmStats>(2);\n    let (server_tx, server_rx) = mpsc::channel::<String>();\n''',
    "server channel",
)
text = replace_once(
    text,
    '''    spawn_llm_worker(\n        server.to_string(),\n        Arc::clone(&refresh_shared),\n        Arc::clone(&stop),\n        llm_tx,\n    );\n''',
    '''    spawn_llm_worker(\n        server.clone(),\n        Arc::clone(&refresh_shared),\n        Arc::clone(&stop),\n        llm_tx,\n        server_rx,\n    );\n''',
    "spawn llm worker",
)
text = replace_once(
    text,
    '''                server,\n                refresh_ms,\n''',
    '''                &server,\n                refresh_ms,\n''',
    "draw current server",
)

old_event = '''                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {\n                    KeyCode::Char('q') => break,\n                    KeyCode::Char('h') => ui_state.toggle_help(),\n                    KeyCode::Esc if ui_state.is_help_open() => ui_state.close_help(),\n                    _ if ui_state.is_help_open() => {}\n                    KeyCode::Char('-') | KeyCode::Char('[') => {\n'''
new_event = '''                Event::Key(key) if key.kind == KeyEventKind::Press && ui_state.is_settings_open() => {\n                    match key.code {\n                        KeyCode::Esc => ui_state.close_settings(),\n                        KeyCode::Tab | KeyCode::Down => ui_state.settings_next_field(),\n                        KeyCode::BackTab | KeyCode::Up => ui_state.settings_previous_field(),\n                        KeyCode::Backspace => ui_state.settings_backspace(),\n                        KeyCode::Enter => match ui_state.settings_endpoint() {\n                            Ok(endpoint) => match config::save_server(&endpoint) {\n                                Ok(()) => {\n                                    server = endpoint.clone();\n                                    snapshot.llm = LlmStats::default();\n                                    let _ = server_tx.send(endpoint);\n                                    ui_state.close_settings();\n                                }\n                                Err(err) => ui_state\n                                    .set_settings_error(format!("Could not save settings: {err}")),\n                            },\n                            Err(err) => ui_state.set_settings_error(err),\n                        },\n                        KeyCode::Char(ch) => ui_state.settings_insert_char(ch),\n                        _ => {}\n                    }\n                }\n                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {\n                    KeyCode::Char('q') => break,\n                    KeyCode::Char('h') => ui_state.toggle_help(),\n                    KeyCode::Esc if ui_state.is_help_open() => ui_state.close_help(),\n                    KeyCode::Esc => ui_state.open_settings(&server),\n                    _ if ui_state.is_help_open() => {}\n                    KeyCode::Char('-') | KeyCode::Char('[') => {\n'''
text = replace_once(text, old_event, new_event, "settings key handling")
text = replace_once(
    text,
    '''                Event::Mouse(mouse) if !ui_state.is_help_open() => match mouse.kind {\n''',
    '''                Event::Mouse(mouse)\n                    if !ui_state.is_help_open() && !ui_state.is_settings_open() =>\n                {\n                    match mouse.kind {\n''',
    "mouse guard open",
)
text = replace_once(
    text,
    '''                    _ => {}\n                },\n                _ => {}\n''',
    '''                    _ => {}\n                    }\n                }\n                _ => {}\n''',
    "mouse guard close",
)

old_worker = '''fn spawn_llm_worker(\n    server: String,\n    refresh_ms: Arc<AtomicU64>,\n    stop: Arc<AtomicBool>,\n    tx: SyncSender<LlmStats>,\n) {\n    thread::spawn(move || {\n        let mut llama = LlamaMonitor::new(&server).ok();\n        let llama_init_error = if llama.is_none() {\n            "failed to initialize HTTP client".to_string()\n        } else {\n            String::new()\n        };\n        let mut last_good_llm: Option<(LlmStats, Instant)> = None;\n\n        while !stop.load(Ordering::Relaxed) {\n            let cycle_started = Instant::now();\n'''
new_worker = '''fn spawn_llm_worker(\n    server: String,\n    refresh_ms: Arc<AtomicU64>,\n    stop: Arc<AtomicBool>,\n    tx: SyncSender<LlmStats>,\n    server_rx: Receiver<String>,\n) {\n    thread::spawn(move || {\n        let mut current_server = server;\n        let mut llama = LlamaMonitor::new(&current_server).ok();\n        let mut llama_init_error = if llama.is_none() {\n            "failed to initialize HTTP client".to_string()\n        } else {\n            String::new()\n        };\n        let mut last_good_llm: Option<(LlmStats, Instant)> = None;\n\n        while !stop.load(Ordering::Relaxed) {\n            let cycle_started = Instant::now();\n            while let Ok(next_server) = server_rx.try_recv() {\n                current_server = next_server;\n                llama = LlamaMonitor::new(&current_server).ok();\n                llama_init_error = if llama.is_none() {\n                    "failed to initialize HTTP client".to_string()\n                } else {\n                    String::new()\n                };\n                last_good_llm = None;\n            }\n'''
text = replace_once(text, old_worker, new_worker, "dynamic llm worker")
path.write_text(text)

# ui.rs
path = Path("src/ui.rs")
text = path.read_text()
text = replace_once(
    text,
    '''enum ProcessSortKey {\n    Pid,\n    Program,\n    Cpu,\n    Memory,\n    Threads,\n}\n''',
    '''enum ProcessSortKey {\n    Pid,\n    Program,\n    Cpu,\n    Memory,\n    Threads,\n}\n\n#[derive(Copy, Clone, Debug, PartialEq, Eq)]\nenum SettingsField {\n    Host,\n    Port,\n}\n''',
    "settings field enum",
)
text = replace_once(
    text,
    '''    help_open: bool,\n}\n''',
    '''    help_open: bool,\n    settings_open: bool,\n    settings_field: SettingsField,\n    settings_host: String,\n    settings_port: String,\n    settings_error: Option<String>,\n}\n''',
    "ui state settings fields",
)
text = replace_once(
    text,
    '''            help_open: false,\n        }\n''',
    '''            help_open: false,\n            settings_open: false,\n            settings_field: SettingsField::Host,\n            settings_host: "127.0.0.1".to_string(),\n            settings_port: "8080".to_string(),\n            settings_error: None,\n        }\n''',
    "ui state settings defaults",
)

methods_anchor = '''    pub fn toggle_help(&mut self) {\n        self.help_open = !self.help_open;\n    }\n\n    pub fn close_help(&mut self) {\n        self.help_open = false;\n    }\n\n    pub fn is_help_open(&self) -> bool {\n        self.help_open\n    }\n'''
methods_new = '''    pub fn toggle_help(&mut self) {\n        self.settings_open = false;\n        self.help_open = !self.help_open;\n    }\n\n    pub fn close_help(&mut self) {\n        self.help_open = false;\n    }\n\n    pub fn is_help_open(&self) -> bool {\n        self.help_open\n    }\n\n    pub fn open_settings(&mut self, server: &str) {\n        let (host, port) = endpoint_parts(server);\n        self.help_open = false;\n        self.settings_open = true;\n        self.settings_field = SettingsField::Host;\n        self.settings_host = host;\n        self.settings_port = port.to_string();\n        self.settings_error = None;\n    }\n\n    pub fn close_settings(&mut self) {\n        self.settings_open = false;\n        self.settings_error = None;\n    }\n\n    pub fn is_settings_open(&self) -> bool {\n        self.settings_open\n    }\n\n    pub fn settings_next_field(&mut self) {\n        self.settings_field = match self.settings_field {\n            SettingsField::Host => SettingsField::Port,\n            SettingsField::Port => SettingsField::Host,\n        };\n        self.settings_error = None;\n    }\n\n    pub fn settings_previous_field(&mut self) {\n        self.settings_next_field();\n    }\n\n    pub fn settings_backspace(&mut self) {\n        match self.settings_field {\n            SettingsField::Host => {\n                self.settings_host.pop();\n            }\n            SettingsField::Port => {\n                self.settings_port.pop();\n            }\n        }\n        self.settings_error = None;\n    }\n\n    pub fn settings_insert_char(&mut self, ch: char) {\n        match self.settings_field {\n            SettingsField::Host if !ch.is_control() && !ch.is_whitespace() => {\n                self.settings_host.push(ch);\n            }\n            SettingsField::Port if ch.is_ascii_digit() && self.settings_port.len() < 5 => {\n                self.settings_port.push(ch);\n            }\n            _ => {}\n        }\n        self.settings_error = None;\n    }\n\n    pub fn settings_endpoint(&self) -> Result<String, String> {\n        build_endpoint(&self.settings_host, &self.settings_port)\n    }\n\n    pub fn set_settings_error(&mut self, error: String) {\n        self.settings_error = Some(error);\n    }\n'''
text = replace_once(text, methods_anchor, methods_new, "settings state methods")

text = replace_once(
    text,
    '''    if state.help_open {\n        draw_help_popup(frame, area);\n    }\n}\n''',
    '''    if state.settings_open {\n        draw_settings_popup(frame, area, state);\n    } else if state.help_open {\n        draw_help_popup(frame, area);\n    }\n}\n''',
    "draw settings popup",
)

help_line = '''        Line::from(vec![key("h"), desc("Toggle this help")]),\n'''
text = replace_once(
    text,
    help_line,
    help_line + '''        Line::from(vec![key("Esc"), desc("Open settings")]),\n''',
    "help settings shortcut",
)

popup_anchor = '''pub fn refresh_controls(area: Rect) -> Option<RefreshControls> {\n'''
popup_code = r'''fn draw_settings_popup(frame: &mut Frame, area: Rect, state: &UiState) {
    let width = area.width.saturating_sub(6).min(70);
    let height = 13.min(area.height.saturating_sub(4));
    if width < 50 || height < 11 {
        return;
    }

    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" SETTINGS · OrsikTop ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let host_selected = state.settings_field == SettingsField::Host;
    let port_selected = state.settings_field == SettingsField::Port;
    let field_style = |selected: bool| {
        let style = Style::default().fg(if selected { BRIGHT_GREEN } else { WHITE });
        if selected {
            style.bg(PROCESS_SELECTED_BG).add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let cursor = |selected: bool| if selected { "▏" } else { "" };
    let preview = state
        .settings_endpoint()
        .unwrap_or_else(|_| "http://…".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            " LLM ENDPOINT",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Host / IP  ", Style::default().fg(MUTED)),
            Span::styled(
                format!(" {:<40}{} ", fit_cell(&state.settings_host, 40), cursor(host_selected)),
                field_style(host_selected),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Port       ", Style::default().fg(MUTED)),
            Span::styled(
                format!(" {:<8}{} ", state.settings_port, cursor(port_selected)),
                field_style(port_selected),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Endpoint   ", Style::default().fg(MUTED)),
            Span::styled(preview, Style::default().fg(CYAN)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Tab / ↑↓ select   Type to edit   Enter apply + save   Esc cancel",
            Style::default().fg(MUTED),
        )),
    ];

    if let Some(error) = state.settings_error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn endpoint_parts(server: &str) -> (String, u16) {
    let compact = server
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');

    if let Some(rest) = compact.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(8080);
            return (host.to_string(), port);
        }
    }

    if let Some((host, port)) = compact.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (compact.to_string(), 8080)
}

fn build_endpoint(host: &str, port: &str) -> Result<String, String> {
    let host = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        return Err("Host / IP must not be empty".to_string());
    }
    if host.contains('/') || host.chars().any(char::is_whitespace) {
        return Err("Host / IP contains invalid characters".to_string());
    }
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "Port must be between 1 and 65535".to_string())?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("http://{host}:{port}"))
}

'''
text = replace_once(text, popup_anchor, popup_code + popup_anchor, "settings popup helpers")

# Add focused UI tests before final test-module closing by inserting near endpoint compact test.
test_anchor = '''    fn endpoint_is_compact() {\n        assert_eq!(compact_endpoint("http://127.0.0.1:8081/"), "127.0.0.1:8081");\n    }\n'''
if test_anchor in text:
    text = text.replace(
        test_anchor,
        test_anchor + '''\n    #[test]\n    fn settings_endpoint_parses_and_rebuilds_ipv4() {\n        assert_eq!(endpoint_parts("http://10.0.0.7:9090"), ("10.0.0.7".to_string(), 9090));\n        assert_eq!(build_endpoint("10.0.0.7", "9090").unwrap(), "http://10.0.0.7:9090");\n    }\n\n    #[test]\n    fn settings_endpoint_supports_ipv6() {\n        assert_eq!(build_endpoint("::1", "8081").unwrap(), "http://[::1]:8081");\n    }\n''',
        1,
    )
else:
    raise SystemExit("UI endpoint test anchor not found")

path.write_text(text)
