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
//!   (`nvmlDeviceGetComputeRunningProcesses`), authoritative for NVIDIA,
//!   including per-instance MIG placements.
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
use crate::providers::nvidia::MigChild;
use crate::system::Sys;

/// Legacy by-path grammar regression helper; names are no longer used as
/// attribution evidence. Parse the PCI BDF embedded in a DRM **by-path** name
/// (`pci-0000:01:00.0-render`, `pci-0000:01:00.0-card`). The by-path name is
/// the stable, PCI-derived identity that `/proc/<pid>/fd` entries resolve to.
/// Returns `None` for non-PCI by-path names (`platform-…`) or malformed input.
/// An 8-digit PCI domain is accepted and canonicalized to the 4-digit form
/// via [`normalize_pci_bdf`] (domains ≥ `0x10000` stay 8-digit, like NVML
/// ingress).
#[cfg(test)]
fn bdf_from_by_path_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("pci-")?;
    let bdf = rest.split('-').next()?;
    crate::domain::parse_pci_bdf(bdf)
}

/// PCI BDFs of the discovered GPUs that the process has an open DRM render
/// node for, derived from the process's open file descriptors.
///
/// Match the character-device number of each open fd against the discovered
/// render nodes. This follows aliases and rejects regular files, even when
/// they are named `renderD128` or `pci-<BDF>-render`. An unreadable device
/// yields no evidence; filenames and BDF-shaped strings are never sufficient.
///
/// Returns deduped device ids (empty if the process has no open render node).
pub fn process_render_gpus<S: Sys>(sys: &S, pid: u32, gpus: &[DiscoveredGpu]) -> Vec<DeviceId> {
    let render_devices: Vec<_> = gpus
        .iter()
        .flat_map(|gpu| {
            gpu.render_nodes.iter().filter_map(move |node| {
                sys.char_device_id(&Path::new("/dev/dri").join(node))
                    .map(|id| (id, &gpu.device_id))
            })
        })
        .collect();
    if render_devices.is_empty() {
        return Vec::new();
    }
    let fd_dir = Path::new("/proc").join(pid.to_string()).join("fd");
    let Some(fd_names) = sys.read_dir_names(&fd_dir) else {
        return Vec::new();
    };

    let mut result: Vec<DeviceId> = Vec::new();
    for name in fd_names {
        let Some(id) = sys.char_device_id(&fd_dir.join(name)) else {
            continue;
        };
        if let Some((_, device)) = render_devices
            .iter()
            .find(|(render_id, _)| *render_id == id)
        {
            if !result.iter().any(|d| d.key() == device.key()) {
                result.push((*device).clone());
            }
        }
    }
    result
}

/// Stable identities of the NVML devices on which `pid` is a running
/// compute process — one per NVML placement of the process.
///
/// Enumerates every NVML device once and asks each whether `pid` appears in
/// its `running_compute_processes()`. A CUDA process is reported on exactly
/// the device(s) it has allocated, so this is authoritative for NVIDIA.
///
/// MIG: NVML reports a GPU/compute instance id per placement. When the
/// placement carries instance ids and a MIG child's identity carries the
/// same ids, the placement is attributed to that child; every other
/// placement (MIG disabled, no instance evidence, or no matching child) is
/// attributed to the physical parent. One placement yields exactly one
/// identity, so a process is never mapped to both a MIG child and its
/// parent.
///
/// `mig_children` is the MIG topology captured once at startup
/// ([`crate::providers::nvidia::discover_mig_children`]); the per-scan path
/// never re-probes it, so no static NVML call rides the refresh cadence.
///
/// Returns an empty vec when NVML is unavailable (non-NVIDIA host, no
/// driver) or on any per-device error — the caller treats that as "no NVML
/// evidence".
pub fn nvml_compute_gpus(
    nvml: Option<&Nvml>,
    pid: u32,
    mig_children: &[MigChild],
) -> Vec<DeviceId> {
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
        let entries: Vec<(Option<u32>, Option<u32>)> = procs
            .iter()
            .filter(|p| p.pid == pid)
            .map(|p| (p.gpu_instance_id, p.compute_instance_id))
            .collect();
        if entries.is_empty() {
            continue;
        }
        let parent = DeviceId::new(
            device
                .pci_info()
                .ok()
                .map(|pci| normalize_pci_bdf(&pci.bus_id))
                .filter(|id| !id.is_empty()),
            device.uuid().ok().filter(|u| !u.is_empty()),
        );
        if parent.key().is_empty() {
            continue;
        }
        let children: Vec<(String, Option<(u32, u32)>)> =
            if entries.iter().any(|(gi, ci)| gi.is_some() || ci.is_some()) {
                mig_children
                    .iter()
                    .filter(|c| c.parent_index == index)
                    .map(|c| (c.key.clone(), c.instance()))
                    .collect()
            } else {
                Vec::new()
            };
        keys.extend(attribute_process_keys(&parent, &children, &entries));
    }
    keys
}

/// Attribute each NVML placement of one process on one physical device to a
/// stable identity. A placement whose MIG instance ids match a child's
/// identity yields that child's identity; any other placement yields the
/// parent's identity.
fn attribute_process_keys(
    parent: &DeviceId,
    children: &[(String, Option<(u32, u32)>)],
    entries: &[(Option<u32>, Option<u32>)],
) -> Vec<DeviceId> {
    entries
        .iter()
        .map(|entry| match *entry {
            (Some(gi), Some(ci)) => {
                let instance = Some((gi, ci));
                children
                    .iter()
                    .find(|(_, child_instance)| *child_instance == instance)
                    .map(|(key, _)| DeviceId::new(None, Some(key.clone())))
                    .unwrap_or_else(|| parent.clone())
            }
            _ => parent.clone(),
        })
        .collect()
}

/// Combine render-node and NVML evidence into a [`GpuMapping`].
///
/// Union by stable key (BDF, else UUID). When both sources name the same
/// device, the render-node evidence wins (it is the more specific,
/// filesystem-derived signal). Dedup is by key; a device with no key is kept
/// as its own entry (never merged with another unkeyed device).
pub fn combine_gpus(
    render: Vec<DeviceId>,
    nvml: Vec<DeviceId>,
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

    for device in &nvml {
        let key = device.key().to_string();
        if key.is_empty() {
            continue;
        }
        insert(&mut merged, key, device.clone(), GpuEvidence::NvmlCompute);
    }

    merged.sort_by(|a, b| a.0.cmp(&b.0));
    let devices = merged.into_iter().map(|(_, m)| m).collect::<Vec<_>>();

    match devices.len() {
        0 => GpuMapping::None,
        1 => match devices.into_iter().next() {
            Some(device) => GpuMapping::Single(device),
            None => GpuMapping::None,
        },
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
    nvml: Vec<DeviceId>,
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
        sys.char_device(
            &format!("/dev/dri/{render}"),
            render.trim_start_matches("renderD").parse().unwrap(),
        );
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
                render_nodes: vec![format!("renderD{}", 128 + i)],
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
    fn render_fd_on_secondary_render_node_maps_to_same_gpu() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        let mut gpus = gpu_list(&["0000:01:00.0"]);
        // The iGPU also exposes a compute-only render node (i915/xe shape).
        gpus[0].render_nodes.push("renderD129".to_string());
        sys.char_device("/dev/dri/renderD129", 129);

        // The process holds an open fd to the compute node, not renderD128.
        sys.dir_entry("/proc/4242/fd", "7", false, false);
        sys.symlink("/proc/4242/fd/7", "/dev/dri/renderD129");

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
    fn render_mapping_requires_a_matching_character_device_not_a_filename() {
        for target in [
            "/tmp/not-a-device/renderD128",
            "/tmp/pci-0000:01:00.0-render",
            "/tmp/pci-0000:01:00.0-card",
            "/dev/dri/renderD999",
        ] {
            let mut sys = FixtureSys::default();
            add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
            sys.dir_entry("/proc/42/fd", "7", false, true);
            sys.symlink("/proc/42/fd/7", target);
            sys.file(target, "regular file");
            let gpus = gpu_list(&["0000:01:00.0"]);
            assert!(process_render_gpus(&sys, 42, &gpus).is_empty(), "{target}");
            // Even a real character device must have the right device number.
            sys.char_device(target, 999);
            assert!(process_render_gpus(&sys, 42, &gpus).is_empty(), "{target}");
        }
    }

    #[test]
    fn render_mapping_accepts_device_aliases_and_deduplicates_fds() {
        let mut sys = FixtureSys::default();
        add_gpu(&mut sys, "card0", "0000:01:00.0", "renderD128");
        sys.char_device("/custom/gpu-alias", 128);
        for fd in ["7", "8"] {
            sys.dir_entry("/proc/42/fd", fd, false, true);
            sys.symlink(&format!("/proc/42/fd/{fd}"), "/custom/gpu-alias");
        }
        let gpus = gpu_list(&["0000:01:00.0"]);
        assert_eq!(
            process_render_gpus(&sys, 42, &gpus),
            vec![gpus[0].device_id.clone()]
        );
    }

    #[test]
    fn unreadable_render_device_never_falls_back_to_its_name() {
        let mut sys = FixtureSys::default();
        sys.dir_entry("/proc/42/fd", "7", false, true);
        sys.symlink("/proc/42/fd/7", "/dev/dri/renderD128");
        // fd itself is readable but the canonical device cannot be verified.
        sys.char_device("/proc/42/fd/7", 128);
        assert!(process_render_gpus(&sys, 42, &gpu_list(&["0000:01:00.0"])).is_empty());
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
        let mapping = map_server_gpus(
            true,
            vec![],
            vec![DeviceId::new(Some("0000:41:00.0".into()), None)],
            &gpus,
        );
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
        let nvml = vec![DeviceId::new(
            Some(normalize_pci_bdf("00000000:41:00.0")),
            None,
        )];
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
        let mapping = map_server_gpus(
            true,
            vec![],
            vec![DeviceId::new(None, Some("GPU-abc123".into()))],
            &gpus,
        );
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
        let nvml = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
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
        let nvml = vec![
            DeviceId::new(Some("0000:41:00.0".into()), None),
            DeviceId::new(Some("0000:42:00.0".into()), None),
        ];
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
        let nvml = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
        let combined = combine_gpus(render, nvml, &gpus);
        assert_eq!(combined.len(), Some(1));
    }

    #[test]
    fn bdf_parser_rejects_malformed_names() {
        assert_eq!(
            bdf_from_by_path_name("pci-0000:01:00.0-render"),
            Some("0000:01:00.0".to_string())
        );
        assert_eq!(
            bdf_from_by_path_name("pci-0000:01:00.0-card"),
            Some("0000:01:00.0".to_string())
        );
        // Non-PCI by-path name.
        assert_eq!(bdf_from_by_path_name("platform-fd500000.gpu"), None);
        // Wrong length / bad separators.
        assert_eq!(bdf_from_by_path_name("pci-0:1.0-render"), None);
        assert_eq!(bdf_from_by_path_name("pci-0000:01:00.8-render"), None);
        // No pci- prefix.
        assert_eq!(bdf_from_by_path_name("renderD128"), None);
    }

    #[test]
    fn bdf_parser_accepts_eight_digit_domains() {
        // The default domain reported in 8-digit form canonicalizes to the
        // 4-digit form used by discovery; domains ≥ 0x10000 stay 8-digit.
        assert_eq!(
            bdf_from_by_path_name("pci-00000000:01:00.0-render"),
            Some("0000:01:00.0".to_string())
        );
        assert_eq!(
            bdf_from_by_path_name("pci-00010000:41:00.0-render"),
            Some("00010000:41:00.0".to_string())
        );
        // 8-digit form with a bad function digit is rejected.
        assert_eq!(bdf_from_by_path_name("pci-00000000:01:00.8-render"), None);
    }

    #[test]
    fn attribute_mig_child_when_instance_evidence_matches() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let children = vec![("MIG-GPU-parent/1/0".to_string(), Some((1, 0)))];
        let keys = attribute_process_keys(&parent, &children, &[(Some(1), Some(0))]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "MIG-GPU-parent/1/0");
    }

    #[test]
    fn attribute_degrades_to_parent_when_no_child_matches() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let children = vec![("MIG-GPU-parent/1/0".to_string(), Some((1, 0)))];
        let keys = attribute_process_keys(&parent, &children, &[(Some(2), Some(0))]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "0000:41:00.0");
    }

    #[test]
    fn opaque_mig_child_without_instance_evidence_never_matches() {
        // A modern opaque MIG child carries no GI/CI, so a placement with
        // instance ids must not be attributed to it (it degrades to parent).
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let children = vec![("MIG-1a2b3c4d5e6f7890abcdef0123456789".to_string(), None)];
        let keys = attribute_process_keys(&parent, &children, &[(Some(1), Some(0))]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "0000:41:00.0");
    }

    #[test]
    fn attribute_non_mig_placement_uses_parent() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let keys = attribute_process_keys(&parent, &[], &[(None, None)]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), "0000:41:00.0");
    }

    #[test]
    fn attribute_multiple_mig_placements_yield_each_child() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let children = vec![
            ("MIG-GPU-parent/1/0".to_string(), Some((1, 0))),
            ("MIG-GPU-parent/2/0".to_string(), Some((2, 0))),
        ];
        let keys = attribute_process_keys(
            &parent,
            &children,
            &[(Some(1), Some(0)), (Some(2), Some(0))],
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key(), "MIG-GPU-parent/1/0");
        assert_eq!(keys[1].key(), "MIG-GPU-parent/2/0");
    }

    #[test]
    fn mig_child_evidence_maps_to_single_child() {
        let gpus = gpu_list(&["0000:41:00.0"]);
        let nvml = vec![DeviceId::new(None, Some("MIG-GPU-parent/1/0".into()))];
        let mapping = map_server_gpus(true, vec![], nvml, &gpus);
        let GpuMapping::Single(device) = mapping else {
            panic!("expected Single, got {mapping:?}");
        };
        assert_eq!(device.key(), "MIG-GPU-parent/1/0");
        assert_eq!(device.evidence, GpuEvidence::NvmlCompute);
        // MIG children are not DRM-discovered: name stays empty, vendor unknown.
        assert_eq!(device.name, "");
        assert_eq!(device.vendor, GpuVendor::Unknown);
    }

    #[test]
    fn combine_mig_child_and_parent_are_distinct_devices() {
        let gpus = gpu_list(&["0000:41:00.0"]);
        // A process cannot occupy a MIG child and its parent at once; the
        // combiner must still keep both identities distinct.
        let nvml = vec![
            DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into())),
            DeviceId::new(None, Some("MIG-GPU-parent/1/0".into())),
        ];
        let mapping = map_server_gpus(true, vec![], nvml, &gpus);
        let GpuMapping::Multi(devices) = mapping else {
            panic!("expected Multi, got {mapping:?}");
        };
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].key(), "0000:41:00.0");
        assert_eq!(devices[1].key(), "MIG-GPU-parent/1/0");
    }
}
