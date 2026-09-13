use std::{env, fs, io, path::PathBuf};

const SERVER_KEY: &str = "server";
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub server: Option<String>,
    pub gpu_index: u32,
    pub refresh_ms: u64,
    pub process_refresh_ms: u64,
    pub offline_grace_ms: u64,
    pub auto_discovery: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: None,
            gpu_index: 0,
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
        self
    }
}

pub fn load() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    fs::read_to_string(path)
        .ok()
        .map(|text| parse_config(&text))
        .unwrap_or_default()
        .sanitized()
}

pub fn save(config: &AppConfig) -> io::Result<()> {
    let path = config_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME/XDG_CONFIG_HOME is unavailable",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let config = config.clone().sanitized();
    let mut lines = Vec::new();
    if let Some(server) = config.server.as_deref() {
        lines.push(format!("{SERVER_KEY}={server}"));
    }
    lines.push(format!("{GPU_INDEX_KEY}={}", config.gpu_index));
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
    fs::write(path, format!("{}\n", lines.join("\n")))
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

fn parse_config(text: &str) -> AppConfig {
    let mut config = AppConfig::default();
    let mut saw_server = false;
    let mut saw_auto_discovery = false;

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
            GPU_INDEX_KEY => {
                if let Ok(parsed) = value.parse() {
                    config.gpu_index = parsed;
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
             gpu_index=1\n\
             refresh_ms=200\n\
             process_refresh_ms=1500\n\
             offline_grace_ms=4000\n\
             auto_discovery=off\n",
        );
        assert_eq!(config.server.as_deref(), Some("http://10.0.0.7:9090"));
        assert_eq!(config.gpu_index, 1);
        assert_eq!(config.refresh_ms, 200);
        assert_eq!(config.process_refresh_ms, 1500);
        assert_eq!(config.offline_grace_ms, 4000);
        assert!(!config.auto_discovery);
    }

    #[test]
    fn legacy_server_config_keeps_manual_server_semantics() {
        let config = parse_config("server=http://10.0.0.7:9090\n");
        assert_eq!(config.server.as_deref(), Some("http://10.0.0.7:9090"));
        assert!(!config.auto_discovery);
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
}
