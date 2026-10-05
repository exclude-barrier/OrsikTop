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
mod redact;
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
    ///
    /// Not `conflicts_with("gpu")`: clap treats env-provided values as present,
    /// so setting both `ORSIKTOP_GPU` and `ORSIKTOP_GPU_INDEX` would make every
    /// invocation fail. `--gpu` simply takes precedence below.
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

    // Load and override the runtime settings once, before dispatching, so the
    // TUI and the settings-aware subcommands (`diag`) see exactly the same
    // configuration. Previously the one-off CLI overrides were applied only to
    // the TUI, so `orsiktop --server … diag` silently ignored the override.
    let mut settings = config::load();
    if let Some(server) = args
        .server
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        settings.server = Some(server.clone());
        settings.auto_discovery = false;
    }
    if let Some(interval_ms) = args.interval_ms {
        settings.refresh_ms = interval_ms;
    }
    if let Some(gpu) = args.gpu.as_ref() {
        settings.gpu_selector = GpuSelector::parse(gpu);
    } else if let Some(gpu_index) = args.gpu_index {
        settings.gpu_selector = GpuSelector::Index(gpu_index);
    }
    settings = settings.sanitized();

    if let Some(command) = args.command.as_ref() {
        return match command {
            Commands::Update => run_updater(),
            Commands::Uninstall { purge } => run_uninstall(*purge),
            Commands::Diag => diagnostics::run(&settings),
        };
    }

    let resolved = resolve_monitor_target(&settings);

    enable_raw_mode()?;
    let _terminal_guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app::run(
        &mut terminal,
        &resolved.endpoint,
        resolved.identity,
        settings,
        resolved.auto,
    )
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

/// The resolved monitor target: the endpoint to poll, the local process PID
/// behind it (when it was produced by a discovered llama server) and whether
/// auto-discovery produced it.
///
/// The three fields change independently — a stable endpoint can still get a
/// new PID (server restart on the same port), and the origin can flip while
/// the endpoint stays the same (auto ↔ manual). Callers must therefore
/// compare them separately instead of keying everything off the URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedServer {
    pub endpoint: String,
    /// Stable identity (PID + process start time) of the local server behind
    /// `endpoint`; `None` for configured/remote endpoints and when the
    /// identity cannot be read. Never a bare PID, so a reused PID cannot be
    /// mistaken for the earlier server.
    pub identity: Option<discovery_llm::LocalServerIdentity>,
    pub auto: bool,
}

/// Resolve the endpoint to monitor, its local process PID and whether
/// auto-discovery produced it, with a single `/proc` scan.
pub(crate) fn resolve_monitor_target(settings: &config::AppConfig) -> ResolvedServer {
    resolve_monitor_target_with(&system::RealSys, settings)
}

/// Fixture-testable [`resolve_monitor_target`]: every filesystem read goes
/// through `sys`.
///
/// Selection follows the documented order: a discovered local server wins
/// (lowest port, never by PID), else the configured endpoint, else the
/// default. [`ResolvedServer::auto`] is true only when auto-discovery is on
/// *and* a local server process is currently running; the PID is `None` for
/// configured/remote endpoints, so a manual URL never gains a local GPU
/// attribution by guessing.
pub(crate) fn resolve_monitor_target_with<S: system::Sys>(
    sys: &S,
    settings: &config::AppConfig,
) -> ResolvedServer {
    let candidates =
        discovery_llm::collect_candidates(sys, settings.auto_discovery, settings.server.as_deref());
    let endpoint = discovery_llm::select_endpoint(&candidates, DEFAULT_SERVER);
    let identity = discovery_llm::selected_endpoint_server(&candidates, &endpoint);
    let auto = settings.auto_discovery
        && candidates.iter().any(|candidate| {
            matches!(
                candidate.source,
                discovery_llm::ServerSource::Process { .. }
            )
        });
    ResolvedServer {
        endpoint,
        identity,
        auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    fn cmdline(parts: &[&str]) -> String {
        parts.join("\0")
    }

    /// A fixture with one `llama-server` per `(pid, port)` pair. Each process
    /// gets a distinct start time so the identity is fully populated.
    fn llama_fixture(servers: &[(u32, u16)]) -> FixtureSys {
        let mut sys = FixtureSys::default();
        for (pid, port) in servers {
            sys.dir_entry("/proc", &pid.to_string(), false, false);
            sys.file(
                &format!("/proc/{pid}/cmdline"),
                cmdline(&["/usr/bin/llama-server", "--port", &port.to_string()]),
            );
            let start_time = u64::from(*pid) * 1_000;
            sys.file(
                &format!("/proc/{pid}/stat"),
                format!(
                    "{pid} (llama-server) S 1 {pid} {pid} 0 -1 4194560 100 0 0 0 5 5 5 0 20 0 \
                     1 0 {start_time} 12345 678 90"
                ),
            );
        }
        sys
    }

    fn identity(pid: u32, start_time: u64) -> discovery_llm::LocalServerIdentity {
        discovery_llm::LocalServerIdentity { pid, start_time }
    }

    #[test]
    fn manual_server_is_used_when_auto_discovery_is_disabled() {
        let settings = config::AppConfig {
            server: Some("http://10.0.0.5:8080".to_string()),
            auto_discovery: false,
            ..config::AppConfig::default()
        };
        // Even with a local server running on the same URL, a manual endpoint
        // gets no local PID (no GPU attribution by guessing).
        let sys = llama_fixture(&[(5000, 8080)]);
        let resolved = resolve_monitor_target_with(&sys, &settings);
        assert_eq!(resolved.endpoint, "http://10.0.0.5:8080");
        assert_eq!(resolved.identity, None);
        assert!(!resolved.auto);
    }

    #[test]
    fn auto_discovery_reports_endpoint_pid_and_origin_independently() {
        let settings = config::AppConfig {
            auto_discovery: true,
            ..config::AppConfig::default()
        };

        // A restart on the same port keeps the endpoint but changes the
        // identity (new PID here; a reused PID would carry a new start time).
        let first = resolve_monitor_target_with(&llama_fixture(&[(5000, 8081)]), &settings);
        assert_eq!(first.endpoint, "http://127.0.0.1:8081");
        assert_eq!(first.identity, Some(identity(5000, 5_000_000)));
        assert!(first.auto);

        let restarted = resolve_monitor_target_with(&llama_fixture(&[(6000, 8081)]), &settings);
        assert_eq!(restarted.endpoint, first.endpoint);
        assert_eq!(restarted.identity, Some(identity(6000, 6_000_000)));
        assert!(restarted.auto);

        // The server disappearing clears the identity and the auto origin,
        // falling back to the configured server (or the default).
        let gone = resolve_monitor_target_with(&llama_fixture(&[]), &settings);
        assert_eq!(gone.endpoint, DEFAULT_SERVER);
        assert_eq!(gone.identity, None);
        assert!(!gone.auto);

        // Enabling auto with the same URL the manual config used picks up the
        // running process: same endpoint, PID appears, origin flips.
        let manual = config::AppConfig {
            server: Some("http://127.0.0.1:8081".to_string()),
            auto_discovery: false,
            ..config::AppConfig::default()
        };
        let manual_target = resolve_monitor_target_with(&llama_fixture(&[(5000, 8081)]), &manual);
        assert_eq!(manual_target.endpoint, "http://127.0.0.1:8081");
        assert_eq!(manual_target.identity, None);
        assert!(!manual_target.auto);

        let auto_target = resolve_monitor_target_with(&llama_fixture(&[(5000, 8081)]), &settings);
        assert_eq!(auto_target.endpoint, manual_target.endpoint);
        assert_eq!(auto_target.identity, Some(identity(5000, 5_000_000)));
        assert!(auto_target.auto);
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
