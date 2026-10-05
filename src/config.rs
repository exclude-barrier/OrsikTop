use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::domain::GpuSelector;

const SERVER_KEY: &str = "server";
const GPU_KEY: &str = "gpu";
const GPU_INDEX_KEY: &str = "gpu_index";
const REFRESH_MS_KEY: &str = "refresh_ms";
const PROCESS_REFRESH_MS_KEY: &str = "process_refresh_ms";
const OFFLINE_GRACE_MS_KEY: &str = "offline_grace_ms";
const AUTO_DISCOVERY_KEY: &str = "auto_discovery";

pub const DEFAULT_REFRESH_MS: u64 = 1_000;
pub const DEFAULT_PROCESS_REFRESH_MS: u64 = 1_000;
pub const DEFAULT_OFFLINE_GRACE_MS: u64 = 2_500;
pub const MIN_PROCESS_REFRESH_MS: u64 = 100;
pub const MAX_PROCESS_REFRESH_MS: u64 = 60_000;
pub const MAX_OFFLINE_GRACE_MS: u64 = 60_000;

/// Monotonic counter for temporary config filenames, paired with the process
/// id so concurrent saves in one process cannot pick the same temporary path.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub server: Option<String>,
    pub gpu_selector: GpuSelector,
    pub refresh_ms: u64,
    pub process_refresh_ms: u64,
    pub offline_grace_ms: u64,
    pub auto_discovery: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: None,
            gpu_selector: GpuSelector::Auto,
            refresh_ms: DEFAULT_REFRESH_MS,
            process_refresh_ms: DEFAULT_PROCESS_REFRESH_MS,
            offline_grace_ms: DEFAULT_OFFLINE_GRACE_MS,
            auto_discovery: true,
        }
    }
}

impl AppConfig {
    pub fn sanitized(mut self) -> Self {
        self.server = self
            .server
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.refresh_ms = self.refresh_ms.clamp(100, 10_000);
        self.process_refresh_ms = self
            .process_refresh_ms
            .clamp(MIN_PROCESS_REFRESH_MS, MAX_PROCESS_REFRESH_MS);
        self.offline_grace_ms = self.offline_grace_ms.min(MAX_OFFLINE_GRACE_MS);
        self.gpu_selector = self.gpu_selector.sanitized();
        self
    }
}

/// A configuration load failure that is not simply "no config yet".
#[derive(Debug)]
pub(crate) enum ConfigLoadError {
    /// No configuration file exists yet; callers keep the built-in defaults.
    Missing,
    /// The file exists but could not be read (a directory in its place,
    /// permissions, invalid UTF-8, ...). The message is secrets-free: it names
    /// the path and the OS error, never the file's contents.
    Unreadable(String),
}

pub fn load() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    let (config, warning) = load_or_default(&path);
    if let Some(warning) = warning {
        eprintln!("orsiktop: {warning}; using defaults");
    }
    config
}

/// Load `path`, falling back to the defaults. A missing file is expected on a
/// first run and stays silent; any other read error yields a secrets-free
/// warning for the caller.
fn load_or_default(path: &Path) -> (AppConfig, Option<String>) {
    match load_from_path(path) {
        Ok(config) => (config, None),
        Err(ConfigLoadError::Missing) => (AppConfig::default().sanitized(), None),
        Err(ConfigLoadError::Unreadable(message)) => {
            (AppConfig::default().sanitized(), Some(message))
        }
    }
}

/// Read and parse one configuration file. `Missing` is kept distinct from any
/// other read error so the caller can stay silent on a first run yet report a
/// real problem (a directory, permissions, corrupt bytes) understandably.
pub(crate) fn load_from_path(path: &Path) -> Result<AppConfig, ConfigLoadError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse_config(&text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(ConfigLoadError::Missing),
        Err(err) => Err(ConfigLoadError::Unreadable(format!(
            "could not read saved config at {}: {err}",
            path.display()
        ))),
    }
}

pub fn save(config: &AppConfig) -> io::Result<()> {
    let path = config_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME/XDG_CONFIG_HOME is unavailable",
        )
    })?;
    save_to_path(&path, config)
}

/// Serialize `config` and write it atomically to `path`.
///
/// The content is written to a temporary file in the same directory and then
/// renamed over `path`, so a concurrent reader sees either the complete old or
/// the complete new file. Any failure removes the temporary file and leaves an
/// existing `path` untouched.
pub(crate) fn save_to_path(path: &Path, config: &AppConfig) -> io::Result<()> {
    // Follow a symlinked config to its real target (as the previous in-place
    // write did) so a dotfiles-managed target keeps being updated instead of
    // the symlink being replaced by a regular file.
    let target = resolve_save_target(path)?;
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let contents = render_config(config);
    // `create_new` keeps the temporary private and refuses to touch anything
    // that already exists; a rare name collision (a stale temp from a reused
    // pid) just retries with a fresh suffix.
    for _ in 0..8 {
        let tmp = atomic_temp_path(&target);
        match replace_file_atomically(&target, &tmp, &contents) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return result,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary config file",
    ))
}

/// The file an atomic save should actually replace, following symlinks. A
/// missing path is returned as-is so a first save can create it. A chain
/// longer than the hop limit (or a loop) is handed to the kernel via
/// `canonicalize`, which fails on a loop instead of clobbering a link.
fn resolve_save_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let target = fs::read_link(&current)?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }
    fs::canonicalize(&current)
}

/// The temporary sibling used for an atomic replace of `path`. The process id
/// and a monotonic counter keep concurrent saves from colliding.
fn atomic_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let unique = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.tmp.{}.{}", std::process::id(), unique))
}

/// Write `contents` to `tmp` and rename it over `path`. `tmp` is removed on
/// every failure path, and `path` is left untouched when the write or the
/// rename fails, so an existing configuration survives a failed save.
fn replace_file_atomically(path: &Path, tmp: &Path, contents: &str) -> io::Result<()> {
    let result = (|| {
        // Create the temp private, write, then flush to disk *before* the
        // rename so a crash cannot leave a renamed-but-empty config behind.
        let mut file = create_private_temp(tmp)?;
        file.write_all(contents.as_bytes())?;
        // Only now widen to the target's existing mode (or keep 0600 for a
        // new file), so the content is never briefly world-readable.
        apply_target_permissions(path, tmp)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, path)?;
        // Persist the directory-entry change too where the platform allows.
        sync_parent_dir(path);
        Ok(())
    })();
    if result.is_err() {
        // Clean up on every failure path: the write, the permissions step, the
        // sync, or the rename. A successful rename already consumed `tmp`.
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Create the temporary file private from the start, so its contents are never
/// briefly world-readable while being written on a multi-user host. `create_new`
/// additionally refuses to follow or overwrite an existing path.
fn create_private_temp(tmp: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(tmp)
}

/// Best-effort `fsync` of the directory containing `path`, so the rename is
/// durable. Failures are ignored: some platforms and filesystems cannot fsync
/// a directory, and the rename is still atomic within the running system.
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// Give `tmp` the permissions of an existing `path`, or `0o600` for a new
/// config file.
///
/// `fs::write` creates the temporary file with the process umask, so without
/// this step a deliberately private config (for example one holding a
/// credential-bearing endpoint) would be replaced by a more permissive file.
fn apply_target_permissions(path: &Path, tmp: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = match fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode() & 0o7777,
            Err(_) => 0o600,
        };
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, tmp);
    }
    Ok(())
}

/// The exact on-disk representation of `config` (sanitized), kept pure so
/// tests can exercise serialization directly.
fn render_config(config: &AppConfig) -> String {
    let config = config.clone().sanitized();
    let mut lines = Vec::new();
    if let Some(server) = config.server.as_deref() {
        lines.push(format!("{SERVER_KEY}={server}"));
    }
    let selector = config.gpu_selector.as_string();
    if !selector.is_empty() {
        lines.push(format!("{GPU_KEY}={selector}"));
    }
    lines.push(format!("{REFRESH_MS_KEY}={}", config.refresh_ms));
    lines.push(format!(
        "{PROCESS_REFRESH_MS_KEY}={}",
        config.process_refresh_ms
    ));
    lines.push(format!(
        "{OFFLINE_GRACE_MS_KEY}={}",
        config.offline_grace_ms
    ));
    lines.push(format!(
        "{AUTO_DISCOVERY_KEY}={}",
        if config.auto_discovery {
            "true"
        } else {
            "false"
        }
    ));
    format!("{}\n", lines.join("\n"))
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("orsiktop"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/orsiktop"))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config"))
}

fn parse_config(text: &str) -> AppConfig {
    let mut config = AppConfig::default();
    let mut saw_server = false;
    let mut saw_auto_discovery = false;
    let mut saw_gpu_key = false;
    let mut legacy_gpu_index: Option<u32> = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            SERVER_KEY if !value.is_empty() => {
                config.server = Some(value.to_string());
                saw_server = true;
            }
            GPU_KEY => {
                config.gpu_selector = GpuSelector::parse(value);
                saw_gpu_key = true;
            }
            GPU_INDEX_KEY if !saw_gpu_key => {
                if let Ok(parsed) = value.parse() {
                    legacy_gpu_index = Some(parsed);
                }
            }
            REFRESH_MS_KEY => {
                if let Ok(parsed) = value.parse() {
                    config.refresh_ms = parsed;
                }
            }
            PROCESS_REFRESH_MS_KEY => {
                if let Ok(parsed) = value.parse() {
                    config.process_refresh_ms = parsed;
                }
            }
            OFFLINE_GRACE_MS_KEY => {
                if let Ok(parsed) = value.parse() {
                    config.offline_grace_ms = parsed;
                }
            }
            AUTO_DISCOVERY_KEY => {
                if let Some(parsed) = parse_bool(value) {
                    config.auto_discovery = parsed;
                    saw_auto_discovery = true;
                }
            }
            _ => {}
        }
    }

    // The old config only stored `server=...` and that value was authoritative.
    // Preserve that behavior when reading a legacy config for the first time.
    if saw_server && !saw_auto_discovery {
        config.auto_discovery = false;
    }
    // Legacy `gpu_index` (a bare NVML ordinal) migrates to an Index selector
    // only when the modern `gpu` key is absent, so an explicit stable
    // selector always wins.
    if !saw_gpu_key {
        if let Some(index) = legacy_gpu_index {
            config.gpu_selector = GpuSelector::Index(index);
        }
    }
    config.sanitized()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_config() {
        let config = parse_config(
            "server=http://10.0.0.7:9090\n\
             gpu=GPU-1a2b3c\n\
             refresh_ms=200\n\
             process_refresh_ms=1500\n\
             offline_grace_ms=4000\n\
             auto_discovery=off\n",
        );
        assert_eq!(config.server.as_deref(), Some("http://10.0.0.7:9090"));
        assert_eq!(
            config.gpu_selector,
            GpuSelector::Uuid("GPU-1a2b3c".to_string())
        );
        assert_eq!(config.refresh_ms, 200);
        assert_eq!(config.process_refresh_ms, 1500);
        assert_eq!(config.offline_grace_ms, 4000);
        assert!(!config.auto_discovery);
    }

    #[test]
    fn legacy_gpu_index_migrates_to_index_selector() {
        let config = parse_config("gpu_index=2\n");
        assert_eq!(config.gpu_selector, GpuSelector::Index(2));
    }

    #[test]
    fn modern_gpu_key_wins_over_legacy_gpu_index() {
        // When both keys are present the stable selector is authoritative,
        // regardless of line order.
        let first = parse_config("gpu_index=1\ngpu=0000:41:00.0\n");
        assert_eq!(
            first.gpu_selector,
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
        let second = parse_config("gpu=0000:41:00.0\ngpu_index=1\n");
        assert_eq!(
            second.gpu_selector,
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
    }

    #[test]
    fn legacy_server_config_keeps_manual_server_semantics() {
        let config = parse_config("server=http://10.0.0.7:9090\n");
        assert_eq!(config.server.as_deref(), Some("http://10.0.0.7:9090"));
        assert!(!config.auto_discovery);
    }

    #[test]
    fn gpu_selector_round_trips_through_save_and_parse() {
        // parse → as_string → parse must be stable for every selector variant.
        for raw in ["", "3", "GPU-1a2b3c", "0000:41:00.0"] {
            let first = GpuSelector::parse(raw);
            let second = GpuSelector::parse(first.as_string().as_str());
            assert_eq!(first, second, "round trip changed selector for {raw}");
        }
        assert_eq!(GpuSelector::parse("").as_string(), "");
        assert_eq!(GpuSelector::Index(5).as_string(), "5");
        assert_eq!(
            GpuSelector::PciBusId("0000:41:00.0".to_string()).as_string(),
            "0000:41:00.0"
        );
    }

    #[test]
    fn gpu_selector_sanitized_trims_blank_to_auto() {
        assert_eq!(
            GpuSelector::Uuid("   ".to_string()).sanitized(),
            GpuSelector::Auto
        );
        assert_eq!(
            GpuSelector::PciBusId(" 0000:41:00.0 ".to_string()).sanitized(),
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
        assert_eq!(GpuSelector::Index(2).sanitized(), GpuSelector::Index(2));
    }

    #[test]
    fn sanitizes_runtime_intervals() {
        let config = AppConfig {
            refresh_ms: 1,
            process_refresh_ms: 99_999,
            offline_grace_ms: 99_999,
            ..AppConfig::default()
        }
        .sanitized();
        assert_eq!(config.refresh_ms, 100);
        assert_eq!(config.process_refresh_ms, MAX_PROCESS_REFRESH_MS);
        assert_eq!(config.offline_grace_ms, MAX_OFFLINE_GRACE_MS);
    }

    /// Fresh, empty temp directory unique to this test and process.
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orsiktop-config-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_file_names(dir: &std::path::Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect()
    }

    #[test]
    fn save_atomically_replaces_config_and_leaves_no_temp_files() {
        let dir = unique_temp_dir("atomic-replace");
        let path = dir.join("config");
        fs::write(&path, "server=http://old:1\n").unwrap();

        let config = AppConfig {
            server: Some("http://new:9999".to_string()),
            refresh_ms: 250,
            ..AppConfig::default()
        };
        save_to_path(&path, &config).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("server=http://new:9999"), "got: {text}");
        assert!(text.contains("refresh_ms=250"), "got: {text}");
        assert!(text.contains("auto_discovery=true"), "got: {text}");
        assert!(
            !text.contains("http://old:1"),
            "the old value must be fully replaced, got: {text}"
        );
        assert!(
            temp_file_names(&dir).is_empty(),
            "temporary files must be cleaned up"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_write_leaves_the_existing_config_untouched() {
        let dir = unique_temp_dir("write-failure");
        let path = dir.join("config");
        fs::write(&path, "server=http://keep:1\n").unwrap();

        // A directory at the temporary path makes the write fail
        // deterministically, without depending on filesystem permissions.
        let blocked_tmp = dir.join("blocked");
        fs::create_dir(&blocked_tmp).unwrap();

        replace_file_atomically(&path, &blocked_tmp, "server=http://new:2\n")
            .expect_err("writing onto a directory must fail");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "server=http://keep:1\n",
            "the previous config must survive a failed write"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_replace_removes_the_temporary_file() {
        let dir = unique_temp_dir("replace-failure");
        // The target is a directory, so renaming the temp file over it fails.
        let target = dir.join("config-dir");
        fs::create_dir(&target).unwrap();
        let tmp = dir.join("config.tmp");

        replace_file_atomically(&target, &tmp, "data\n")
            .expect_err("replacing a directory must fail");
        assert!(
            !tmp.exists(),
            "the temporary file must be removed when the replace fails"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_temporary_file_is_created_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("private-temp");
        let tmp = dir.join(".config.tmp.test");
        let _file = create_private_temp(&tmp).unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o600,
            "the temporary file must be private before content is written"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_follows_a_long_symlink_chain() {
        let dir = unique_temp_dir("long-symlink");
        let target = dir.join("final");
        fs::write(&target, "server=http://old:1\n").unwrap();

        // l0 -> l1 -> ... -> l9 -> final, i.e. a chain longer than any small
        // hop guard must still resolve to the real target.
        let mut previous = target.clone();
        for index in (0..10).rev() {
            let link = dir.join(format!("l{index}"));
            std::os::unix::fs::symlink(&previous, &link).unwrap();
            previous = link;
        }

        let config = AppConfig {
            server: Some("http://new:2".to_string()),
            ..AppConfig::default()
        };
        save_to_path(&previous, &config).unwrap();

        assert!(
            fs::symlink_metadata(&previous)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the first link must survive"
        );
        let text = fs::read_to_string(&target).unwrap();
        assert!(
            text.contains("server=http://new:2"),
            "the final target must be updated, got: {text}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_rejects_a_symlink_loop_without_clobbering() {
        let dir = unique_temp_dir("symlink-loop");
        let a = dir.join("a");
        let b = dir.join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();

        save_to_path(&a, &AppConfig::default())
            .expect_err("a symlink loop must not be written through");

        // Both links survive untouched instead of one being replaced.
        assert!(fs::symlink_metadata(&a).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(&b).unwrap().file_type().is_symlink());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temp_paths_are_unique_and_carry_the_process_id() {
        let path = Path::new("/tmp/orsiktop-config");
        let first = atomic_temp_path(path);
        let second = atomic_temp_path(path);
        assert_ne!(first, second, "each save must use a fresh temp path");
        assert_eq!(first.parent(), path.parent());
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(".orsiktop-config.tmp."),
            "unexpected temp name: {name}"
        );
        assert!(
            name.contains(&std::process::id().to_string()),
            "the process id must be part of the temp name: {name}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_existing_permissions_and_restricts_new_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("permissions");
        let path = dir.join("config");
        fs::write(&path, "server=http://old:1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        save_to_path(&path, &AppConfig::default()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "an existing private config must stay private");

        // A brand-new config gets the restrictive default, not the umask one.
        let fresh = dir.join("config-new");
        save_to_path(&fresh, &AppConfig::default()).unwrap();
        let mode = fs::metadata(&fresh).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "a new config must not be world-readable");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_follows_a_symlinked_config_instead_of_replacing_the_link() {
        let dir = unique_temp_dir("symlink");
        let real_dir = dir.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let target = real_dir.join("config");
        fs::write(&target, "server=http://old:1\n").unwrap();
        let link = dir.join("config");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let config = AppConfig {
            server: Some("http://new:2".to_string()),
            ..AppConfig::default()
        };
        save_to_path(&link, &config).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the config symlink must survive the save"
        );
        let text = fs::read_to_string(&target).unwrap();
        assert!(
            text.contains("server=http://new:2"),
            "the real target must be updated, got: {text}"
        );
        assert!(
            temp_file_names(&dir).is_empty(),
            "no temp file may be left next to the symlink"
        );
        assert!(
            temp_file_names(&real_dir).is_empty(),
            "no temp file may be left next to the real target"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_saves_leave_one_complete_config_and_no_temp_files() {
        let dir = unique_temp_dir("concurrent");
        let path = dir.join("config");
        fs::write(&path, "server=http://init:1\n").unwrap();

        std::thread::scope(|scope| {
            for index in 0..16u32 {
                let path = path.clone();
                scope.spawn(move || {
                    let config = AppConfig {
                        server: Some(format!("http://host:{index}")),
                        refresh_ms: 100 + index as u64,
                        ..AppConfig::default()
                    };
                    save_to_path(&path, &config).unwrap();
                });
            }
        });

        // Last writer wins, but the file must be one complete, parseable
        // config and no temporary file may survive the race.
        let text = fs::read_to_string(&path).unwrap();
        assert!(parse_config(&text).server.is_some(), "got: {text}");
        assert!(text.contains("refresh_ms="), "got: {text}");
        assert!(
            temp_file_names(&dir).is_empty(),
            "concurrent saves must not leave temporary files"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_config_is_silent_and_uses_defaults() {
        let dir = unique_temp_dir("missing");
        let path = dir.join("config");
        assert!(!path.exists());

        let (config, warning) = load_or_default(&path);
        assert_eq!(config, AppConfig::default().sanitized());
        assert!(
            warning.is_none(),
            "a missing config is expected on a first run, not reported"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_config_is_reported_secrets_free_and_uses_defaults() {
        let dir = unique_temp_dir("unreadable");
        // A directory where a file is expected: read_to_string fails
        // deterministically without relying on permissions.
        let path = dir.join("config");
        fs::create_dir(&path).unwrap();

        let (config, warning) = load_or_default(&path);
        assert_eq!(config, AppConfig::default().sanitized());
        let warning = warning.expect("an unreadable config must be reported");
        assert!(
            warning.contains("could not read saved config"),
            "the message must explain the problem, got: {warning}"
        );
        assert!(
            !warning.contains("server="),
            "the message must not leak config contents, got: {warning}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_utf8_config_is_reported_as_unreadable() {
        let dir = unique_temp_dir("bad-utf8");
        let path = dir.join("config");
        fs::write(&path, [0x66u8, 0x6f, 0x80, 0x6f]).unwrap();

        let (config, warning) = load_or_default(&path);
        assert_eq!(config, AppConfig::default().sanitized());
        assert!(warning.is_some(), "corrupt bytes must be reported");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_path_keeps_migrations_and_reports_valid_config() {
        let dir = unique_temp_dir("migrations");
        let path = dir.join("config");
        fs::write(&path, "server=http://10.0.0.7:9090\ngpu_index=2\n").unwrap();

        let config = load_from_path(&path).expect("valid config");
        assert_eq!(config.server.as_deref(), Some("http://10.0.0.7:9090"));
        // Legacy `server=` implies manual mode; `gpu_index` migrates to Index.
        assert!(!config.auto_discovery);
        assert_eq!(config.gpu_selector, GpuSelector::Index(2));

        fs::remove_dir_all(&dir).unwrap();
    }
}
