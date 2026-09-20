//! Vendor-specific GPU telemetry providers.
//!
//! Each backend (NVIDIA NVML, AMD, Intel, ...) implements [`GpuProvider`] and
//! samples into the normalized [`GpuStats`] model. The application and UI only
//! ever see [`GpuProvider`] + [`GpuStats`]; they never reference a vendor API.
//! Missing metrics stay `None` (explicit "unavailable"), never fake zeroes.

pub mod amd;
pub mod intel;
pub mod nvidia;

use std::path::{Path, PathBuf};

use crate::domain::DeviceId;
use crate::system::Sys;

fn pci_device_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

/// True when any path component of `target` (searched from the end) equals
/// `bdf`.
fn path_component_is_bdf(target: &str, bdf: &str) -> bool {
    Path::new(target)
        .components()
        .rev()
        .any(|c| c.as_os_str().to_string_lossy() == bdf)
}

/// Find the `hwmonN` for the PCI device with `bdf`.
///
/// The amdgpu and i915/xe hwmon devices are registered on the GPU's PCI
/// device, so the hwmon `device` symlink resolves to that device and the BDF
/// is one of its path components. More than one hwmon can be parented to the
/// same PCI device (e.g. a power-domain block next to the main sensor block),
/// so the best candidate is chosen: a hwmon whose `name` attribute matches
/// one of `driver_names` (e.g. `amdgpu`, `i915`, `xe`) beats an unmatched
/// one; among otherwise equal candidates the one with a `temp1_input` sensor
/// beats one without; ties keep the first in directory order (deterministic).
/// `None` when no hwmon matches the BDF at all (the provider's hwmon-only
/// metrics then degrade to `None`) — this is how APUs and driver differences
/// are tolerated.
pub(crate) fn find_hwmon_for_bdf<S: Sys>(
    sys: &S,
    bdf: &str,
    driver_names: &[&str],
) -> Option<String> {
    let root = Path::new("/sys/class/hwmon");
    let entries = sys.read_dir(root)?;
    let mut best: Option<(u8, String)> = None;
    for entry in &entries {
        if !entry.name.starts_with("hwmon") {
            continue;
        }
        let dir = root.join(&entry.name);
        let Some(target) = sys.symlink_target(&dir.join("device")) else {
            continue;
        };
        if !path_component_is_bdf(&target, bdf) {
            continue;
        }
        let name = sys
            .read_to_string(&dir.join("name"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut score = 0u8;
        if name.as_deref().is_some_and(|n| driver_names.contains(&n)) {
            score += 2;
        }
        if sys
            .read_to_string(&dir.join("temp1_input"))
            .is_some_and(|s| !s.trim().is_empty())
        {
            score += 1;
        }
        // Strictly greater only: ties keep the first candidate in dir order.
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, entry.name.clone()));
        }
    }
    best.map(|(_, name)| name)
}

/// Normalized per-sample GPU telemetry, vendor-agnostic.
///
/// `None` fields mean the provider/driver does not expose that metric.
/// `utilization` and the `memory_*` fields are `Option`: NVIDIA and AMD expose
/// them, but neither Intel driver (i915, xe) surfaces a GPU busy counter or VRAM
/// total/used through sysfs, so those report `None` — explicitly "unavailable",
/// never a fake zero.
#[derive(Clone, Debug, Default)]
pub struct GpuStats {
    /// True when a device was sampled this cycle (even if some metrics are missing).
    pub available: bool,
    /// NVML (or vendor) enumeration index of the sampled device, for display.
    pub index: u32,
    /// Stable device identity (PCI BDF and/or vendor UUID), when known.
    /// Drives the LLM chip in the GPU panel title (S23).
    pub device: DeviceId,
    /// Vendor model name.
    pub name: String,
    /// GPU core utilization, 0..=100, when the driver exposes a busy counter.
    pub utilization: Option<f64>,
    /// Memory bandwidth utilization, 0..=100, when exposed.
    pub memory_utilization: Option<f64>,
    /// Used VRAM in MiB, when exposed.
    pub memory_used_mib: Option<f64>,
    /// Total VRAM in MiB, when exposed.
    pub memory_total_mib: Option<f64>,
    /// GPU temperature in °C.
    pub temperature_c: Option<f64>,
    /// Instantaneous power draw in W.
    pub power_w: Option<f64>,
    /// Enforced power limit in W.
    pub power_limit_w: Option<f64>,
    /// Performance state (e.g. `P0`).
    pub pstate: String,
    /// Human-readable throttle/limit reason summary.
    pub limit_reason: String,
    /// Graphics clock in MHz.
    pub graphics_clock_mhz: Option<f64>,
    /// Memory clock in MHz.
    pub memory_clock_mhz: Option<f64>,
    /// Encoder (NVENC) utilization, 0..=100.
    pub encoder_utilization: Option<f64>,
    /// Decoder (NVDEC) utilization, 0..=100.
    pub decoder_utilization: Option<f64>,
    /// Fan speed, 0..=100 (may exceed 100 for valid high-speed fans).
    pub fan_percent: Option<f64>,
    /// PCIe receive throughput, MB/s.
    pub pcie_rx_mb_s: Option<f64>,
    /// PCIe transmit throughput, MB/s.
    pub pcie_tx_mb_s: Option<f64>,
    /// PCIe link speed in GT/s.
    pub pcie_link_speed_gts: Option<f64>,
    /// PCIe link width in lanes.
    pub pcie_link_width: Option<u32>,
    /// Set when sampling failed for the selected device (e.g. no match, NVML
    /// init error). Empty on success.
    pub error: String,
}

/// A GPU telemetry backend. Implementations are owned by a worker thread and
/// sampled on its cadence; `sample` must never block on the UI.
pub trait GpuProvider: Send {
    /// Sample the selected GPU once, returning normalized telemetry.
    fn sample(&mut self) -> GpuStats;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    /// Lay down a hwmon parented to `bdf`.
    fn add_hwmon(sys: &mut FixtureSys, hwmon: &str, bdf: &str, name: Option<&str>, temp: bool) {
        let dir = format!("/sys/class/hwmon/{hwmon}");
        sys.dir_entry("/sys/class/hwmon", hwmon, false, true);
        sys.symlink(
            format!("{dir}/device").as_str(),
            &format!("../../devices/pci0000:00/{bdf}"),
        );
        if let Some(n) = name {
            sys.file(format!("{dir}/name").as_str(), format!("{n}\n"));
        }
        if temp {
            sys.file(format!("{dir}/temp1_input").as_str(), "42000\n");
        }
    }

    #[test]
    fn hwmon_match_prefers_driver_name_over_directory_order() {
        // Regression: the wrong hwmon (e.g. a power-domain block) sits FIRST
        // in /sys/class/hwmon. The driver-named one must still win.
        let mut sys = FixtureSys::default();
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0", Some("acpi_power"), true);
        add_hwmon(&mut sys, "hwmon1", "0000:01:00.0", Some("amdgpu"), true);

        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:01:00.0", &["amdgpu"]),
            Some("hwmon1".to_string())
        );
    }

    #[test]
    fn hwmon_match_prefers_temp_sensor_when_names_tie() {
        let mut sys = FixtureSys::default();
        // Both unnamed and parented to the same BDF; only hwmon1 has a temp.
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0", None, false);
        add_hwmon(&mut sys, "hwmon1", "0000:01:00.0", None, true);

        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:01:00.0", &["amdgpu"]),
            Some("hwmon1".to_string())
        );
    }

    #[test]
    fn hwmon_match_keeps_directory_order_for_identical_candidates() {
        let mut sys = FixtureSys::default();
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0", Some("xe"), true);
        add_hwmon(&mut sys, "hwmon1", "0000:01:00.0", Some("xe"), true);

        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:01:00.0", &["i915", "xe"]),
            Some("hwmon0".to_string())
        );
    }

    #[test]
    fn hwmon_match_ignores_other_bdfs_and_non_hwmon_entries() {
        let mut sys = FixtureSys::default();
        add_hwmon(&mut sys, "hwmon0", "0000:02:00.0", Some("amdgpu"), true);
        sys.dir_entry("/sys/class/hwmon", "fan0", false, true); // not a hwmon

        assert_eq!(find_hwmon_for_bdf(&sys, "0000:01:00.0", &["amdgpu"]), None);
        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:02:00.0", &["amdgpu"]),
            Some("hwmon0".to_string())
        );
    }

    #[test]
    fn hwmon_match_returns_none_without_hwmon_root() {
        let sys = FixtureSys::default();
        assert_eq!(find_hwmon_for_bdf(&sys, "0000:01:00.0", &["amdgpu"]), None);
    }
}
