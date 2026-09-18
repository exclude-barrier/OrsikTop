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
    discovery::discover_gpus,
    discovery_llm::{collect_candidates, select_endpoint, ServerSource},
    domain::{GpuEvidence, GpuMapping, GpuSelector},
    gpu::new_gpu_provider,
    gpu_map::{map_server_gpus, nvml_compute_gpus, process_render_gpus},
    llama::LlamaMonitor,
    providers::nvidia::discover_mig_children,
    system::RealSys,
};

use nvml_wrapper::Nvml;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";

/// Render the full diagnostics report to stdout (and run the bounded live
/// llama probe).
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let settings = config::load();
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let report = render(
        &RealSys,
        logical,
        settings.auto_discovery,
        settings.server.as_deref(),
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
/// Pure with respect to the filesystem: every read goes through `sys`, so a
/// `FixtureSys` yields a deterministic report. The llama section prints
/// discovery + the mapping decision, not live connection state.
pub fn render<S: crate::system::Sys>(
    sys: &S,
    logical: usize,
    auto_discovery: bool,
    server: Option<&str>,
) -> String {
    let mut out = String::new();

    out.push_str("OrsikTop diagnostics\n");
    out.push_str(&format!("  version    : {}\n", env!("CARGO_PKG_VERSION")));

    cpu_section(&mut out, sys, logical);
    gpu_section(&mut out, sys);
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

fn gpu_section(out: &mut String, sys: &impl crate::system::Sys) {
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
            opt(gpu.render_node.as_deref())
        ));
        if !gpu.outputs.is_empty() {
            out.push_str(&format!("       outputs  : {}\n", gpu.outputs.join(", ")));
        }
    }
    provider_section(out, &gpus);
}

/// One-shot provider sample for the `Auto` selection: shows which backend
/// the dispatch picks and which normalized metrics it exposes, without
/// running the TUI.
fn provider_section(out: &mut String, gpus: &[crate::discovery::DiscoveredGpu]) {
    let selector = GpuSelector::Auto;
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

    // PID of the local process behind the selected endpoint (PID-only
    // evidence — command lines are never read or printed).
    let pid = crate::discovery_llm::selected_endpoint_pid(&candidates, &endpoint);

    // Server→GPU mapping decision, exactly like the TUI computes it (S16).
    let gpus = discover_gpus(sys);
    let nvml = Nvml::init().ok();
    let mig_children = nvml.as_ref().map(discover_mig_children).unwrap_or_default();
    let mapping = match pid {
        Some(pid) => {
            let render = process_render_gpus(sys, pid, &gpus);
            let nvml_keys = nvml_compute_gpus(nvml.as_ref(), pid, &mig_children);
            map_server_gpus(true, render, nvml_keys, &gpus)
        }
        None => GpuMapping::None,
    };

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
            i, candidate.endpoint
        ));
    }
    out.push_str(&format!("  endpoint   : {endpoint} ({source_label})\n"));
    out.push_str(&format!("  gpu map    : {}\n", describe_mapping(&mapping)));
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
                let model = if stats.model.is_empty() {
                    "<unnamed>"
                } else {
                    &stats.model
                };
                println!("  status     : connected");
                println!("  model      : {model}");
                println!(
                    "  context    : {}/{} (watermark {})",
                    stats.context_used, stats.context_size, stats.context_high_watermark
                );
                println!(
                    "  slots      : {} busy / {} total",
                    stats.busy_slots, stats.slot_count
                );
                println!(
                    "  rate       : prompt {:.1} tps, generation {:.1} tps",
                    stats.prompt_tps, stats.generation_tps
                );
            } else {
                println!(
                    "  status     : unreachable ({})",
                    if stats.error.is_empty() {
                        "no error detail"
                    } else {
                        &stats.error
                    }
                );
            }
        }
        Err(err) => println!("  status     : client init failed ({err})"),
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
        let out = render(&fixture, 4, true, None);
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
        fixture.dir_entry("/proc/4242/fd", "7", false, false);
        fixture.symlink(
            "/proc/4242/fd/7",
            "/dev/dri/by-path/pci-0000:01:00.0-render",
        );

        let out = render(&fixture, 4, true, None);
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
        let out = render(&fixture, 4, true, None);
        assert!(out.contains("vendor     : Intel"), "missing vendor:\n{out}");
        assert!(out.contains("classes    : P=2 E=2 LP=0"), "{out}");
        assert!(out.contains("hybrid     : yes (P/E minibar)"), "{out}");
        assert!(out.contains("core kinds : P,P,E,E"), "{out}");
    }
}
