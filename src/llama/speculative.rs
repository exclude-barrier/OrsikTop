//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use super::*;

pub(super) fn speculative_config(value: &Value) -> SpeculativeConfig {
    let params = value.get("params");
    let enabled_value = value.get("speculative");
    let n_max_value = params
        .and_then(|params| params.get("speculative.n_max"))
        .or_else(|| value.get("speculative.n_max"));
    let types_value = params
        .and_then(|params| params.get("speculative.types"))
        .or_else(|| value.get("speculative.types"));
    let enabled = enabled_value.and_then(Value::as_bool);
    let n_max = n_max_value.and_then(Value::as_u64);
    let types = types_value.and_then(Value::as_str).unwrap_or_default();
    let named_type = types.split(',').any(|kind| {
        let kind = kind.trim();
        !kind.is_empty() && kind != "none"
    });
    SpeculativeConfig {
        present: enabled_value.is_some() || n_max_value.is_some() || types_value.is_some(),
        enabled,
        is_mtp: types.split(',').any(|kind| kind.trim() == "draft-mtp"),
        n_max,
        types_known: types_value.is_some(),
        named_type,
    }
}

/// Fill the speculative fields the server did not decide from the local
/// server process CLI.
///
/// Precedence (unambiguous):
/// * a server `speculative` boolean is authoritative for enabled/disabled;
/// * a server type list decides the type when it names a real type
///   (`draft-mtp` → MTP; any other named type → not MTP);
/// * a server `speculative.n_max` (including an explicit `0`) is authoritative;
/// * anything the server did not state is filled from the CLI, so a server
///   whose only type report is `speculative.types: "none"` still shows the MTP
///   the process was actually launched with;
/// * a server that explicitly says `speculative: false` is never overridden
///   to MTP by the CLI.
pub(super) fn apply_local_spec_fallback(
    props: &CachedProps,
    stats: &mut LlmStats,
    local: LocalSpeculativeConfig,
) {
    // The server disabled speculation either explicitly (`speculative: false`)
    // or by reporting `speculative.n_max: 0` (known, but filtered to `None`).
    let n_max_zero = props.spec_n_max_known && props.spec_n_max.is_none();
    let server_disabled = (props.spec_enabled_known && !props.spec_enabled) || n_max_zero;
    if !props.spec_enabled_known && !server_disabled {
        stats.spec_enabled |= local.enabled;
    }
    if !props.spec_named_type && !server_disabled {
        stats.spec_is_mtp |= local.is_mtp;
    }
    if !props.spec_n_max_known && !server_disabled && stats.spec_n_max.is_none() {
        stats.spec_n_max = local.n_max;
    }
}

/// True when `host` (as returned by [`reqwest::Url::host_str`]) names the
/// local machine. IPv6 hosts come back bracketed (`[::1]`), so brackets are
/// stripped before the address is parsed; a hostname is only `localhost`.
/// This keeps the local SPEC-CLI fallback available for IPv6 loopback
/// endpoints exactly as for IPv4.
pub(super) fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

pub(super) fn local_speculative_process_config(base: &str) -> Option<LocalSpeculativeConfig> {
    let url = reqwest::Url::parse(base).ok()?;
    let host = url.host_str()?;
    if !is_loopback_host(host) {
        return None;
    }
    let target_port = url.port_or_known_default()?;

    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        if !pid.to_string_lossy().chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if args.is_empty() || !is_llama_server_process(&args) {
            continue;
        }
        if !command_targets_port(&args, target_port) {
            continue;
        }
        let env = fs::read(entry.path().join("environ")).ok();
        return Some(speculative_from_process(&args, env.as_deref()));
    }
    None
}

pub(super) fn is_llama_server_process(args: &[String]) -> bool {
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

pub(super) fn command_targets_port(args: &[String], target_port: u16) -> bool {
    let mut saw_port = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--port" {
            saw_port = true;
            if args.get(index + 1).and_then(|v| v.parse::<u16>().ok()) == Some(target_port) {
                return true;
            }
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--port=") {
            saw_port = true;
            if value.parse::<u16>().ok() == Some(target_port) {
                return true;
            }
        }
        index += 1;
    }
    !saw_port && target_port == 8080
}

pub(super) fn speculative_from_process(
    args: &[String],
    env_bytes: Option<&[u8]>,
) -> LocalSpeculativeConfig {
    let spec_type = cli_value(args, "--spec-type").unwrap_or_default();
    let is_mtp = spec_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    let n_max = cli_value(args, "--spec-draft-n-max")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            env_value(env_bytes, "LLAMA_ARG_SPEC_DRAFT_N_MAX").and_then(|value| value.parse().ok())
        });
    let env_type = env_value(env_bytes, "LLAMA_ARG_SPEC_TYPE").unwrap_or_default();
    let env_mtp = env_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    LocalSpeculativeConfig {
        enabled: is_mtp || env_mtp || n_max.is_some(),
        is_mtp: is_mtp || env_mtp,
        n_max,
    }
}

pub(super) fn cli_value(args: &[String], flag: &str) -> Option<String> {
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

pub(super) fn env_value(env_bytes: Option<&[u8]>, key: &str) -> Option<String> {
    env_bytes?
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}
