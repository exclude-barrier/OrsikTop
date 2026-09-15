mod app;
mod config;
mod cpu;
#[allow(dead_code)] // consumed by the GPU provider layer (S5+)
mod discovery;
mod domain;
mod gpu;
mod llama;
mod system;
mod ui;

use std::{env, fs, io, path::Path, path::PathBuf, process::Command};

use clap::{Parser, Subcommand};
use crossterm::{
    cursor::Show,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::domain::GpuSelector;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";
const CARGO_UPDATE_COMMAND: &str =
    "cargo install --git https://github.com/exclude-barrier/OrsikTop --locked --force";

#[derive(Subcommand, Debug)]
enum Commands {
    /// Update a standalone OrsikTop installation to the latest release.
    Update,
    /// Remove the OrsikTop binaries, and optionally the saved config.
    Uninstall {
        /// Also remove the saved OrsikTop config (endpoint, GPU index, refresh settings).
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "orsiktop",
    version,
    about = "Fast terminal monitoring for local LLM Orks"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

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

    /// NVIDIA GPU to monitor: a PCI bus ID (`0000:41:00.0`), a vendor UUID
    /// (`GPU-…`), or a legacy NVML index. Overrides the saved setting.
    #[arg(long, env = "ORSIKTOP_GPU")]
    gpu: Option<String>,

    /// Legacy NVIDIA GPU index. Prefer `--gpu`.
    #[arg(long, conflicts_with = "gpu", env = "ORSIKTOP_GPU_INDEX")]
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

    if let Some(command) = args.command.as_ref() {
        return match command {
            Commands::Update => run_updater(),
            Commands::Uninstall { purge } => run_uninstall(*purge),
        };
    }

    let mut settings = config::load();
    if let Some(server) = args.server.filter(|value| !value.trim().is_empty()) {
        settings.server = Some(server);
        settings.auto_discovery = false;
    }
    if let Some(interval_ms) = args.interval_ms {
        settings.refresh_ms = interval_ms;
    }
    if let Some(gpu) = args.gpu {
        settings.gpu_selector = GpuSelector::parse(&gpu);
    } else if let Some(gpu_index) = args.gpu_index {
        settings.gpu_selector = GpuSelector::Index(gpu_index);
    }
    settings = settings.sanitized();
    let server = resolve_server(&settings);
    let server_auto = server_is_auto_discovered(&settings);

    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app::run(&mut terminal, &server, settings, server_auto)
}

fn run_updater() -> Result<(), Box<dyn std::error::Error>> {
    let sibling_updater = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("orsiktop-update")))
        .filter(|path| path.is_file());

    let status = if let Some(path) = sibling_updater {
        Command::new(path).status()
    } else {
        Command::new("orsiktop-update").status()
    };

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("OrsikTop updater exited with status {status}").into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "orsiktop-update was not found. Self-update is available for standalone installations created by the OrsikTop installer. If you installed OrsikTop with Cargo, update it with:\n{CARGO_UPDATE_COMMAND}"
        )
        .into()),
        Err(error) => Err(error.into()),
    }
}

fn run_uninstall(purge: bool) -> Result<(), Box<dyn std::error::Error>> {
    let exe = env::current_exe()
        .map_err(|error| format!("could not determine the running binary: {error}"))?;
    let dir = exe
        .parent()
        .ok_or("could not determine the installation directory")?;

    let cargo_install = is_cargo_install_dir(dir);
    let mut lines = Vec::new();

    if cargo_install {
        lines.push(
            "OrsikTop was installed with Cargo, so the binaries were left in place. Remove it with:"
                .to_string(),
        );
        lines.push("cargo uninstall orsiktop".to_string());
    } else {
        for path in uninstall_files(dir) {
            fs::remove_file(&path)
                .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
            lines.push(format!("removed {}", path.display()));
        }
    }

    if let Some(config_dir) = config::config_dir() {
        if purge {
            match fs::remove_dir_all(&config_dir) {
                Ok(()) => lines.push(format!("removed {}", config_dir.display())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(
                        format!("failed to remove {}: {error}", config_dir.display()).into(),
                    )
                }
            }
        } else if config_dir.is_dir() {
            lines.push(format!(
                "kept config at {} (remove it with: orsiktop uninstall --purge)",
                config_dir.display()
            ));
        }
    }

    if lines.is_empty() {
        lines.push("no OrsikTop binaries found; nothing to remove".to_string());
    }
    println!("{}", lines.join("\n"));
    Ok(())
}

fn is_cargo_install_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|name| name == "bin")
        && dir
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|name| name == ".cargo")
}

fn uninstall_files(dir: &Path) -> Vec<PathBuf> {
    ["orsiktop", "orsiktop-update"]
        .into_iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .collect()
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

pub(crate) fn server_is_auto_discovered(settings: &config::AppConfig) -> bool {
    settings.auto_discovery && discover_local_llama_server().is_some()
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

    #[test]
    fn uninstall_files_only_lists_existing_orsiktop_binaries() {
        let dir =
            std::env::temp_dir().join(format!("orsiktop-uninstall-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let main_bin = dir.join("orsiktop");
        let updater = dir.join("orsiktop-update");
        let unrelated = dir.join("llama-server");
        fs::write(&main_bin, b"binary").unwrap();
        fs::write(&unrelated, b"binary").unwrap();

        assert_eq!(uninstall_files(&dir), vec![main_bin.clone()]);

        fs::write(&updater, b"binary").unwrap();
        assert_eq!(uninstall_files(&dir), vec![main_bin, updater]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cargo_install_dir_is_recognized() {
        let cargo_dir = Path::new("/home/user/.cargo/bin");
        let other_dir = Path::new("/home/user/.local/bin");
        assert!(is_cargo_install_dir(cargo_dir));
        assert!(!is_cargo_install_dir(other_dir));
    }
}
