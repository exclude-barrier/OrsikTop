mod app;
mod config;
mod cpu;
mod gpu;
mod llama;
mod ui;

use std::{fs, io, path::Path};

use clap::Parser;
use crossterm::{
    cursor::Show,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";

#[derive(Parser, Debug)]
#[command(name = "orsiktop", version, about = "btop for local LLM Orks")]
struct Args {
    /// llama.cpp server base URL. When omitted, OrsikTop auto-discovers a local llama serve / llama-server process.
    #[arg(long, env = "ORSIKTOP_SERVER")]
    server: Option<String>,

    /// Refresh interval in milliseconds. Overrides the saved setting for this run.
    #[arg(
        short = 'i',
        long = "interval-ms",
        alias = "interval",
        env = "ORSIKTOP_INTERVAL_MS"
    )]
    interval_ms: Option<u64>,

    /// NVIDIA GPU index to monitor. Overrides the saved setting for this run.
    #[arg(long, env = "ORSIKTOP_GPU_INDEX")]
    gpu_index: Option<u32>,
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut settings = config::load();
    if let Some(server) = args.server.filter(|value| !value.trim().is_empty()) {
        settings.server = Some(server);
        settings.auto_discovery = false;
    }
    if let Some(interval_ms) = args.interval_ms {
        settings.refresh_ms = interval_ms;
    }
    if let Some(gpu_index) = args.gpu_index {
        settings.gpu_index = gpu_index;
    }
    settings = settings.sanitized();
    let server = resolve_server(&settings);

    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app::run(&mut terminal, &server, settings)
}

fn resolve_server(settings: &config::AppConfig) -> String {
    let discovered = settings
        .auto_discovery
        .then(discover_local_llama_server)
        .flatten();
    discovered
        .or_else(|| settings.server.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER.to_string())
}

fn discover_local_llama_server() -> Option<String> {
    let mut candidates = Vec::new();

    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name().to_string_lossy().parse::<u32>().ok();
        let Some(pid) = pid else {
            continue;
        };

        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let args = parse_cmdline(&cmdline);
        if !is_llama_server_process(&args) {
            continue;
        }

        candidates.push((pid, endpoint_from_args(&args)));
    }

    candidates.sort_by_key(|(pid, _)| *pid);
    candidates.into_iter().next().map(|(_, endpoint)| endpoint)
}

fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

fn is_llama_server_process(args: &[String]) -> bool {
    let Some(executable) = args.first() else {
        return false;
    };
    let executable = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);

    executable.contains("llama-server")
        || (executable == "llama" && args.get(1).is_some_and(|arg| arg == "serve"))
}

fn endpoint_from_args(args: &[String]) -> String {
    let host = cli_value(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = cli_value(args, "--port")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let host = connect_host(&host);
    format!("http://{host}:{port}")
}

fn connect_host(host: &str) -> String {
    let host = host.trim();
    match host {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        "::1" | "[::1]" => "[::1]".to_string(),
        _ if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
        _ => host.to_string(),
    }
}

fn cli_value(args: &[String], flag: &str) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if arg == flag {
            return args.get(index + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_both_llama_server_cli_forms() {
        let server = vec!["/usr/bin/llama-server".to_string()];
        let serve = vec![
            "/home/user/.local/bin/llama".to_string(),
            "serve".to_string(),
        ];
        let unrelated = vec!["/usr/bin/llama".to_string(), "chat".to_string()];

        assert!(is_llama_server_process(&server));
        assert!(is_llama_server_process(&serve));
        assert!(!is_llama_server_process(&unrelated));
    }

    #[test]
    fn discovers_endpoint_from_llama_serve_args() {
        let args = vec![
            "/home/user/.local/bin/llama".to_string(),
            "serve".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "8081".to_string(),
        ];

        assert_eq!(endpoint_from_args(&args), "http://127.0.0.1:8081");
    }

    #[test]
    fn wildcard_bind_is_reached_through_loopback() {
        let args = vec![
            "/usr/bin/llama-server".to_string(),
            "--host=0.0.0.0".to_string(),
            "--port=9090".to_string(),
        ];

        assert_eq!(endpoint_from_args(&args), "http://127.0.0.1:9090");
    }

    #[test]
    fn manual_server_is_used_when_auto_discovery_is_disabled() {
        let settings = config::AppConfig {
            server: Some("http://10.0.0.5:8080".to_string()),
            auto_discovery: false,
            ..config::AppConfig::default()
        };
        assert_eq!(resolve_server(&settings), "http://10.0.0.5:8080");
    }
}
