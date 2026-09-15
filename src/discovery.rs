//! Vendor-neutral GPU discovery from Linux DRM sysfs.
//!
//! Every DRM card is discovered from `/sys/class/drm` without assuming a
//! vendor: the card's symlink target carries the PCI BDF, and the PCI
//! device attributes (vendor/device IDs, class, driver binding) classify it.
//! Enumeration order (`card0`, `card1`, …) is never used as identity; the
//! PCI BDF is the stable key, and results are sorted by BDF.

use std::path::{Path, PathBuf};

use crate::domain::DeviceId;
use crate::system::Sys;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Other,
    Unknown,
}

/// A GPU discovered through DRM sysfs, before any vendor telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredGpu {
    /// DRM card name (e.g. `card0`) — for display only, not identity.
    pub card: String,
    pub device_id: DeviceId,
    pub vendor: GpuVendor,
    pub pci_vendor_id: u16,
    pub pci_device_id: u16,
    pub pci_class_code: u32,
    /// Bound driver name (`nvidia`, `amdgpu`, `i915`, `xe`, …) or empty.
    pub driver: String,
    /// Render node name (`renderD128`) when the card exposes one.
    pub render_node: Option<String>,
    /// Connector names (`card0-DP-1`, …) attached to this card.
    pub outputs: Vec<String>,
}

fn is_display_class(class: u32) -> bool {
    // PCI base class 0x03 (Display controller) is in bits 23..=16.
    (class >> 16) & 0xff == 0x03
}

/// Parse and validate a PCI BDF like `0000:01:00.0` (no external regex).
fn parse_bdf(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.len() != 12 {
        return None;
    }
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    if !(0..4).all(|i| is_hex(bytes[i]))
        || bytes[4] != b':'
        || !(5..7).all(|i| is_hex(bytes[i]))
        || bytes[7] != b':'
        || !(8..10).all(|i| is_hex(bytes[i]))
        || bytes[10] != b'.'
        || !bytes[11].is_ascii_digit()
        || bytes[11] > b'7'
    {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

/// The PCI BDF is the final component of the PCI device directory, with a
/// fallback scan for platforms that nest the device deeper in the path.
fn bdf_from_path(path: &Path) -> Option<String> {
    let components = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for component in components.iter().rev() {
        if let Some(bdf) = parse_bdf(component) {
            return Some(bdf);
        }
    }
    None
}

fn vendor_from_pci_id(id: u16) -> GpuVendor {
    match id {
        0x10de => GpuVendor::Nvidia,
        0x1002 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        _ if id != 0 => GpuVendor::Other,
        _ => GpuVendor::Unknown,
    }
}

/// Resolve a (possibly relative) symlink target against the directory that
/// contains the link. Handles `..` segments without touching the filesystem;
/// `..` above the root clamps to `/`, like the kernel.
fn resolve_link(base: &Path, target: &str) -> Option<PathBuf> {
    let target_path = Path::new(target);
    let mut parts: Vec<std::ffi::OsString> = if target_path.is_absolute() {
        Vec::new()
    } else {
        base.components()
            .map(|component| component.as_os_str().to_os_string())
            .collect()
    };
    for component in target_path.components() {
        match component {
            std::path::Component::ParentDir => {
                if parts.last().is_some_and(|p| p.as_encoded_bytes() != b"/") {
                    parts.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    Some(parts.into_iter().collect())
}

fn parse_u16_hex(text: &str) -> Option<u16> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").unwrap_or(text);
    u16::from_str_radix(digits.trim(), 16).ok()
}

fn parse_u32_hex(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").unwrap_or(text);
    u32::from_str_radix(digits.trim(), 16).ok()
}

fn read_hex<S: Sys>(sys: &S, path: &Path, parse: fn(&str) -> Option<u32>) -> Option<u32> {
    let text = sys.read_to_string(path)?;
    parse(&text)
}

pub fn discover_gpus<S: Sys>(sys: &S) -> Vec<DiscoveredGpu> {
    let drm_root = PathBuf::from("/sys/class/drm");
    let Some(entries) = sys.read_dir(&drm_root) else {
        return Vec::new();
    };

    let mut cards: Vec<String> = Vec::new();
    let mut outputs: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut render_nodes: Vec<(String, String)> = Vec::new(); // (name, target)

    for entry in entries {
        if !entry.is_symlink {
            continue;
        }
        let Some(target) = sys.symlink_target(&drm_root.join(&entry.name)) else {
            continue;
        };
        if entry.name.starts_with("card") && entry.name.contains('-') {
            let card = entry.name.split('-').next().unwrap_or_default().to_string();
            outputs.entry(card).or_default().push(entry.name);
        } else if entry.name.starts_with("renderD") {
            render_nodes.push((entry.name, target));
        } else if entry.name.starts_with("card") {
            cards.push(entry.name.clone());
        }
    }

    let mut discovered = Vec::new();
    for card in cards {
        let card_path = drm_root.join(&card);
        let Some(target) = sys.symlink_target(&card_path) else {
            continue;
        };
        let Some(resolved) = resolve_link(drm_root.as_path(), &target) else {
            continue;
        };
        let Some(drm_dir) = resolved.parent() else {
            continue;
        };
        let Some(pci_dir) = drm_dir.parent() else {
            continue;
        };
        let Some(bdf) = bdf_from_path(pci_dir) else {
            continue;
        };

        let pci_vendor = read_hex(sys, &pci_dir.join("vendor"), |t| {
            parse_u16_hex(t).map(|v| v as u32)
        });
        let pci_device = read_hex(sys, &pci_dir.join("device"), |t| {
            parse_u16_hex(t).map(|v| v as u32)
        });
        let class = read_hex(sys, &pci_dir.join("class"), parse_u32_hex);

        // Non-display PCI devices are not GPUs (e.g. some virtual functions).
        match class {
            Some(value) if !is_display_class(value) => continue,
            Some(_) | None => {}
        }

        let driver = sys
            .symlink_target(&pci_dir.join("driver"))
            .and_then(|path| {
                Path::new(&path)
                    .components()
                    .next_back()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_default();

        let vendor = match driver.as_str() {
            "nvidia" => GpuVendor::Nvidia,
            "amdgpu" => GpuVendor::Amd,
            "i915" | "xe" => GpuVendor::Intel,
            _ => pci_vendor
                .and_then(|id| {
                    let id = id as u16;
                    (id != 0).then_some(vendor_from_pci_id(id))
                })
                .unwrap_or(GpuVendor::Unknown),
        };

        let render_node = render_nodes
            .iter()
            .find(|(_, target)| {
                resolve_link(drm_root.as_path(), target)
                    .is_some_and(|path| path.starts_with(pci_dir))
            })
            .map(|(name, _)| name.clone());

        discovered.push(DiscoveredGpu {
            card: card.clone(),
            device_id: DeviceId::new(Some(bdf), None),
            vendor,
            pci_vendor_id: pci_vendor.unwrap_or(0) as u16,
            pci_device_id: pci_device.unwrap_or(0) as u16,
            pci_class_code: class.unwrap_or(0),
            driver,
            render_node,
            outputs: outputs.remove(&card).unwrap_or_default(),
        });
    }

    // Deterministic order: PCI BDF, not enumeration order.
    discovered.sort_by(|a, b| a.device_id.key().cmp(b.device_id.key()));
    discovered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    /// Add one DRM card with its PCI device, driver binding and render node.
    fn add_card(
        sys: &mut FixtureSys,
        card: &str,
        bdf: &str,
        vendor_id: &str,
        device_id: &str,
        driver: &str,
    ) {
        let pci = format!("/sys/devices/pci0000:00/{bdf}");
        sys.file(format!("{pci}/vendor").as_str(), format!("{vendor_id}\n"));
        sys.file(format!("{pci}/device").as_str(), format!("{device_id}\n"));
        sys.file(format!("{pci}/class").as_str(), "0x030000\n");
        if !driver.is_empty() {
            sys.symlink(
                format!("{pci}/driver").as_str(),
                &format!("../../../bus/pci/drivers/{driver}"),
            );
            sys.symlink(
                "/sys/class/drm/renderD128",
                &format!("../../devices/pci0000:00/{bdf}/drm/renderD128"),
            );
            sys.dir_entry("/sys/class/drm", "renderD128", false, true);
        }
        sys.dir_entry("/sys/class/drm", card, false, true);
        sys.symlink(
            format!("/sys/class/drm/{card}").as_str(),
            &format!("../../devices/pci0000:00/{bdf}/drm/{card}"),
        );
    }

    fn add_connector(sys: &mut FixtureSys, name: &str, bdf: &str) {
        sys.dir_entry("/sys/class/drm", name, false, true);
        sys.symlink(
            format!("/sys/class/drm/{name}").as_str(),
            &format!("../../devices/pci0000:00/{bdf}/drm/{name}"),
        );
    }

    #[test]
    fn discovers_single_nvidia_card() {
        let mut sys = FixtureSys::default();
        add_card(
            &mut sys,
            "card0",
            "0000:01:00.0",
            "0x10de",
            "0x2684",
            "nvidia",
        );
        add_connector(&mut sys, "card0-DP-1", "0000:01:00.0");

        let gpus = discover_gpus(&sys);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.card, "card0");
        assert_eq!(gpu.device_id.key(), "0000:01:00.0");
        assert_eq!(gpu.vendor, GpuVendor::Nvidia);
        assert_eq!(gpu.pci_vendor_id, 0x10de);
        assert_eq!(gpu.pci_device_id, 0x2684);
        assert_eq!(gpu.pci_class_code, 0x030000);
        assert_eq!(gpu.driver, "nvidia");
        assert_eq!(gpu.render_node.as_deref(), Some("renderD128"));
        assert_eq!(gpu.outputs, vec!["card0-DP-1".to_string()]);
    }

    #[test]
    fn discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf() {
        let mut sys = FixtureSys::default();
        add_card(
            &mut sys,
            "card0",
            "0000:65:00.0",
            "0x1002",
            "0x74a0",
            "amdgpu",
        );
        add_card(&mut sys, "card1", "0000:00:02.0", "0x8086", "0x64a0", "xe");
        add_connector(&mut sys, "card1-eDP-1", "0000:00:02.0");

        let gpus = discover_gpus(&sys);
        // BDF sort: the iGPU (00:02.0) comes first even though it is card1.
        assert_eq!(
            gpus.iter()
                .map(|gpu| gpu.device_id.key().to_string())
                .collect::<Vec<_>>(),
            vec!["0000:00:02.0", "0000:65:00.0"]
        );
        assert_eq!(gpus[0].vendor, GpuVendor::Intel);
        assert_eq!(gpus[0].pci_vendor_id, 0x8086);
        assert_eq!(gpus[0].driver, "xe");
        assert_eq!(gpus[0].outputs, vec!["card1-eDP-1".to_string()]);
        assert_eq!(gpus[1].vendor, GpuVendor::Amd);
        assert_eq!(gpus[1].driver, "amdgpu");
    }

    #[test]
    fn vendor_falls_back_to_pci_id_when_driver_missing() {
        let mut sys = FixtureSys::default();
        // Card with a PCI vendor ID but no driver binding at all.
        let pci = "/sys/devices/pci0000:00/0000:01:00.0";
        sys.file(format!("{pci}/vendor").as_str(), "0x10de\n");
        sys.file(format!("{pci}/device").as_str(), "0x2684\n");
        sys.file(format!("{pci}/class").as_str(), "0x030000\n");
        sys.dir_entry("/sys/class/drm", "card0", false, true);
        sys.symlink(
            "/sys/class/drm/card0",
            "../../devices/pci0000:00/0000:01:00.0/drm/card0",
        );

        let gpus = discover_gpus(&sys);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor, GpuVendor::Nvidia);
        assert_eq!(gpus[0].pci_vendor_id, 0x10de);
        assert_eq!(gpus[0].driver, "");
    }

    #[test]
    fn ignores_non_display_pci_devices() {
        let mut sys = FixtureSys::default();
        // Network card that somehow ended up in /sys/class/drm: class 0x020000.
        add_card(&mut sys, "card0", "0000:02:00.0", "0x8086", "0x1267", "igb");
        sys.file(
            "/sys/devices/pci0000:00/0000:02:00.0/class"
                .to_string()
                .as_str(),
            "0x020000\n",
        );

        assert!(discover_gpus(&sys).is_empty());
    }

    #[test]
    fn headless_render_only_card_is_still_discovered() {
        let mut sys = FixtureSys::default();
        add_card(
            &mut sys,
            "card0",
            "0000:41:00.0",
            "0x10de",
            "0x26b3",
            "nvidia",
        );
        // No connectors at all — compute-only headless card.

        let gpus = discover_gpus(&sys);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].outputs.is_empty());
        assert_eq!(gpus[0].render_node.as_deref(), Some("renderD128"));
    }

    #[test]
    fn parse_bdf_accepts_valid_and_rejects_garbage() {
        assert_eq!(parse_bdf("0000:01:00.0"), Some("0000:01:00.0".to_string()));
        assert_eq!(parse_bdf("0000:01:00.8"), None);
        assert_eq!(parse_bdf("0000:01:00"), None);
        assert_eq!(parse_bdf("0000-01-00-0"), None);
        assert_eq!(parse_bdf("zzzz:01:00.0"), None);
    }

    #[test]
    fn resolve_link_handles_relative_and_parent_segments() {
        let base = PathBuf::from("/sys/class/drm");
        assert_eq!(
            resolve_link(&base, "../../devices/pci0000:00/0000:00:02.0/drm/card0").as_deref(),
            Some(Path::new("/sys/devices/pci0000:00/0000:00:02.0/drm/card0"))
        );
        assert_eq!(
            resolve_link(&base, "/abs/path").as_deref(),
            Some(Path::new("/abs/path"))
        );
        // `..` above the root clamps to `/`, like the kernel.
        assert_eq!(
            resolve_link(&base, "../../..").as_deref(),
            Some(Path::new("/"))
        );
        assert_eq!(
            resolve_link(Path::new("/a/b"), "../../..").as_deref(),
            Some(Path::new("/"))
        );
    }

    /// Smoke test against the real machine (run with
    /// `cargo test -- --ignored discovery_real`). Not part of CI.
    #[test]
    #[ignore]
    fn discovery_real_system_smoke() {
        use crate::system::RealSys;
        let gpus = discover_gpus(&RealSys);
        for gpu in &gpus {
            println!(
                "{:?} card={} bdf={} driver={} vendor=0x{v:04x} device=0x{d:04x} class=0x{c:06x} render={r:?} outputs={outputs:?}",
                gpu.vendor,
                gpu.card,
                gpu.device_id.key(),
                gpu.driver,
                v = gpu.pci_vendor_id,
                d = gpu.pci_device_id,
                c = gpu.pci_class_code,
                r = gpu.render_node,
                outputs = gpu.outputs
            );
        }
        assert!(
            !gpus.is_empty(),
            "expected at least one GPU on this machine"
        );
        for gpu in &gpus {
            assert!(
                !gpu.device_id.key().is_empty(),
                "every discovered GPU must carry a BDF"
            );
            // The read path must have worked, not just the symlink walk.
            assert!(
                gpu.pci_vendor_id != 0 || gpu.driver.is_empty(),
                "GPU {bdf}: vendor read failed",
                bdf = gpu.device_id.key()
            );
        }
    }
}
