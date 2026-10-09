//! `orsiktop diag` — a human-readable diagnostics dump for bug reports.
//!
//! Summarizes the non-secret state OrsikTop sees at startup: CPU topology,
//! GPU discovery (DRM sysfs), the provider selected for the auto selector
//! and a one-shot sample of it, CPU sensor discovery, and llama.cpp
//! server discovery plus the server→GPU mapping decision.
//!
//! No-secrets policy: process evidence is PID-only (never command lines),
//! no environment variables, no credentials; the only network contact is
//! one bounded read-only probe of the resolved llama endpoint.
//!
//! `render` is pure with respect to the filesystem (everything goes
//! through [`Sys`]); the live llama HTTP probe happens in `run`.

use crate::{
    config,
    cpu::{detect_topology, CpuCoreKind},
    cpu_sensors::CpuSensors,
    discovery::{discover_gpus, DiscoveredGpu},
    discovery_llm::{
        collect_candidates, read_process_start_time, select_endpoint, selected_endpoint_server,
        LocalServerIdentity, ServerSource,
    },
    domain::{GpuEvidence, GpuMapping, GpuSelector},
    gpu::new_gpu_provider,
    gpu_map::{map_server_gpus, nvml_compute_gpus, process_render_gpus},
    llama::{LlamaMonitor, LlmStats, UNKNOWN_MODEL_LABEL},
    providers::nvidia::{discover_mig_children, MigChild},
    redact::{redact_urls, safe_endpoint},
    system::RealSys,
};

use nvml_wrapper::Nvml;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";

/// Render the full diagnostics report to stdout (and run the bounded live
/// llama probe).
///
/// `settings` is the same configuration the TUI runs with, so one-off CLI
/// overrides (`--server`, `--gpu`, `--interval-ms`) apply to `diag` too.
///
/// `multi` switches to the opt-in multi-server overview (`orsiktop diag
/// --servers`): every monitored endpoint is probed (bounded, read-only) and
/// any configured endpoint past the cap is listed as not monitored. The
/// default single-server report is unchanged.
pub fn run(settings: &config::AppConfig, multi: bool) -> Result<(), Box<dyn std::error::Error>> {
    if multi {
        return run_multi(settings);
    }
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let report = render(
        &RealSys,
        logical,
        settings.auto_discovery,
        settings.server.as_deref(),
        &settings.gpu_selector,
    );
    print!("{report}");
    probe_llama(
        &RealSys,
        settings.auto_discovery,
        settings.server.as_deref(),
    );
    Ok(())
}

/// Build the diagnostics report as a string (testable without a TTY).
///
/// The CPU, sensor and llama/discovery/mapping sections read only through
/// `sys`, so a `FixtureSys` makes them deterministic. The GPU provider sample
/// (`provider_section`) and the NVML mapping query run against live hardware —
/// NVML cannot be injected — so those parts stay host-dependent even under a
/// fixture. The llama section prints discovery + the mapping decision, not live
/// connection state.
pub fn render<S: crate::system::Sys>(
    sys: &S,
    logical: usize,
    auto_discovery: bool,
    server: Option<&str>,
    gpu_selector: &GpuSelector,
) -> String {
    let mut out = String::new();

    out.push_str("OrsikTop diagnostics\n");
    out.push_str(&format!("  version    : {}\n", env!("CARGO_PKG_VERSION")));

    cpu_section(&mut out, sys, logical);
    gpu_section(&mut out, sys, gpu_selector);
    cpu_sensors_section(&mut out, sys);
    llama_section(&mut out, sys, auto_discovery, server);

    out
}

fn cpu_section(out: &mut String, sys: &impl crate::system::Sys, logical: usize) {
    let topology = detect_topology(logical, sys);
    out.push_str("CPU\n");
    out.push_str(&format!("  vendor     : {}\n", topology.vendor.label()));
    out.push_str(&format!("  model      : {}\n", topology.model));
    out.push_str(&format!("  logical    : {} CPUs\n", topology.logical_cpus));
    out.push_str(&format!(
        "  physical   : {} cores\n",
        opt(topology.physical_cores)
    ));
    out.push_str(&format!(
        "  classes    : P={} E={} LP={}\n",
        opt(topology.performance_cores),
        opt(topology.efficiency_cores),
        opt(topology.low_power_cores)
    ));
    out.push_str(&format!(
        "  hybrid     : {}\n",
        if topology.is_hybrid() {
            "yes (P/E minibar)"
        } else {
            "no"
        }
    ));
    if !topology.core_kinds.is_empty() {
        let kinds: Vec<&str> = topology.core_kinds.iter().map(kind_short).collect();
        out.push_str(&format!("  core kinds : {}\n", kinds.join(",")));
    }
}

fn kind_short(kind: &CpuCoreKind) -> &'static str {
    match kind {
        CpuCoreKind::Performance => "P",
        CpuCoreKind::Efficiency => "E",
        CpuCoreKind::LowPower => "L",
        CpuCoreKind::Unknown => "?",
    }
}

fn gpu_section(out: &mut String, sys: &impl crate::system::Sys, gpu_selector: &GpuSelector) {
    let gpus = discover_gpus(sys);
    out.push_str("GPU discovery (DRM sysfs)\n");
    if gpus.is_empty() {
        out.push_str("  no display-class DRM devices found\n");
    }
    for (i, gpu) in gpus.iter().enumerate() {
        let bdf = gpu.device_id.key();
        out.push_str(&format!(
            "  [{}] {} {} vendor={}\n",
            i,
            gpu.card,
            if bdf.is_empty() { "<no BDF>" } else { bdf },
            vendor_name(gpu.vendor)
        ));
        out.push_str(&format!(
            "       pci {:04x}:{:04x} class={:08x} driver={} render={}\n",
            gpu.pci_vendor_id,
            gpu.pci_device_id,
            gpu.pci_class_code,
            if gpu.driver.is_empty() {
                "<unbound>"
            } else {
                &gpu.driver
            },
            opt((!gpu.render_nodes.is_empty()).then(|| gpu.render_nodes.join(", ")))
        ));
        if !gpu.outputs.is_empty() {
            out.push_str(&format!("       outputs  : {}\n", gpu.outputs.join(", ")));
        }
    }
    provider_section(out, &gpus, gpu_selector);
}

/// One-shot provider sample for the `Auto` selection: shows which backend
/// the dispatch picks and which normalized metrics it exposes, without
/// running the TUI.
fn provider_section(
    out: &mut String,
    gpus: &[crate::discovery::DiscoveredGpu],
    gpu_selector: &GpuSelector,
) {
    let selector = gpu_selector.clone();
    let mut provider = new_gpu_provider(selector.clone(), gpus);
    let stats = provider.sample();
    out.push_str(&format!("GPU provider (selector = {selector:?})\n"));
    if stats.available {
        let name = if stats.name.is_empty() {
            "<unnamed>"
        } else {
            &stats.name
        };
        out.push_str(&format!("  device     : {name}\n"));
        out.push_str(&format!(
            "  utilization: {}  temperature: {}  power: {}\n",
            opt(stats.utilization.map(|u| format!("{u:.0}%"))),
            opt(stats.temperature_c.map(|t| format!("{t:.0}°C"))),
            opt(stats.power_w.map(|p| format!("{p:.1}W")))
        ));
    } else if !gpus.is_empty() {
        out.push_str(&format!(
            "  no sample  : {}\n",
            if stats.error.is_empty() {
                "<no error>"
            } else {
                &stats.error
            }
        ));
    } else {
        out.push_str("  no sample  : no devices to probe\n");
    }
    let caps = [
        ("utilization", stats.utilization.is_some()),
        (
            "memory",
            stats.memory_total_mib.is_some() && stats.memory_used_mib.is_some(),
        ),
        ("temperature", stats.temperature_c.is_some()),
        ("power", stats.power_w.is_some()),
        ("clocks", stats.graphics_clock_mhz.is_some()),
        ("fan", stats.fan_percent.is_some()),
        ("pcie", stats.pcie_link_speed_gts.is_some()),
        (
            "enc/dec",
            stats.encoder_utilization.is_some() || stats.decoder_utilization.is_some(),
        ),
    ];
    let available: Vec<&str> = caps.iter().filter(|(_, v)| *v).map(|(n, _)| *n).collect();
    let missing: Vec<&str> = caps.iter().filter(|(_, v)| !v).map(|(n, _)| *n).collect();
    out.push_str(&format!("  available  : {}\n", join_or_none(&available)));
    out.push_str(&format!("  unavailable: {}\n", join_or_none(&missing)));
}

fn cpu_sensors_section(out: &mut String, sys: &impl crate::system::Sys) {
    let mut sensors = CpuSensors::default();
    sensors.discover(sys);
    let freq = sensors.sample_frequency_mhz(sys);
    let temp = sensors.sample_temperature_c(sys);
    let power = sensors.sample_power_w(sys);
    out.push_str("CPU sensors\n");
    out.push_str(&format!(
        "  frequency  : {} MHz\n",
        opt(freq.map(|f| format!("{f:.0}")))
    ));
    out.push_str(&format!(
        "  temperature: {} °C\n",
        opt(temp.map(|t| format!("{t:.0}")))
    ));
    out.push_str(&format!(
        "  power      : {} W{}\n",
        opt(power.map(|p| format!("{p:.1}"))),
        if power.is_none() {
            " (unavailable)"
        } else {
            ""
        }
    ));
}

fn llama_section(
    out: &mut String,
    sys: &impl crate::system::Sys,
    auto_discovery: bool,
    server: Option<&str>,
) {
    let candidates = collect_candidates(sys, auto_discovery, server);
    let endpoint = select_endpoint(&candidates, DEFAULT_SERVER);
    let source_label = if auto_discovery
        && candidates
            .first()
            .is_some_and(|c| matches!(c.source, ServerSource::Process { .. }))
    {
        "auto-discovered"
    } else if server.is_some() {
        "configured"
    } else {
        "default"
    };

    // Identity (PID + start time) of the local process behind the selected
    // endpoint (PID-only evidence — command lines are never read or printed).
    let server = selected_endpoint_server(&candidates, &endpoint);

    // Server→GPU mapping decision, exactly like the TUI computes it (S16):
    // only attributed while the PID still carries the start time read at
    // discovery, so a reused PID is never mapped to the earlier server.
    let gpus = discover_gpus(sys);
    let nvml = Nvml::init().ok();
    let mig_children = nvml.as_ref().map(discover_mig_children).unwrap_or_default();
    let mapping = server_gpu_mapping(sys, server, &gpus, nvml.as_ref(), &mig_children);

    out.push_str("llama.cpp\n");
    if candidates.is_empty() {
        out.push_str("  no local server processes found\n");
    }
    for (i, candidate) in candidates.iter().enumerate() {
        let source = match &candidate.source {
            ServerSource::Process { pid } => format!("process (pid {pid})"),
            ServerSource::Configured => "configured".to_string(),
        };
        out.push_str(&format!(
            "  candidate[{}]: {source} {}\n",
            i,
            safe_endpoint(&candidate.endpoint)
        ));
    }
    out.push_str(&format!(
        "  endpoint   : {} ({source_label})\n",
        safe_endpoint(&endpoint)
    ));
    out.push_str(&format!("  gpu map    : {}\n", describe_mapping(&mapping)));
}

/// The `connected` section of the live probe, independent of the network so
/// its unavailable-value handling is fixture-testable. Without `/metrics` the
/// watermark and rate are unknown and print as `—` / are omitted, never a
/// fabricated zero.
fn connected_probe_report(stats: &LlmStats) -> String {
    let model = if stats.model.is_empty() {
        "<unnamed>"
    } else {
        &stats.model
    };
    let watermark = match (stats.metrics_available, stats.context_high_watermark) {
        (true, Some(value)) => value.to_string(),
        _ => "—".to_string(),
    };
    let used = stats
        .context_used
        .map_or_else(|| "—".to_string(), |value| value.to_string());
    // A capacity of 0 means both /props and /slots were unavailable, so the
    // pair is unknown — never a fabricated "0".
    let context = if stats.context_size > 0 {
        format!("{used}/{}", stats.context_size)
    } else {
        "—".to_string()
    };
    // /slots may be down while /metrics is up (connected, but no slot state):
    // fall back to the /props total, else `—`, never a fabricated "0 busy".
    let slots = slots_line(stats);
    let mut lines = vec![
        "  status     : connected".to_string(),
        format!("  model      : {model}"),
        format!(
            "  metrics    : {}",
            if stats.metrics_available {
                "available"
            } else {
                "unavailable"
            }
        ),
        format!("  context    : {context} (watermark {watermark})"),
        format!("  slots      : {slots}"),
    ];
    if stats.metrics_available && (stats.prompt_tps.is_some() || stats.generation_tps.is_some()) {
        lines.push(format!(
            "  rate       : prompt {} tps, generation {} tps",
            tps_text(stats.prompt_tps),
            tps_text(stats.generation_tps)
        ));
    }
    lines.join("\n")
}

/// A live throughput value, or `—` when it is unavailable (never 0.0).
fn tps_text(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_string(), |value| format!("{value:.1}"))
}

/// Live bounded llama probe (750 ms connect + 1200 ms total, see llama.rs).
/// Only `run` calls this — `render` stays network-free and fixture-testable.
fn probe_llama<S: crate::system::Sys>(sys: &S, auto_discovery: bool, server: Option<&str>) {
    let candidates = collect_candidates(sys, auto_discovery, server);
    let endpoint = select_endpoint(&candidates, DEFAULT_SERVER);
    match LlamaMonitor::new(&endpoint) {
        Ok(mut monitor) => {
            let stats = monitor.sample();
            if stats.connected {
                println!("{}", connected_probe_report(&stats));
            } else {
                // reqwest embeds the request URL in its errors; redact any URL
                // so a credential-bearing endpoint cannot leak through the
                // error path.
                let detail = if stats.error.is_empty() {
                    "no error detail".to_string()
                } else {
                    redact_urls(&stats.error)
                };
                println!("  status     : unreachable ({detail})");
            }
        }
        Err(err) => println!(
            "  status     : client init failed ({})",
            crate::redact::sanitize_display_text(
                &redact_urls(&err.to_string()),
                crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
            )
        ),
    }
}

// ---------------------------------------------------------------------------
// Multi-server overview (`orsiktop diag --servers`)
// ---------------------------------------------------------------------------

/// One monitored server as shown in the multi-server overview.
struct MultiServerEntry {
    /// Display label: the server-sanitized model name, or the redacted
    /// endpoint when no model is known.
    label: String,
    /// Raw endpoint; rendered through [`safe_endpoint`] so no credential can
    /// reach stdout.
    endpoint: String,
    stats: LlmStats,
    mapping: GpuMapping,
}

/// Probe every monitored endpoint concurrently and return their stats in the
/// original order.
///
/// Bounded by the server cap: one scoped thread per server, joined when the
/// scope closes, so a slow or hanging endpoint cannot delay the others and
/// diagnostics always terminate. Each probe reuses the production
/// [`LlamaMonitor`] timeouts (750 ms connect / 1200 ms total), and the whole
/// set is bounded by the 16-server cap.
fn probe_servers(servers: &[crate::app::ServerSpec]) -> Vec<LlmStats> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = servers
            .iter()
            .take(crate::domain::MAX_MONITORED_SERVERS)
            .map(|spec| {
                let endpoint = spec.endpoint.clone();
                scope.spawn(move || match LlamaMonitor::new(&endpoint) {
                    Ok(mut monitor) => monitor.sample(),
                    Err(err) => LlmStats {
                        // A client-construction error is rare; redact any URL
                        // defensively so an endpoint credential cannot leak, and
                        // pass it through the same display-safe boundary as the
                        // normal sample path.
                        error: crate::redact::sanitize_display_text(
                            &redact_urls(&err.to_string()),
                            crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
                        ),
                        ..Default::default()
                    },
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// The GPU attribution for one server, from its verified local process
/// identity. `None` (no local process) is deliberately distinct from
/// `Unknown` (a local process that could not be attributed): a remote or
/// configured endpoint is *not evaluated*, never guessed.
fn server_gpu_mapping<S: crate::system::Sys>(
    sys: &S,
    identity: Option<LocalServerIdentity>,
    gpus: &[DiscoveredGpu],
    nvml: Option<&Nvml>,
    mig_children: &[MigChild],
) -> GpuMapping {
    let Some(identity) = identity else {
        return GpuMapping::None;
    };
    // PID-reuse guard: only attribute while the PID still carries the start
    // time read at discovery.
    if read_process_start_time(sys, identity.pid) != Some(identity.start_time) {
        return GpuMapping::Unknown;
    }
    let render = process_render_gpus(sys, identity.pid, gpus);
    let nvml_keys = nvml_compute_gpus(nvml, identity.pid, mig_children);
    map_server_gpus(true, render, nvml_keys, gpus)
}

/// Run the opt-in multi-server overview: collect the monitored/overflow set,
/// probe the monitored endpoints once, attribute GPUs, and print the report.
fn run_multi(settings: &config::AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let list = crate::collect_server_list(settings);
    let stats = probe_servers(&list.monitored);

    // Expensive host evidence (DRM discovery + optional NVML) is gathered once
    // and reused for every server, so host queries do not grow per endpoint.
    let gpus = discover_gpus(&RealSys);
    let nvml = Nvml::init().ok();
    let mig_children = nvml.as_ref().map(discover_mig_children).unwrap_or_default();

    let entries: Vec<MultiServerEntry> = list
        .monitored
        .iter()
        .zip(stats)
        .map(|(spec, stats)| {
            let mapping =
                server_gpu_mapping(&RealSys, spec.identity, &gpus, nvml.as_ref(), &mig_children);
            // Only a connected server with a real (non-placeholder) model name
            // is labelled by it; otherwise the redacted endpoint is clearer and
            // never claims a model that was not reported.
            let label = entry_label(&stats, &spec.endpoint);
            MultiServerEntry {
                label,
                endpoint: spec.endpoint.clone(),
                stats,
                mapping,
            }
        })
        .collect();

    let overflow: Vec<String> = list
        .overflow
        .iter()
        .map(|spec| spec.endpoint.clone())
        .collect();

    print!("{}", render_multi_report(&entries, &overflow));
    Ok(())
}

/// Display label for one monitored server: the real model name only when the
/// server is connected and reported one; otherwise the redacted endpoint. The
/// [`UNKNOWN_MODEL_LABEL`] placeholder is treated as "no model known".
fn entry_label(stats: &LlmStats, endpoint: &str) -> String {
    if stats.connected && !stats.model.is_empty() && stats.model != UNKNOWN_MODEL_LABEL {
        stats.model.clone()
    } else {
        safe_endpoint(endpoint)
    }
}

/// Build the multi-server overview string (pure; testable without a TTY or a
/// live server). Only values actually reported by a server are shown: unknown
/// slots/context/metrics render `—`, never a fabricated zero, and a single
/// counter snapshot never produces a tok/s rate.
fn render_multi_report(entries: &[MultiServerEntry], overflow: &[String]) -> String {
    let mut out = String::new();
    out.push_str("OrsikTop multi-server diagnostics\n");
    out.push_str(&format!("  version    : {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!(
        "  monitored  : {} / {}\n",
        entries.len(),
        crate::domain::MAX_MONITORED_SERVERS
    ));
    out.push_str(&format!("  over limit : {}\n", overflow.len()));

    for (index, entry) in entries.iter().enumerate() {
        out.push_str(&format!("\n[{}] {}\n", index + 1, entry.label));
        out.push_str(&format!(
            "  endpoint   : {}\n",
            safe_endpoint(&entry.endpoint)
        ));
        out.push_str(&format!("  status     : {}\n", status_line(&entry.stats)));
        out.push_str(&format!(
            "  metrics    : {}\n",
            if entry.stats.metrics_available {
                "available"
            } else {
                "unavailable"
            }
        ));
        out.push_str(&format!("  slots      : {}\n", slots_line(&entry.stats)));
        out.push_str(&format!("  context    : {}\n", context_line(&entry.stats)));
        out.push_str(&format!(
            "  gpu        : {}\n",
            describe_mapping(&entry.mapping)
        ));
    }

    if !overflow.is_empty() {
        out.push_str(&format!(
            "\nNot monitored (server cap of {} reached; not polled — availability not checked):\n",
            crate::domain::MAX_MONITORED_SERVERS
        ));
        for (index, endpoint) in overflow.iter().enumerate() {
            out.push_str(&format!("  [{}] {}\n", index + 1, safe_endpoint(endpoint)));
        }
    }

    out
}

fn status_line(stats: &LlmStats) -> String {
    if stats.connected {
        if stats.metrics_available {
            "connected".to_string()
        } else {
            "connected (metrics unavailable)".to_string()
        }
    } else {
        let detail = if stats.error.is_empty() {
            "no error detail".to_string()
        } else {
            redact_urls(&stats.error)
        };
        format!("unreachable ({detail})")
    }
}

fn slots_line(stats: &LlmStats) -> String {
    if stats.slots_available {
        format!("{} busy / {} total", stats.busy_slots, stats.slot_count)
    } else if stats.props_slot_count > 0 {
        format!("—/{} total", stats.props_slot_count)
    } else {
        "—".to_string()
    }
}

fn context_line(stats: &LlmStats) -> String {
    // Mirror the TUI: when /slots is unavailable the /metrics high-water mark
    // is the best known occupancy. Unknown capacity renders `—`, never `/0`.
    let used = if stats.slots_available {
        stats.context_used
    } else {
        stats.context_high_watermark
    };
    match (used, stats.context_size) {
        (Some(used), size) if size > 0 => format!("{used}/{size}"),
        (None, size) if size > 0 => format!("—/{size}"),
        _ => "—".to_string(),
    }
}

fn vendor_name(vendor: crate::domain::GpuVendor) -> &'static str {
    match vendor {
        crate::domain::GpuVendor::Nvidia => "nvidia",
        crate::domain::GpuVendor::Amd => "amd",
        crate::domain::GpuVendor::Intel => "intel",
        crate::domain::GpuVendor::Other => "other",
        crate::domain::GpuVendor::Unknown => "unknown",
    }
}

fn opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn join_or_none(items: &[&str]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

/// Human-readable `GpuMapping` state — explicit, never a guess.
pub fn describe_mapping(mapping: &GpuMapping) -> String {
    match mapping {
        GpuMapping::None => "not computed (no local server process)".to_string(),
        GpuMapping::Single(gpu) => {
            format!(
                "single {} (evidence: {})",
                gpu.key(),
                evidence_name(gpu.evidence)
            )
        }
        GpuMapping::Multi(devices) => format!("multi ({} devices)", devices.len()),
        GpuMapping::Unknown => "unknown (insufficient evidence)".to_string(),
    }
}

fn evidence_name(evidence: GpuEvidence) -> &'static str {
    match evidence {
        GpuEvidence::NvmlCompute => "NVML compute",
        GpuEvidence::RenderNodeFd => "DRM render-node fd",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceId, GpuVendor, MappedGpu};

    fn fixture_with_gpu(sys: &mut crate::system::FixtureSys, card: &str, bdf: &str, render: &str) {
        let pci = format!("/sys/devices/pci0000:00/{bdf}");
        sys.file(format!("{pci}/vendor").as_str(), "0x8086\n");
        sys.file(format!("{pci}/device").as_str(), "0x76a0\n");
        sys.file(format!("{pci}/class").as_str(), "0x030000\n");
        sys.symlink(
            format!("{pci}/driver").as_str(),
            "../../../bus/pci/drivers/xe",
        );
        sys.symlink(
            "/sys/class/drm/renderD128",
            &format!("../../devices/pci0000:00/{bdf}/drm/renderD128"),
        );
        sys.dir_entry("/sys/class/drm", "renderD128", false, true);
        sys.dir_entry("/sys/class/drm", card, false, true);
        sys.symlink(
            format!("/sys/class/drm/{card}").as_str(),
            &format!("../../devices/pci0000:00/{bdf}/drm/{card}"),
        );
        sys.dir_entry(
            "/dev/dri/by-path",
            &format!("pci-{bdf}-render"),
            false,
            true,
        );
        sys.symlink(
            format!("/dev/dri/by-path/pci-{bdf}-render").as_str(),
            &format!("/dev/dri/{render}"),
        );
        sys.dir_entry("/dev/dri", render, false, true);
    }

    #[test]
    fn render_on_empty_sys_is_well_formed() {
        let fixture = crate::system::FixtureSys::default();
        let out = render(&fixture, 4, true, None, &GpuSelector::Auto);
        // The report always carries the fixed section headers.
        assert!(
            out.contains("OrsikTop diagnostics\n"),
            "missing header:\n{out}"
        );
        assert!(out.contains("CPU\n"), "missing CPU section:\n{out}");
        assert!(
            out.contains("GPU discovery (DRM sysfs)"),
            "missing GPU section:\n{out}"
        );
        assert!(
            out.contains("CPU sensors"),
            "missing sensors section:\n{out}"
        );
        assert!(out.contains("llama.cpp\n"), "missing llama section:\n{out}");
        // No display-class devices on an empty sysfs.
        assert!(out.contains("no display-class DRM devices found"));
        // Nothing discovered → the default endpoint is selected.
        assert!(
            out.contains(&format!("{DEFAULT_SERVER} (default)")),
            "{out}"
        );
        // No local process → the mapping is explicitly not computed.
        assert!(out.contains("gpu map    : not computed"), "{out}");
    }

    #[test]
    fn render_maps_local_server_process_to_render_gpu() {
        let mut fixture = crate::system::FixtureSys::default();
        fixture_with_gpu(&mut fixture, "card0", "0000:01:00.0", "renderD128");

        // A llama-server process whose cmdline matches and which holds an
        // open fd on the by-path render node.
        fixture.dir_entry("/proc", "4242", false, false);
        fixture.file("/proc/4242/cmdline", "llama-server\0--port\08080\0");
        fixture.file(
            "/proc/4242/stat",
            "4242 (llama-server) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 5 5 0 20 0 1 0 555 \
             12345 678 90",
        );
        fixture.dir_entry("/proc/4242/fd", "7", false, false);
        fixture.symlink(
            "/proc/4242/fd/7",
            "/dev/dri/by-path/pci-0000:01:00.0-render",
        );

        let out = render(&fixture, 4, true, None, &GpuSelector::Auto);
        assert!(
            out.contains("candidate[0]: process (pid 4242)"),
            "missing candidate:\n{out}"
        );
        assert!(
            out.contains("endpoint   : http://127.0.0.1:8080 (auto-discovered)"),
            "{out}"
        );
        assert!(
            out.contains("gpu map    : single 0000:01:00.0 (evidence: DRM render-node fd)"),
            "{out}"
        );
    }

    #[test]
    fn mapping_states_are_described_explicitly() {
        assert_eq!(
            describe_mapping(&GpuMapping::None),
            "not computed (no local server process)"
        );
        assert_eq!(
            describe_mapping(&GpuMapping::Unknown),
            "unknown (insufficient evidence)"
        );
        let single = GpuMapping::Single(MappedGpu {
            device: DeviceId::new(Some("0000:01:00.0".into()), None),
            name: "card0".into(),
            vendor: GpuVendor::Nvidia,
            evidence: GpuEvidence::NvmlCompute,
        });
        assert_eq!(
            describe_mapping(&single),
            "single 0000:01:00.0 (evidence: NVML compute)"
        );
        let multi = GpuMapping::Multi(vec![
            MappedGpu {
                device: DeviceId::new(Some("0000:01:00.0".into()), None),
                name: "card0".into(),
                vendor: GpuVendor::Nvidia,
                evidence: GpuEvidence::NvmlCompute,
            },
            MappedGpu {
                device: DeviceId::new(Some("0000:02:00.0".into()), None),
                name: "card1".into(),
                vendor: GpuVendor::Nvidia,
                evidence: GpuEvidence::NvmlCompute,
            },
        ]);
        assert_eq!(describe_mapping(&multi), "multi (2 devices)");
    }

    #[test]
    fn cpu_section_renders_fixture_topology() {
        let mut fixture = crate::system::FixtureSys::default();
        fixture.file(
            "/proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Core(TM) Ultra 9\n",
        );
        fixture
            .file("/sys/devices/cpu_core/cpus", "0-1")
            .file("/sys/devices/cpu_atom/cpus", "2-3");
        for cpu in 0..4usize {
            fixture.file(
                &format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"),
                cpu.to_string(),
            );
        }
        let out = render(&fixture, 4, true, None, &GpuSelector::Auto);
        assert!(out.contains("vendor     : Intel"), "missing vendor:\n{out}");
        assert!(out.contains("classes    : P=2 E=2 LP=0"), "{out}");
        assert!(out.contains("hybrid     : yes (P/E minibar)"), "{out}");
        assert!(out.contains("core kinds : P,P,E,E"), "{out}");
    }

    #[test]
    fn render_reports_the_selected_gpu_selector() {
        // `--gpu` reaches the report: the provider section must name the
        // requested selector instead of always claiming `Auto`.
        let fixture = crate::system::FixtureSys::default();
        let out = render(
            &fixture,
            4,
            false,
            None,
            &GpuSelector::PciBusId("0000:01:00.0".to_string()),
        );
        assert!(
            out.contains("GPU provider (selector = PciBusId(\"0000:01:00.0\"))"),
            "{out}"
        );
    }

    #[test]
    fn render_redacts_credentials_and_url_extras_in_diagnostics_output() {
        let fixture = crate::system::FixtureSys::default();
        let out = render(
            &fixture,
            4,
            false,
            Some("http://demo-user:demo-password@localhost:8080/v1?token=supersecret#fragsecret"),
            &GpuSelector::Auto,
        );
        for secret in ["demo-user", "demo-password", "supersecret", "fragsecret"] {
            // Static failure message: never echo the synthetic secret or the
            // full report (cleartext-logging risk). The condition still fails
            // the test if redaction regresses.
            assert!(
                !out.contains(secret),
                "diagnostics output unexpectedly contains sensitive information"
            );
        }
        // The connection-relevant part is still shown, sanitized.
        assert!(out.contains("http://localhost:8080"), "{out}");
        assert!(out.contains("(configured)"), "{out}");
    }

    #[test]
    fn probe_report_marks_metrics_unavailable_without_fabricating_a_watermark() {
        // Regression: with /metrics disabled/failing the probe still shows the
        // /slots-derived context, but the /metrics-only watermark must be `—`,
        // not a fabricated 0, and no rate may be printed.
        let stats = LlmStats {
            connected: true,
            metrics_available: false,
            context_used: Some(1000),
            context_size: 4096,
            slot_count: 1,
            busy_slots: 0,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(report.contains("metrics    : unavailable"), "{report}");
        assert!(report.contains("watermark —"), "{report}");
        assert!(!report.contains("watermark 0"), "{report}");
        assert!(!report.contains("rate"), "{report}");
        assert!(report.contains("context    : 1000/4096"), "{report}");
    }

    #[test]
    fn probe_report_omits_rate_when_throughput_is_absent() {
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            prompt_tps: None,
            generation_tps: None,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(
            !report.contains("rate"),
            "absent throughput must not print a rate: {report}"
        );
    }

    #[test]
    fn probe_report_marks_a_missing_rate_side_unavailable() {
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            prompt_tps: Some(12.5),
            generation_tps: None,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(
            report.contains("prompt 12.5 tps, generation — tps"),
            "got: {report}"
        );
    }

    #[test]
    fn probe_report_includes_watermark_and_rate_when_metrics_are_available() {
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            context_used: Some(1000),
            context_size: 4096,
            context_high_watermark: Some(2048),
            prompt_tps: Some(12.5),
            generation_tps: Some(34.5),
            slot_count: 2,
            busy_slots: 1,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(report.contains("metrics    : available"), "{report}");
        assert!(report.contains("watermark 2048"), "{report}");
        assert!(report.contains("prompt 12.5 tps"), "{report}");
    }

    #[test]
    fn probe_report_does_not_fabricate_zero_slots_when_slots_are_unavailable() {
        // /metrics up but /slots down: `connected` is true (/metrics is the
        // liveness gate) yet the slot state is unknown. It must render `—`,
        // never a fabricated "0 busy / 0 total".
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: false,
            props_slot_count: 0,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(report.contains("slots      : —"), "{report}");
        assert!(!report.contains("0 busy / 0 total"), "{report}");

        // /props still knows the configured total.
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: false,
            props_slot_count: 3,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(report.contains("slots      : —/3 total"), "{report}");
    }

    #[test]
    fn probe_report_does_not_print_a_zero_context_capacity() {
        // Neither /props nor /slots reported a capacity: unknown, not "0".
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            context_size: 0,
            context_used: None,
            ..Default::default()
        };
        let report = connected_probe_report(&stats);
        assert!(report.contains("context    : —"), "{report}");
        assert!(!report.contains("/0"), "{report}");
    }

    fn multi_entry(
        label: &str,
        endpoint: &str,
        stats: LlmStats,
        mapping: GpuMapping,
    ) -> MultiServerEntry {
        MultiServerEntry {
            label: label.to_string(),
            endpoint: endpoint.to_string(),
            stats,
            mapping,
        }
    }

    #[test]
    fn multi_report_separates_states_and_bounds_the_inventory() {
        let mapped = crate::domain::MappedGpu {
            device: crate::domain::DeviceId::new(Some("0000:01:00.0".into()), None),
            name: "card0".into(),
            vendor: GpuVendor::Nvidia,
            evidence: crate::domain::GpuEvidence::NvmlCompute,
        };
        let entries = vec![
            multi_entry(
                "Qwen2.5",
                "http://127.0.0.1:8081",
                LlmStats {
                    connected: true,
                    metrics_available: true,
                    model: "Qwen2.5".into(),
                    slots_available: true,
                    slot_count: 4,
                    busy_slots: 2,
                    context_used: Some(1000),
                    context_size: 4096,
                    ..Default::default()
                },
                GpuMapping::Single(mapped),
            ),
            multi_entry(
                "http://127.0.0.1:8082",
                "http://127.0.0.1:8082",
                LlmStats {
                    connected: false,
                    error: "connection refused".into(),
                    ..Default::default()
                },
                GpuMapping::Unknown,
            ),
            multi_entry(
                "Remote",
                "http://example.org:8080",
                LlmStats {
                    connected: true,
                    ..Default::default()
                },
                GpuMapping::None,
            ),
        ];

        let report = render_multi_report(&entries, &[]);
        assert!(report.contains("monitored  : 3 / 16"), "{report}");
        assert!(report.contains("over limit : 0"), "{report}");
        assert!(report.contains("slots      : 2 busy / 4 total"), "{report}");
        assert!(report.contains("context    : 1000/4096"), "{report}");
        assert!(
            report.contains("gpu        : single 0000:01:00.0 (evidence: NVML compute)"),
            "{report}"
        );
        // Offline, evaluated-but-unknown, and not-evaluated are distinct.
        assert!(
            report.contains("unreachable (connection refused)"),
            "{report}"
        );
        assert!(
            report.contains("unknown (insufficient evidence)"),
            "{report}"
        );
        assert!(
            report.contains("not computed (no local server process)"),
            "{report}"
        );
    }

    #[test]
    fn multi_report_redacts_endpoints_and_lists_overflow_as_not_polled() {
        let entries = vec![multi_entry(
            "srv",
            "http://demo-user:demo-password@localhost:8081/v1?token=topsecret#frag",
            LlmStats {
                connected: true,
                ..Default::default()
            },
            GpuMapping::None,
        )];
        let overflow = vec!["http://host16:1?token=overflowsecret".to_string()];
        let report = render_multi_report(&entries, &overflow);

        for secret in [
            "demo-user",
            "demo-password",
            "topsecret",
            "overflowsecret",
            "frag",
        ] {
            // Static failure message: never echo the synthetic secret or the
            // full report (cleartext-logging risk). The condition still fails
            // the test if redaction regresses.
            assert!(
                !report.contains(secret),
                "diagnostic report unexpectedly contains sensitive information"
            );
        }
        // Connection-relevant, non-secret parts survive.
        assert!(report.contains("http://localhost:8081"), "{report}");
        assert!(report.contains("monitored  : 1 / 16"), "{report}");
        assert!(report.contains("over limit : 1"), "{report}");
        // Overflow is explicitly not monitored and not claimed to be checked.
        assert!(report.contains("Not monitored"), "{report}");
        assert!(report.contains("availability not checked"), "{report}");
        assert!(report.contains("http://host16:1"), "{report}");
    }

    #[test]
    fn context_line_falls_back_to_the_metrics_watermark_without_slots() {
        let stats = LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: false,
            context_used: None,
            context_high_watermark: Some(2048),
            context_size: 4096,
            ..Default::default()
        };
        assert_eq!(context_line(&stats), "2048/4096");

        // No evidence at all: unknown, never a fabricated capacity.
        let empty = LlmStats {
            connected: true,
            metrics_available: true,
            ..Default::default()
        };
        assert_eq!(context_line(&empty), "—");
    }

    #[test]
    fn multi_label_uses_model_only_for_a_connected_real_model() {
        let connected = LlmStats {
            connected: true,
            model: "Qwen".into(),
            ..Default::default()
        };
        assert_eq!(entry_label(&connected, "http://127.0.0.1:8081"), "Qwen");

        // The /props placeholder is not a real model name.
        let placeholder = LlmStats {
            connected: true,
            model: UNKNOWN_MODEL_LABEL.into(),
            ..Default::default()
        };
        assert_eq!(entry_label(&placeholder, "http://host:1"), "http://host:1");

        // An unreachable server never claims the model it may have reported
        // earlier.
        let offline = LlmStats {
            connected: false,
            model: "Qwen".into(),
            ..Default::default()
        };
        assert_eq!(entry_label(&offline, "http://host:1"), "http://host:1");

        // The endpoint fallback is redacted.
        assert_eq!(
            entry_label(&LlmStats::default(), "http://user:pw@host:1/x?token=s"),
            "http://host:1/…?…"
        );
    }

    #[test]
    fn server_gpu_mapping_distinguishes_not_evaluated_from_unknown() {
        let sys = crate::system::FixtureSys::default();
        let gpus: Vec<DiscoveredGpu> = Vec::new();
        // No local process -> not evaluated (never a guess).
        assert_eq!(
            server_gpu_mapping(&sys, None, &gpus, None, &[]),
            GpuMapping::None
        );
        // A local identity whose process is gone: evaluated but unknown.
        let identity = LocalServerIdentity {
            pid: 4242,
            start_time: 7,
        };
        assert_eq!(
            server_gpu_mapping(&sys, Some(identity), &gpus, None, &[]),
            GpuMapping::Unknown
        );
    }

    #[test]
    fn probe_servers_reports_a_healthy_server_while_an_offline_one_is_isolated() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        fn read_path(stream: &mut TcpStream) -> Option<String> {
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let n = stream.read(&mut chunk).ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8_lossy(&buf)
                .lines()
                .next()?
                .split_whitespace()
                .nth(1)
                .map(str::to_string)
        }
        fn write_json(stream: &mut TcpStream, body: &str) {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Some(path) = read_path(&mut stream) {
                            let body = if path.contains("/props") {
                                "{\"model_path\":\"model-A\",\"total_slots\":1,\"default_generation_settings\":{\"n_ctx\":4096}}".to_string()
                            } else if path.contains("/slots") {
                                "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string()
                            } else {
                                "llamacpp:prompt_tokens_total 100\n".to_string()
                            };
                            write_json(&mut stream, &body);
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        // A port bound then released: nothing listens there, so a probe is
        // refused quickly.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let servers = vec![
            crate::app::ServerSpec {
                key: crate::domain::server_key(&format!("http://{addr}")),
                endpoint: format!("http://{addr}"),
                identity: None,
            },
            crate::app::ServerSpec {
                key: crate::domain::server_key(&format!("http://{dead_addr}")),
                endpoint: format!("http://{dead_addr}"),
                identity: None,
            },
        ];
        let stats = probe_servers(&servers);

        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        assert!(
            stats[0].connected && stats[0].metrics_available,
            "healthy server must connect: {:?}",
            stats[0]
        );
        assert!(
            !stats[1].connected,
            "offline server must be isolated, not fail the report: {:?}",
            stats[1]
        );
    }
}
