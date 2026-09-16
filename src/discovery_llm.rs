//! llama.cpp server discovery.
//!
//! OrsikTop can auto-discover a running `llama-server` / `llama serve`
//! process by scanning `/proc`. Discovery here keeps *every* candidate
//! endpoint (not just one) and selects a single endpoint to monitor with a
//! stable, documented rule — **never by PID**. The remaining candidates stay
//! available for selection (pinning `server=`) and diagnostics (S18).
//!
//! All filesystem access goes through the [`Sys`] abstraction so the process
//! scan is fixture-testable without a live llama server.

use std::path::Path;

use crate::system::Sys;

/// Where a candidate endpoint came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerSource {
    /// A running `llama-server` / `llama serve` process.
    Process { pid: u32 },
    /// The configured `server` setting or `--server` flag.
    Configured,
}

/// One candidate llama.cpp server endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlamaServerCandidate {
    /// Base URL to poll (scheme + host + port, no path).
    pub endpoint: String,
    /// Where this candidate came from.
    pub source: ServerSource,
    /// For process candidates: the command line that produced the endpoint.
    /// Consumed by diagnostics (S18).
    #[allow(dead_code)]
    pub command: Option<String>,
}

/// Scan `/proc` for running llama.cpp servers and return every distinct
/// endpoint, deterministically ordered (lowest port first, then endpoint
/// string) — never by PID.
pub fn discover_processes<S: Sys>(sys: &S) -> Vec<LlamaServerCandidate> {
    let proc_root = Path::new("/proc");
    let Some(entries) = sys.read_dir(proc_root) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(pid) = entry.name.parse::<u32>() else {
            continue;
        };
        let Some(cmdline) = sys.read_to_string(&proc_root.join(&entry.name).join("cmdline")) else {
            continue;
        };
        let args = parse_cmdline(&cmdline);
        if !is_llama_server_process(&args) {
            continue;
        }
        candidates.push(LlamaServerCandidate {
            endpoint: endpoint_from_args(&args),
            source: ServerSource::Process { pid },
            command: Some(args.join(" ")),
        });
    }

    // Deterministic order independent of PID: lowest port first, then the
    // endpoint string. De-duplicate endpoints that resolve to the same URL
    // (e.g. a proxy in front of the same server).
    candidates.sort_by_key(|c| (parse_port(&c.endpoint), c.endpoint.clone()));
    let mut deduped = Vec::new();
    let mut previous: Option<String> = None;
    for candidate in candidates {
        if previous.as_deref() != Some(candidate.endpoint.as_str()) {
            previous = Some(candidate.endpoint.clone());
            deduped.push(candidate);
        }
    }
    deduped
}

/// All candidate endpoints in display order: discovered servers first (when
/// auto-discovery is on), then the configured endpoint. The built-in default
/// is not a candidate; it is the fallback [`select_endpoint`] returns when
/// nothing else exists.
pub fn collect_candidates<S: Sys>(
    sys: &S,
    auto_discovery: bool,
    configured: Option<&str>,
) -> Vec<LlamaServerCandidate> {
    let mut candidates = if auto_discovery {
        discover_processes(sys)
    } else {
        Vec::new()
    };

    if let Some(endpoint) = configured {
        let endpoint = endpoint.trim();
        if !endpoint.is_empty() && !candidates.iter().any(|c| c.endpoint == endpoint) {
            candidates.push(LlamaServerCandidate {
                endpoint: endpoint.to_string(),
                source: ServerSource::Configured,
                command: None,
            });
        }
    }

    candidates
}

/// The endpoint to monitor. Deterministic:
///
/// 1. auto-discovery found a server → the best discovered one (lowest port,
///    then endpoint; never by PID);
/// 2. else a configured endpoint (when present, non-empty);
/// 3. else `default`.
///
/// A discovered server takes precedence over the configured one, matching the
/// historical behavior. The non-selected candidates are still returned by
/// [`collect_candidates`] so the user can pin one via `server=` and so
/// diagnostics (S18) can show them.
pub fn select_endpoint(candidates: &[LlamaServerCandidate], default: &str) -> String {
    if let Some(first) = candidates.first() {
        if matches!(first.source, ServerSource::Process { .. }) {
            return first.endpoint.clone();
        }
    }
    candidates
        .iter()
        .find(|c| matches!(c.source, ServerSource::Configured))
        .map(|c| c.endpoint.clone())
        .unwrap_or_else(|| default.to_string())
}

/// The local process PID behind `selected_endpoint`, when that endpoint was
/// produced by a discovered `llama-server` / `llama serve` process.
///
/// Selection stays endpoint-based (matching [`select_endpoint`]) — the PID is
/// only a means of attributing the process to a GPU (S16), never the
/// selection criterion. Returns `None` when the endpoint is configured/remote
/// or was not produced by a running local process.
pub fn selected_endpoint_pid(
    candidates: &[LlamaServerCandidate],
    selected_endpoint: &str,
) -> Option<u32> {
    candidates
        .iter()
        .find(|c| c.endpoint == selected_endpoint)
        .and_then(|c| match c.source {
            ServerSource::Process { pid } => Some(pid),
            ServerSource::Configured => None,
        })
}

fn parse_cmdline(raw: &str) -> Vec<String> {
    raw.split('\0')
        .filter(|arg| !arg.is_empty())
        .map(ToString::to_string)
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

fn parse_port(endpoint: &str) -> u16 {
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    /// Build a NUL-delimited `cmdline` string from args (as `/proc/*/cmdline`
    /// stores it), avoiding octal-looking `\0` escapes in literals.
    fn cmdline(parts: &[&str]) -> String {
        parts.join("\0")
    }

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

    /// Two local servers arranged so that "lowest PID" and "lowest port"
    /// disagree: the lower PID (4000) runs the higher port (9090) and the
    /// higher PID (5000) runs the lower port (8081). A non-llama process (6000)
    /// is also present and must be ignored.
    fn proc_fixture() -> FixtureSys {
        let mut sys = FixtureSys::default();
        sys.dir_entry("/proc", "4000", false, false)
            .dir_entry("/proc", "5000", false, false)
            .dir_entry("/proc", "6000", false, false);
        sys.file(
            "/proc/4000/cmdline",
            cmdline(&["/usr/bin/llama-server", "--port", "9090"]),
        );
        sys.file(
            "/proc/5000/cmdline",
            cmdline(&[
                "/usr/bin/llama-server",
                "--host",
                "127.0.0.1",
                "--port",
                "8081",
            ]),
        );
        sys.file(
            "/proc/6000/cmdline",
            cmdline(&["/usr/bin/other", "--port", "1234"]),
        );
        sys
    }

    #[test]
    fn discovers_all_servers_and_ignores_non_llama() {
        let sys = proc_fixture();
        let candidates = discover_processes(&sys);

        assert_eq!(candidates.len(), 2, "only the two llama servers");
        // Ordered by port (8081 before 9090), not by PID.
        assert_eq!(candidates[0].endpoint, "http://127.0.0.1:8081");
        assert!(matches!(
            candidates[0].source,
            ServerSource::Process { pid: 5000 }
        ));
        assert_eq!(candidates[1].endpoint, "http://127.0.0.1:9090");
        assert!(matches!(
            candidates[1].source,
            ServerSource::Process { pid: 4000 }
        ));
    }

    #[test]
    fn selection_is_by_port_not_pid() {
        let sys = proc_fixture();
        let candidates = discover_processes(&sys);

        // Lowest PID is 4000 (port 9090); the pick must be port 8081 (pid 5000).
        let best = select_endpoint(&candidates, "http://127.0.0.1:8080");
        assert_eq!(best, "http://127.0.0.1:8081");
    }

    #[test]
    fn dedupes_identical_endpoints() {
        let mut sys = FixtureSys::default();
        sys.dir_entry("/proc", "100", false, false)
            .dir_entry("/proc", "200", false, false);
        sys.file(
            "/proc/100/cmdline",
            cmdline(&["/usr/bin/llama-server", "--port", "8080"]),
        );
        sys.file(
            "/proc/200/cmdline",
            cmdline(&["/usr/bin/llama-server", "--port", "8080"]),
        );

        let candidates = discover_processes(&sys);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].endpoint, "http://127.0.0.1:8080");
    }

    #[test]
    fn missing_proc_yields_no_candidates() {
        let sys = FixtureSys::default();
        assert!(discover_processes(&sys).is_empty());
    }

    #[test]
    fn configured_and_default_selection() {
        let sys = FixtureSys::default(); // no processes

        // auto on, no processes, configured set -> configured.
        let c = collect_candidates(&sys, true, Some("http://10.0.0.7:9090"));
        assert_eq!(
            select_endpoint(&c, "http://127.0.0.1:8080"),
            "http://10.0.0.7:9090"
        );

        // auto on, nothing -> default.
        let c = collect_candidates(&sys, true, None);
        assert_eq!(
            select_endpoint(&c, "http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn configured_duplicate_of_discovered_is_not_pushed_twice() {
        let sys = proc_fixture(); // pid 5000 -> http://127.0.0.1:8081
        let c = collect_candidates(&sys, true, Some("http://127.0.0.1:8081"));
        assert_eq!(
            c.iter()
                .filter(|x| x.endpoint == "http://127.0.0.1:8081")
                .count(),
            1
        );
    }

    #[test]
    fn auto_discovery_off_excludes_processes() {
        let sys = proc_fixture();
        let c = collect_candidates(&sys, false, Some("http://10.0.0.5:8080"));
        // Only the configured endpoint; the two local servers are hidden.
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].source, ServerSource::Configured));
        assert_eq!(
            select_endpoint(&c, "http://127.0.0.1:8080"),
            "http://10.0.0.5:8080"
        );
    }

    #[test]
    fn discovered_beats_configured_when_auto_is_on() {
        let sys = proc_fixture();
        let c = collect_candidates(&sys, true, Some("http://10.0.0.5:8080"));
        // Discovered (8081) precedes configured; selection picks discovered.
        assert_eq!(c[0].source, ServerSource::Process { pid: 5000 });
        assert_eq!(
            select_endpoint(&c, "http://127.0.0.1:8080"),
            "http://127.0.0.1:8081"
        );
        // ...but the configured endpoint is still recorded for diagnostics.
        assert!(c.iter().any(|x| x.endpoint == "http://10.0.0.5:8080"));
    }
}
