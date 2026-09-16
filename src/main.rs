mod app;
mod config;
mod cpu;
mod cpu_sensors;
mod diagnostics;
#[allow(dead_code)] // consumed by the GPU provider layer (S5+)
mod discovery;
mod discovery_llm;
mod domain;
mod drm;
mod gpu;
mod gpu_map;
mod llama;
mod providers;
mod system;
mod ui;

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

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
/// Maximum wall time for the `orsiktop-update` helper. It downloads and
/// replaces binaries, so a slow network can legitimately take minutes; the
/// bound only catches a hung process.
const UPDATER_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Update a standalone OrsikTop installation to the latest release.
    Update,
    /// Print a diagnostics summary for bug reports (no secrets).
    Diag,
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
            Commands::Diag => diagnostics::run(),
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
    let server_pid = resolve_server_pid(&settings);
    let server_auto = server_is_auto_discovered(&settings);

    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app::run(&mut terminal, &server, server_pid, settings, server_auto)
}

fn run_updater() -> Result<(), Box<dyn std::error::Error>> {
    let sibling_updater = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("orsiktop-update")))
        .filter(|path| path.is_file());

    let result = if let Some(path) = sibling_updater {
        run_updater_command(Command::new(path))
    } else {
        run_updater_command(Command::new("orsiktop-update"))
    };

    match result {
        Ok(()) => Ok(()),
        Err(UpdaterError::NotFound) => Err(format!(
            "orsiktop-update was not found. Self-update is available for standalone installations created by the OrsikTop installer. If you installed OrsikTop with Cargo, update it with:\n{CARGO_UPDATE_COMMAND}"
        )
        .into()),
        Err(UpdaterError::TimedOut) => Err(format!(
            "orsiktop-update did not finish within {} s and was terminated.",
            UPDATER_TIMEOUT_MS / 1_000
        )
        .into()),
        Err(UpdaterError::Failed(message)) => Err(message.into()),
    }
}

fn run_updater_command(mut command: Command) -> Result<(), UpdaterError> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            UpdaterError::NotFound
        } else {
            UpdaterError::Failed(error.to_string())
        }
    })?;
    wait_for_child(
        &mut child,
        Duration::from_millis(UPDATER_TIMEOUT_MS),
        Duration::from_millis(100),
    )
}

enum UpdaterError {
    NotFound,
    TimedOut,
    Failed(String),
}

/// Waits for `child` to exit, polling with `try_wait` against a `timeout`
/// deadline and sleeping `poll` between polls. On deadline expiry the child
/// is killed (and reaped) and `TimedOut` is returned, so a hung updater can
/// never hang the caller. `poll` is a parameter (not a constant) so tests can
/// drive the deadline with a short timeout and a short poll interval.
fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    poll: Duration,
) -> Result<(), UpdaterError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(UpdaterError::Failed(format!(
                        "OrsikTop updater exited with status {status}"
                    )))
                };
            }
            Ok(None) => {}
            Err(error) => return Err(UpdaterError::Failed(error.to_string())),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdaterError::TimedOut);
        }
        thread::sleep(poll);
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

/// Resolve the endpoint to monitor *and* the local process PID behind it,
/// scanning `/proc` once.
pub(crate) fn resolve_server_full(settings: &config::AppConfig) -> (String, Option<u32>) {
    let candidates = discovery_llm::collect_candidates(
        &system::RealSys,
        settings.auto_discovery,
        settings.server.as_deref(),
    );
    let endpoint = discovery_llm::select_endpoint(&candidates, DEFAULT_SERVER);
    let pid = discovery_llm::selected_endpoint_pid(&candidates, &endpoint);
    (endpoint, pid)
}

fn resolve_server(settings: &config::AppConfig) -> String {
    resolve_server_full(settings).0
}

/// The local process PID behind the resolved endpoint, when it was produced
/// by a discovered `llama-server` / `llama serve` process (used to map the
/// server to a GPU, S16). `None` for configured/remote endpoints.
fn resolve_server_pid(settings: &config::AppConfig) -> Option<u32> {
    resolve_server_full(settings).1
}

/// True when auto-discovery is enabled and at least one local llama server
/// process is running. Recomputed after settings edits in `app::run`.
pub(crate) fn server_is_auto_discovered(settings: &config::AppConfig) -> bool {
    settings.auto_discovery && !discovery_llm::discover_processes(&system::RealSys).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn updater_wait_times_out_a_hung_process() {
        // A process that outlives the deadline must be killed and reported
        // as TimedOut, not hang the caller. 30 ms is far longer than the
        // 10 ms poll, so the loop has time to observe the deadline.
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let result = wait_for_child(
            &mut child,
            Duration::from_millis(30),
            Duration::from_millis(10),
        );
        assert!(matches!(result, Err(UpdaterError::TimedOut)));
        // The child must be reaped (wait() returns Some after kill), proving
        // it was actually killed and not just abandoned.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn updater_wait_returns_failure_for_nonzero_exit() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 3")
            .spawn()
            .expect("spawn sh");
        let result = wait_for_child(
            &mut child,
            Duration::from_millis(5_000),
            Duration::from_millis(10),
        );
        assert!(matches!(result, Err(UpdaterError::Failed(message)) if message.contains("3")));
    }

    #[test]
    fn updater_wait_missing_binary_is_not_found() {
        let result = run_updater_command(Command::new("definitely-not-a-real-updater-binary"));
        assert!(matches!(result, Err(UpdaterError::NotFound)));
    }
}
