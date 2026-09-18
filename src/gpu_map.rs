//! Map the llama.cpp inference server process to the GPU(s) it actually uses.
//!
//! S16 determines device attribution from *evidence*, never by guessing. Two
//! independent, vendor-neutral evidence sources are combined:
//!
//! * **DRM render-node fds** — the process holds an open DRM render node
//!   (`/dev/dri/by-path/…` or `/dev/dri/renderD…`) that identifies a PCI BDF.
//!   This covers AMD/Intel/xe/i915 (and any DRM GPU). The full
//!   `/proc/<pid>/fdinfo` DRM-client telemetry (per-engine utilization +
//!   resident memory) is implemented in [`crate::drm`] (S8).
//! * **NVIDIA NVML compute processes** — NVML reports the process as a running
//!   compute client of a specific device
//!   (`nvmlDeviceGetComputeRunningProcesses`), authoritative for NVIDIA and
//!   also capturing MIG placements.
//!
//! The result is a [`GpuMapping`] with explicit states (one / multiple /
//! unknown) so a missing or ambiguous device is never faked.
//!
//! All filesystem access goes through [`Sys`] so the DRM path is
//! fixture-testable; the NVML path degrades to "no evidence" when NVML is
//! unavailable (non-NVIDIA hosts, no driver, permission).

use std::path::Path;

use nvml_wrapper::Nvml;

use crate::discovery::DiscoveredGpu;
use crate::domain::{normalize_pci_bdf, DeviceId, GpuEvidence, GpuMapping, GpuVendor, MappedGpu};
use crate::system::Sys;

/// Parse the PCI BDF embedded in a DRM **by-path** name
/// (`pci-0000:01:00.0-render`, `pci-0000:01:00.0-card`). The by-path name is
/// the stable, PCI-derived identity that `/proc/<pid>/fd` entries resolve to.
/// Returns `None` for non-PCI by-path names (`platform-…`) or malformed input.
fn bdf_from_by_path_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("pci-")?;
    let bdf = rest.split('-').next()?;
    let bytes = bdf.as_bytes();
    if bytes.len() != 12 {
        return None;
    }
    if !(0..4).all(|i| bytes[i].is_ascii_hexdigit())
        || bytes[4] != b':'
        || !(5..7).all(|i| bytes[i].is_ascii_hexdigit())
        || bytes[7] != b':'
        || !(8..10).all(|i| bytes[i].is_ascii_hexdigit())
        || bytes[10] != b'.'
        || !bytes[11].is_ascii_digit()
        || bytes[11] > b'7'
    {
        return None;
    }
    Some(bdf)
}

/// True when `key` is a PCI BDF of the form `dddd:bb:dd.f`.
fn is_bdf(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 12
        && (0..4).all(|i| bytes[i].is_ascii_hexdigit())
        && bytes[4] == b':'
        && (5..7).all(|i| bytes[i].is_ascii_hexdigit())
        && bytes[7] == b':'
        && (8..10).all(|i| bytes[i].is_ascii_hexdigit())
        && bytes[10] == b'.'
        && bytes[11].is_ascii_digit()
        && bytes[11] <= b'7'
}

/// PCI BDFs of the discovered GPUs that the process has an open DRM render
/// node for, derived from the process's open file descriptors.
///
/// A process that has allocated a DRM render node keeps it open for the life
/// of its context, so an open `/dev/dri/*` fd is solid evidence the process is
/// using that GPU. Each fd is an absolute path whose final component is
/// either a render-node name (`renderD…`) or a by-path name (`pci-<BDF>-…`);
/// both resolve to a PCI BDF that is matched against the discovered GPUs.
///
/// Returns deduped device ids (empty if the process has no open render node).
pub fn process_render_gpus<S: Sys>(sys: &S, pid: u32, gpus: &[DiscoveredGpu]) -> Vec<DeviceId> {
    let fd_dir = Path::new("/proc").join(pid.to_string()).join("fd");
    let Some(fd_entries) = sys.read_dir(&fd_dir) else {
        return Vec::new();
    };

    let mut result: Vec<DeviceId> = Vec::new();
    for entry in &fd_entries {
        let Some(target) = sys.symlink_target(&fd_dir.join(&entry.name)) else {
            continue;
        };
        let Some(component) = Path::new(target.trim())
            .components()
            .next_back()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
        else {
            continue;
        };

        // Resolve the fd's device node to a stable BDF key.
        let key = if component.starts_with("renderD") {
            // Stable render-node name → the discovered GPU that owns it.
            gpus.iter()
                .find(|g| g.render_node.as_deref() == Some(component.as_str()))
                .map(|g| g.device_id.key().to_string())
        } else {
            // by-path name → BDF embedded in the name.
            bdf_from_by_path_name(&component).map(str::to_string)
        };

        if let Some(key) = key {
            let Some(device) = gpus
                .iter()
                .find(|g| g.device_id.key() == key)
                .map(|g| g.device_id.clone())
            else {
                continue;
            };
            if !result.iter().any(|d| d.key() == device.key()) {
                result.push(device);
            }
        }
    }
    result
}

/// Stable keys (PCI BDF, else vendor UUID) of the NVML devices on which `pid`
/// is a running compute process.
///
/// Enumerates every NVML device once and asks each whether `pid` appears in
/// its `running_compute_processes()`. A CUDA process is reported on exactly
/// the device(s) it has allocated, so this is authoritative for NVIDIA.
/// Returns an empty vec when NVML is unavailable (non-NVIDIA host, no driver)
/// or on any per-device error — the caller treats that as "no NVML evidence".
pub fn nvml_compute_gpus(nvml: Option<&Nvml>, pid: u32) -> Vec<String> {
    let Some(nvml) = nvml else {
        return Vec::new();
    };
    let count = match nvml.device_count() {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let mut keys = Vec::new();
    for index in 0..count {
        let Some(device) = nvml.device_by_index(index).ok() else {
            continue;
        };
        let Some(procs) = device.running_compute_processes().ok() else {
            continue;
        };
        if procs.iter().any(|p| p.pid == pid) {
            // Prefer the PCI BDF (comparable with the DRM path), else UUID.
            if let Some(bdf) = device
                .pci_info()
                .ok()
                .map(|pci| normalize_pci_bdf(&pci.bus_id))
            {
                keys.push(bdf);
            } else if let Ok(uuid) = device.uuid() {
                keys.push(uuid);
            }
        }
    }
    keys
}

/// Combine render-node and NVML evidence into a [`GpuMapping`].
///
/// Union by stable key (BDF, else UUID). When both sources name the same
/// device, the render-node evidence wins (it is the more specific,
/// filesystem-derived signal). Dedup is by key; a device with no key is kept
/// as its own entry (never merged with another unkeyed device).
pub fn combine_gpus(
    render: Vec<DeviceId>,
    nvml: Vec<String>,
    gpus: &[DiscoveredGpu],
) -> GpuMapping {
    // (stable key, mapped device). Render nodes first so their evidence
    // takes precedence on conflict.
    let mut merged: Vec<(String, MappedGpu)> = Vec::new();

    let insert = |merged: &mut Vec<(String, MappedGpu)>,
                  key: String,
                  device: DeviceId,
                  evidence: GpuEvidence| {
        let discovered = gpus.iter().find(|g| g.device_id.key() == key);
        let name = discovered.map(|g| g.card.clone()).unwrap_or_default();
        let vendor = discovered.map(|g| g.vendor).unwrap_or(GpuVendor::Unknown);
        let mapped = MappedGpu {
            device,
            name,
            vendor,
            evidence,
        };
        match merged.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => {
                if evidence == GpuEvidence::RenderNodeFd {
                    slot.1.evidence = GpuEvidence::RenderNodeFd;
                }
            }
            None => merged.push((key, mapped)),
        }
    };

    for (i, device) in render.iter().enumerate() {
        let key = if device.key().is_empty() {
            // Unkeyed device: unique slot so it is not merged with another.
            format!("\u{0}render:{i}")
        } else {
            device.key().to_string()
        };
        insert(&mut merged, key, device.clone(), GpuEvidence::RenderNodeFd);
    }

    for key in &nvml {
        let device = if is_bdf(key) {
            DeviceId::new(Some(key.clone()), None)
        } else {
            DeviceId::new(None, Some(key.clone()))
        };
        insert(&mut merged, key.clone(), device, GpuEvidence::NvmlCompute);
    }

    merged.sort_by(|a, b| a.0.cmp(&b.0));
    let devices = merged.into_iter().map(|(_, m)| m).collect::<Vec<_>>();

    match devices.len() {
        0 => GpuMapping::None,
        1 => GpuMapping::Single(devices.into_iter().next().unwrap()),
        _ => GpuMapping::Multi(devices),
    }
}

/// Full mapping for a server process, given its evidence inputs.
///
/// Returns [`GpuMapping::Unknown`] when the process is not running locally
/// (`server_running` is false) or when both evidence sources come up empty
/// (the process is local but no GPU could be attributed to it). One device
/// yields `Single`; more than one yields `Multi`.
pub fn map_server_gpus(
    server_running: bool,
    render: Vec<DeviceId>,
    nvml: Vec<String>,
    gpus: &[DiscoveredGpu],
) -> GpuMapping {
    if !server_running {
        return GpuMapping::Unknown;
    }
    match combine_gpus(render, nvml, gpus) {
        GpuMapping::None => GpuMapping::Unknown,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    /// Add one DRM GPU: a PCI device, a render node in `/sys/class/drm`, a
    /// by-path entry and the device node, mirroring `discover_gpus` fixtures.
    fn add_gpu(sys: &mut FixtureSys, card: &str, bdf: &str, render: &str) {
        let pci = format!("/sys/devices/pci0000:00/{bdf}");
        sys.file(format!("{pci}/vendor").as_str(), "0x1002\n");
        sys.file(format!("{pci}/device").as_str(), "0x74a0\n");
        sys.file(format!("{pci}/class").as_str(), "0x030000\n");
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

    fn gpu_list(bdfs: &[&str]) -> Vec<DiscoveredGpu> {
        bdfs.iter()
            .enumerate()
            .map(|(i, bdf)| DiscoveredGpu {
                card: format!("card{i}"),
                device_id: DeviceId::new(Some(bdf.to_string()), None),
                vendor: GpuVendor::Amd,
                pci_vendor_id: 0x1002,
                pci_device_id: 0x74a0,
                pci_class_code: 0x030000,
                driver: "amdgpu".to_string(),
                render_node: Some(format!("renderD{}", 128 + i)),
                outputs: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn render_fd_maps_process_to_single_gpu() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let gpus = gpu_list(&["0000:01:00.0"]);

        // The process holds an open fd to the by-path render node.
        sys.dir_entry("/proc/4242/fd", "7", false, false);
        sys.symlink(
            "/proc/4242/fd/7",
            "/dev/dri/by-path/pci-0000:01:00.0-render",
        );

        let render = process_render_gpus(&sys, 4242, &gpus);
        assert_eq!(
            render,
            vec![DeviceId::new(Some("0000:01:00.0".into()), None)]
        );

        let mapping = map_server_gpus(true, render, vec![], &gpus);
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "0000:01:00.0");
        assert_eq!(device.evidence, GpuEvidence::RenderNodeFd);
        assert_eq!(device.name, "card0");
        assert_eq!(device.vendor, GpuVendor::Amd);
    }

    #[test]
    fn render_fd_by_render_node_name_maps_to_gpu() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let gpus = gpu_list(&["0000:01:00.0"]);

        // fd target is the stable render-node device node itself.
        sys.dir_entry("/proc/4242/fd", "7", false, false);
        sys.symlink("/proc/4242/fd/7", "/dev/dri/renderD128");

        let render = process_render_gpus(&sys, 4242, &gpus);
        assert_eq!(
            render,
            vec![DeviceId::new(Some("0000:01:00.0".into()), None)]
        );
    }

    #[test]
    fn no_open_render_fd_yields_no_evidence() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let gpus = gpu_list(&["0000:01:00.0"]);

        // The process has fds, but none point at a DRM render node.
        sys.dir_entry("/proc/4242/fd", "0", false, false);
        sys.symlink("/proc/4242/fd/0", "/dev/null");

        let render = process_render_gpus(&sys, 4242, &gpus);
        assert!(render.is_empty());
        assert_eq!(
            map_server_gpus(true, render, vec![], &gpus),
            GpuMapping::Unknown
        );
    }

    #[test]
    fn process_without_fd_dir_is_unknown() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let gpus = gpu_list(&["0000:01:00.0"]);

        // No /proc/<pid>/fd directory at all (process gone or unreadable).
        let render = process_render_gpus(&sys, 9999, &gpus);
        assert!(render.is_empty());
        assert_eq!(
            map_server_gpus(true, render, vec![], &gpus),
            GpuMapping::Unknown
        );
    }

    #[test]
    fn server_not_running_is_unknown_even_with_evidence() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let gpus = gpu_list(&["0000:01:00.0"]);
        sys.dir_entry("/proc/4242/fd", "7", false, false);
        sys.symlink(
            "/proc/4242/fd/7",
            "/dev/dri/by-path/pci-0000:01:00.0-render",
        );
        let render = process_render_gpus(&sys, 4242, &gpus);
        assert_eq!(
            map_server_gpus(false, render, vec![], &gpus),
            GpuMapping::Unknown
        );
    }

    #[test]
    fn multi_gpu_when_process_opens_two_render_nodes() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        add_gpu(&mut sys, "card1", "0000:02:00.0", "renderD129");
        let gpus = gpu_list(&["0000:01:00.0", "0000:02:00.0"]);

        sys.dir_entry("/proc/4242/fd", "7", false, false);
        sys.symlink(
            "/proc/4242/fd/7",
            "/dev/dri/by-path/pci-0000:01:00.0-render",
        );
        sys.dir_entry("/proc/4242/fd", "8", false, false);
        sys.symlink(
            "/proc/4242/fd/8",
            "/dev/dri/by-path/pci-0000:02:00.0-render",
        );

        let render = process_render_gpus(&sys, 4242, &gpus);
        assert_eq!(render.len(), 2);
        let mapping = map_server_gpus(true, render, vec![], &gpus);
        let GpuMapping::Multi(devices) = mapping else {
            panic!("expected Multi, got {mapping:?}");
        };
        assert_eq!(devices.len(), 2);
        // Sorted by BDF.
        assert_eq!(devices[0].key(), "0000:01:00.0");
        assert_eq!(devices[1].key(), "0000:02:00.0");
    }

    #[test]
    fn nvml_evidence_alone_maps_single_gpu() {
        let gpus = gpu_list(&["0000:41:00.0"]);
        // No render fd evidence, but NVML reports the pid on that BDF.
        let mapping = map_server_gpus(true, vec![], vec!["0000:41:00.0".into()], &gpus);
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "0000:41:00.0");
        assert_eq!(device.evidence, GpuEvidence::NvmlCompute);
    }

    #[test]
    fn nvml_eight_digit_bdf_unifies_with_discovered_device() {
        let gpus = gpu_list(&["0000:41:00.0"]);
        // NVML reports the 8-digit domain form; ingress normalization
        // (nvml_compute_gpus) yields the canonical key before combining.
        let nvml = vec![normalize_pci_bdf("00000000:41:00.0")];
        let mapping = map_server_gpus(true, vec![], nvml, &gpus);
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "0000:41:00.0");
        assert_eq!(device.evidence, GpuEvidence::NvmlCompute);
        // Name and vendor resolve from the discovered device.
        assert_eq!(device.name, "card0");
        assert_eq!(device.vendor, GpuVendor::Amd);
    }

    #[test]
    fn nvml_uuid_evidence_maps_single_gpu() {
        let gpus = Vec::new();
        // NVML-only device (not DRM-discovered) keyed by UUID.
        let mapping = map_server_gpus(true, vec![], vec!["GPU-abc123".into()], &gpus);
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "GPU-abc123");
        assert_eq!(device.evidence, GpuEvidence::NvmlCompute);
        assert_eq!(device.vendor, GpuVendor::Unknown);
    }

    #[test]
    fn render_and_nvml_agree_on_same_device() {
        let gpus = gpu_list(&["0000:01:00.0"]);
        let render = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
        let nvml = vec!["0000:01:00.0".to_string()];
        let mapping = map_server_gpus(true, render, nvml, &gpus);
        // Deduped to a single device; render evidence wins.
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "0000:01:00.0");
        assert_eq!(device.evidence, GpuEvidence::RenderNodeFd);
    }

    #[test]
    fn nvml_multi_device_yields_multi() {
        let gpus = gpu_list(&["0000:41:00.0", "0000:42:00.0"]);
        let nvml = vec!["0000:41:00.0".into(), "0000:42:00.0".into()];
        let mapping = map_server_gpus(true, vec![], nvml, &gpus);
        let GpuMapping::Multi(devices) = mapping else {
            panic!("expected Multi, got {mapping:?}");
        };
        assert_eq!(devices.len(), 2);
    }

    #[test]
    fn combine_dedupes_by_stable_key() {
        let gpus = gpu_list(&["0000:01:00.0"]);
        let render = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
        let nvml = vec!["0000:01:00.0".to_string()];
        let combined = combine_gpus(render, nvml, &gpus);
        assert_eq!(combined.len(), Some(1));
    }

    #[test]
    fn bdf_parser_rejects_malformed_names() {
        assert_eq!(
            bdf_from_by_path_name("pci-0000:01:00.0-render"),
            Some("0000:01:00.0")
        );
        assert_eq!(
            bdf_from_by_path_name("pci-0000:01:00.0-card"),
            Some("0000:01:00.0")
        );
        // Non-PCI by-path name.
        assert_eq!(bdf_from_by_path_name("platform-fd500000.gpu"), None);
        // Wrong length / bad separators.
        assert_eq!(bdf_from_by_path_name("pci-0:1.0-render"), None);
        assert_eq!(bdf_from_by_path_name("pci-0000:01:00.8-render"), None);
        // No pci- prefix.
        assert_eq!(bdf_from_by_path_name("renderD128"), None);
    }
}
