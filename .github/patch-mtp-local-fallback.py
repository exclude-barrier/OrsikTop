from pathlib import Path


def replace_once(text, old, new, label):
    if old not in text:
        raise SystemExit(f'target not found: {label}')
    return text.replace(old, new, 1)

p = Path('src/llama.rs')
s = p.read_text()

s = replace_once(
    s,
    'use std::{\n    path::Path,\n    time::{Duration, Instant},\n};',
    'use std::{\n    fs,\n    path::Path,\n    time::{Duration, Instant},\n};',
    'std fs import',
)

s = replace_once(
    s,
    '    pub spec_enabled: bool,\n    pub spec_n_max: Option<u64>,\n    pub spec_acceptance_pct: Option<f64>,\n',
    '    pub spec_enabled: bool,\n    pub spec_is_mtp: bool,\n    pub spec_n_max: Option<u64>,\n    pub spec_acceptance_pct: Option<f64>,\n',
    'stats spec fields',
)

s = replace_once(
    s,
    '    spec_enabled: bool,\n    spec_n_max: Option<u64>,\n    last_refresh: Option<Instant>,\n',
    '    spec_enabled: bool,\n    spec_is_mtp: bool,\n    spec_n_max: Option<u64>,\n    last_refresh: Option<Instant>,\n',
    'cached spec fields',
)

s = replace_once(
    s,
    '        stats.spec_enabled = self.props.spec_enabled;\n        stats.spec_n_max = self.props.spec_n_max;\n',
    '        stats.spec_enabled = self.props.spec_enabled;\n        stats.spec_is_mtp = self.props.spec_is_mtp;\n        stats.spec_n_max = self.props.spec_n_max;\n\n        if let Some(local_spec) = local_speculative_process_config(&self.base) {\n            stats.spec_enabled |= local_spec.enabled;\n            stats.spec_is_mtp |= local_spec.is_mtp;\n            if stats.spec_n_max.is_none() {\n                stats.spec_n_max = local_spec.n_max;\n            }\n        }\n',
    'copy process fallback',
)

s = replace_once(
    s,
    '        if let Some(defaults) = props.get("default_generation_settings") {\n            let (enabled, n_max) = speculative_config(defaults);\n            if let Some(enabled) = enabled {\n                self.props.spec_enabled = enabled;\n            }\n            if let Some(n_max) = n_max {\n                self.props.spec_n_max = (n_max > 0).then_some(n_max);\n                if n_max > 0 {\n                    self.props.spec_enabled = true;\n                }\n            }\n        }\n',
    '        if let Some(defaults) = props.get("default_generation_settings") {\n            let spec = speculative_config(defaults);\n            if let Some(enabled) = spec.enabled {\n                self.props.spec_enabled = enabled;\n            }\n            self.props.spec_is_mtp |= spec.is_mtp;\n            if let Some(n_max) = spec.n_max {\n                self.props.spec_n_max = (n_max > 0).then_some(n_max);\n                if n_max > 0 {\n                    self.props.spec_enabled = true;\n                }\n            }\n        }\n        let root_spec = speculative_config(&props);\n        if let Some(enabled) = root_spec.enabled {\n            self.props.spec_enabled |= enabled;\n        }\n        self.props.spec_is_mtp |= root_spec.is_mtp;\n        if self.props.spec_n_max.is_none() {\n            self.props.spec_n_max = root_spec.n_max.filter(|value| *value > 0);\n        }\n',
    'props speculative parsing',
)

s = replace_once(
    s,
    '        let (slot_spec_enabled, slot_spec_n_max) = speculative_config(slot);\n        if slot_spec_enabled.unwrap_or(false) || slot_spec_n_max.is_some_and(|value| value > 0) {\n            stats.spec_enabled = true;\n        }\n        if let Some(n_max) = slot_spec_n_max.filter(|value| *value > 0) {\n            stats.spec_n_max = Some(stats.spec_n_max.map_or(n_max, |current| current.max(n_max)));\n        }\n',
    '        let slot_spec = speculative_config(slot);\n        if slot_spec.enabled.unwrap_or(false) || slot_spec.n_max.is_some_and(|value| value > 0) {\n            stats.spec_enabled = true;\n        }\n        stats.spec_is_mtp |= slot_spec.is_mtp;\n        if let Some(n_max) = slot_spec.n_max.filter(|value| *value > 0) {\n            stats.spec_n_max = Some(stats.spec_n_max.map_or(n_max, |current| current.max(n_max)));\n        }\n',
    'slot speculative parsing',
)

old_helper = '''fn speculative_config(value: &Value) -> (Option<bool>, Option<u64>) {\n    let enabled = value.get("speculative").and_then(Value::as_bool);\n    let n_max = value\n        .get("params")\n        .and_then(|params| params.get("speculative.n_max"))\n        .and_then(Value::as_u64);\n    (enabled, n_max)\n}\n'''
new_helper = r'''#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct SpeculativeConfig {
    enabled: Option<bool>,
    is_mtp: bool,
    n_max: Option<u64>,
}

fn speculative_config(value: &Value) -> SpeculativeConfig {
    let enabled = value.get("speculative").and_then(Value::as_bool);
    let params = value.get("params");
    let n_max = params
        .and_then(|params| params.get("speculative.n_max"))
        .or_else(|| value.get("speculative.n_max"))
        .and_then(Value::as_u64);
    let types = params
        .and_then(|params| params.get("speculative.types"))
        .or_else(|| value.get("speculative.types"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    SpeculativeConfig {
        enabled,
        is_mtp: types.split(',').any(|kind| kind.trim() == "draft-mtp"),
        n_max,
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct LocalSpeculativeConfig {
    enabled: bool,
    is_mtp: bool,
    n_max: Option<u64>,
}

fn local_speculative_process_config(base: &str) -> Option<LocalSpeculativeConfig> {
    let url = reqwest::Url::parse(base).ok()?;
    let host = url.host_str()?;
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
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
        if args.is_empty() || !args.iter().any(|arg| arg.contains("llama-server")) {
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

fn command_targets_port(args: &[String], target_port: u16) -> bool {
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

fn speculative_from_process(args: &[String], env_bytes: Option<&[u8]>) -> LocalSpeculativeConfig {
    let spec_type = cli_value(args, "--spec-type").unwrap_or_default();
    let is_mtp = spec_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    let n_max = cli_value(args, "--spec-draft-n-max")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            env_value(env_bytes, "LLAMA_ARG_SPEC_DRAFT_N_MAX")
                .and_then(|value| value.parse().ok())
        });
    let env_type = env_value(env_bytes, "LLAMA_ARG_SPEC_TYPE").unwrap_or_default();
    let env_mtp = env_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    LocalSpeculativeConfig {
        enabled: is_mtp || env_mtp || n_max.is_some(),
        is_mtp: is_mtp || env_mtp,
        n_max,
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

fn env_value(env_bytes: Option<&[u8]>, key: &str) -> Option<String> {
    env_bytes?
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}
'''
s = replace_once(s, old_helper, new_helper, 'spec helper')

s = s.replace(
    'assert_eq!(speculative_config(&slot), (Some(true), Some(3)));',
    'assert_eq!(speculative_config(&slot), SpeculativeConfig { enabled: Some(true), is_mtp: false, n_max: Some(3) });',
)

if 'mod local_speculative_process_tests {' not in s:
    s += r'''

#[cfg(test)]
mod local_speculative_process_tests {
    use super::*;

    #[test]
    fn reads_draft_mtp_type_from_custom_slot_shape() {
        let value = serde_json::json!({
            "speculative": true,
            "params": {"speculative.types": "none,draft-mtp"}
        });
        let spec = speculative_config(&value);
        assert_eq!(spec.enabled, Some(true));
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, None);
    }

    #[test]
    fn reads_n_max_from_cli() {
        let args = vec![
            "llama-server".to_string(),
            "--port".to_string(),
            "8081".to_string(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--spec-draft-n-max".to_string(),
            "3".to_string(),
        ];
        let spec = speculative_from_process(&args, None);
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
        assert!(command_targets_port(&args, 8081));
    }

    #[test]
    fn reads_n_max_from_environment() {
        let args = vec!["llama-server".to_string(), "--port=8081".to_string()];
        let env = b"LLAMA_ARG_SPEC_TYPE=draft-mtp\0LLAMA_ARG_SPEC_DRAFT_N_MAX=3\0";
        let spec = speculative_from_process(&args, Some(env));
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
    }
}
'''

p.write_text(s)

p = Path('src/ui.rs')
s = p.read_text()
old = '''    let (mtp, mtp_color) = if llm.spec_enabled {\n        (\n            llm.spec_n_max\n                .filter(|value| *value > 0)\n                .map(|value| format!("MTP{value}"))\n                .unwrap_or_else(|| "MTP".to_string()),\n            CYAN,\n        )\n    } else {\n        ("MTP OFF".to_string(), MUTED)\n    };\n'''
new = '''    let (mtp, mtp_color) = if llm.spec_is_mtp {\n        (\n            llm.spec_n_max\n                .filter(|value| *value > 0)\n                .map(|value| format!("MTP{value}"))\n                .unwrap_or_else(|| "MTP".to_string()),\n            CYAN,\n        )\n    } else if llm.spec_enabled {\n        ("SPEC".to_string(), CYAN)\n    } else {\n        ("MTP OFF".to_string(), MUTED)\n    };\n'''
s = replace_once(s, old, new, 'ui mtp label')
p.write_text(s)
