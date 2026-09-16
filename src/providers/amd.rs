//! AMD GPU/APU telemetry provider backed by the amdgpu kernel sysfs and hwmon.
//!
//! The provider samples one already-discovered AMD GPU (identified by its PCI
//! BDF) into the normalized [`GpuStats`] model. It reads only stable, kernel
//! exposed attributes:
//!
//! - PCI device dir (`/sys/bus/pci/devices/<BDF>/`): `gpu_busy_percent`,
//!   `mem_busy_percent` (0–100), `mem_info_vram_total`/`_used` (bytes) and
//!   `product_name`.
//! - hwmon (the `amdgpu` hwmon device, registered on the PCI device so its
//!   `device` symlink resolves to the BDF): `temp1_input` (edge, millidegrees),
//!   `freq1_input`/`freq2_input` (sclk/mclk, Hz), `power1_input` (microwatts) and
//!   `fan1_input`/`fan1_max` (RPM).
//!
//! Missing attributes stay `None` (explicit "unavailable"), never a fake zero.
//! On an APU the VRAM figures are the system-memory carveout the kernel itself
//! reports through the same `mem_info_vram_*` files — the value is what the
//! kernel exposes, not an invented dedicated-VRAM number. All filesystem access
//! goes through [`Sys`] so the provider is fixture-testable.

use std::path::{Path, PathBuf};

use super::{GpuProvider, GpuStats};
use crate::discovery::DiscoveredGpu;
use crate::system::{RealSys, Sys};

/// AMD GPU/APU telemetry provider backed by amdgpu sysfs + hwmon.
///
/// `S` is the filesystem it samples; production uses [`RealSys`], fixture tests
/// use an in-memory `Sys`. The hwmon device is resolved once at construction
/// (the hwmon set is stable for the life of the process) so `sample` reads only
/// value files.
pub(crate) struct AmdGpuProvider<S: Sys> {
    /// The discovered device this provider samples (identity, BDF, card name).
    gpu: DiscoveredGpu,
    /// The filesystem to sample.
    sys: S,
    /// hwmon device name (`hwmonN`) parented to this GPU's PCI device, if found
    /// at construction. `None` when the hwmon is absent, in which case the
    /// thermal/clock/power/fan metrics degrade to `None`.
    hwmon: Option<String>,
    /// Position of this device in the discovery list (display only).
    index: u32,
}

impl AmdGpuProvider<RealSys> {
    /// Build a provider over the real filesystem for one discovered AMD GPU.
    pub(crate) fn new(gpu: DiscoveredGpu, index: u32) -> Self {
        let sys = RealSys;
        let hwmon = find_hwmon_for_bdf(&sys, gpu.device_id.key());
        Self {
            gpu,
            sys,
            hwmon,
            index,
        }
    }
}

fn pci_device_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

/// Find the `hwmonN` whose parent is the PCI device with `bdf`. The amdgpu
/// hwmon is registered on the GPU's PCI device, so the hwmon `device` symlink
/// resolves to that device and the BDF is one of its path components. `None`
/// when no hwmon matches (then the thermal/clock/power/fan metrics degrade to
/// `None`) — this is how kernel/driver differences are tolerated.
fn find_hwmon_for_bdf<S: Sys>(sys: &S, bdf: &str) -> Option<String> {
    let root = Path::new("/sys/class/hwmon");
    let entries = sys.read_dir(root)?;
    for entry in &entries {
        if !entry.name.starts_with("hwmon") {
            continue;
        }
        let device_link = root.join(&entry.name).join("device");
        let Some(target) = sys.symlink_target(&device_link) else {
            continue;
        };
        if path_component_is_bdf(&target, bdf) {
            return Some(entry.name.clone());
        }
    }
    None
}

/// True when any path component of `target` (searched from the end) equals `bdf`.
fn path_component_is_bdf(target: &str, bdf: &str) -> bool {
    Path::new(target)
        .components()
        .rev()
        .any(|c| c.as_os_str().to_string_lossy() == bdf)
}

impl<S: Sys + Send> GpuProvider for AmdGpuProvider<S> {
    fn sample(&mut self) -> GpuStats {
        let bdf = self.gpu.device_id.key();
        let pci = pci_device_dir(bdf);

        if self.sys.read_dir(&pci).is_none() {
            return GpuStats {
                available: false,
                index: self.index,
                device: self.gpu.device_id.clone(),
                name: self.gpu.card.clone(),
                error: format!("AMD GPU PCI device {bdf} not readable"),
                ..Default::default()
            };
        }

        let utilization = read_u64(&self.sys, &pci.join("gpu_busy_percent")).map(|v| v as f64);
        let memory_utilization =
            read_u64(&self.sys, &pci.join("mem_busy_percent")).map(|v| v as f64);
        let vram_total = read_u64(&self.sys, &pci.join("mem_info_vram_total")).map(bytes_to_mib);
        let vram_used = read_u64(&self.sys, &pci.join("mem_info_vram_used")).map(bytes_to_mib);
        let name = read_text(&self.sys, &pci.join("product_name"))
            .unwrap_or_else(|| self.gpu.card.clone());

        let mut stats = GpuStats {
            available: true,
            index: self.index,
            device: self.gpu.device_id.clone(),
            name,
            utilization,
            memory_utilization,
            memory_used_mib: vram_used,
            memory_total_mib: vram_total,
            ..Default::default()
        };

        self.fill_hwmon(&mut stats);

        sanitize(&mut stats);
        stats
    }
}

impl<S: Sys> AmdGpuProvider<S> {
    /// Fill the hwmon-derived metrics (edge temp, power, clocks, fan) on
    /// `stats`. Each field is set to `None` when the hwmon or its attribute is
    /// absent — explicit "unavailable", never a fake zero.
    fn fill_hwmon(&self, stats: &mut GpuStats) {
        let Some(hwmon) = &self.hwmon else {
            return;
        };
        let dir = Path::new("/sys/class/hwmon").join(hwmon);
        stats.temperature_c =
            read_i64(&self.sys, &dir.join("temp1_input")).map(|v| v as f64 / 1000.0);
        stats.power_w =
            read_i64(&self.sys, &dir.join("power1_input")).map(|v| v as f64 / 1_000_000.0);
        stats.graphics_clock_mhz =
            read_i64(&self.sys, &dir.join("freq1_input")).map(|v| v as f64 / 1_000_000.0);
        stats.memory_clock_mhz =
            read_i64(&self.sys, &dir.join("freq2_input")).map(|v| v as f64 / 1_000_000.0);
        stats.fan_percent = match (
            read_i64(&self.sys, &dir.join("fan1_input")),
            read_i64(&self.sys, &dir.join("fan1_max")),
        ) {
            (Some(rpm), Some(max)) if max > 0 => Some(rpm as f64 / max as f64 * 100.0),
            _ => None,
        };
    }
}

fn read_text<S: Sys>(sys: &S, path: &Path) -> Option<String> {
    sys.read_to_string(path)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_u64<S: Sys>(sys: &S, path: &Path) -> Option<u64> {
    read_text(sys, path).and_then(|s| s.parse::<u64>().ok())
}

fn read_i64<S: Sys>(sys: &S, path: &Path) -> Option<i64> {
    read_text(sys, path).and_then(|s| s.parse::<i64>().ok())
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

/// Clamp utilization percentages to 0..=100 and drop non-finite optional
/// readings, mirroring the NVIDIA provider's normalization.
fn sanitize(stats: &mut GpuStats) {
    stats.utilization = stats.utilization.map(clamp_percent);
    stats.memory_utilization = stats.memory_utilization.map(clamp_percent);
    stats.fan_percent = stats
        .fan_percent
        .filter(|v| v.is_finite())
        .map(|v| v.max(0.0));

    stats.temperature_c = nonnegative(stats.temperature_c);
    stats.power_w = nonnegative(stats.power_w);
    stats.graphics_clock_mhz = nonnegative(stats.graphics_clock_mhz);
    stats.memory_clock_mhz = nonnegative(stats.memory_clock_mhz);
}

fn nonnegative(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite()).map(|v| v.max(0.0))
}

fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceId, GpuVendor};
    use crate::system::FixtureSys;

    fn amd_gpu(bdf: &str, card: &str) -> DiscoveredGpu {
        DiscoveredGpu {
            card: card.to_string(),
            device_id: DeviceId::new(Some(bdf.to_string()), None),
            vendor: GpuVendor::Amd,
            pci_vendor_id: 0x1002,
            pci_device_id: 0x74a0,
            pci_class_code: 0x030000,
            driver: "amdgpu".to_string(),
            render_node: None,
            outputs: Vec::new(),
        }
    }

    /// Lay down the amdgpu PCI device sysfs attributes for `bdf`.
    fn add_pci(
        sys: &mut FixtureSys,
        bdf: &str,
        busy: Option<u32>,
        mem_busy: Option<u32>,
        vram: Option<u64>,
    ) {
        let dir = format!("/sys/bus/pci/devices/{bdf}");
        sys.dir_entry(&dir, "", false, false);
        if let Some(b) = busy {
            sys.file(format!("{dir}/gpu_busy_percent").as_str(), format!("{b}\n"));
        }
        if let Some(b) = mem_busy {
            sys.file(format!("{dir}/mem_busy_percent").as_str(), format!("{b}\n"));
        }
        if let Some(total) = vram {
            sys.file(
                format!("{dir}/mem_info_vram_total").as_str(),
                format!("{total}\n"),
            );
            sys.file(
                format!("{dir}/mem_info_vram_used").as_str(),
                format!("{}\n", total / 4),
            );
        }
    }

    /// Lay down the amdgpu hwmon device parented to `bdf` with a full sensor set.
    fn add_hwmon(sys: &mut FixtureSys, hwmon: &str, bdf: &str) {
        let dir = format!("/sys/class/hwmon/{hwmon}");
        sys.dir_entry("/sys/class/hwmon", hwmon, false, true);
        sys.symlink(
            format!("{dir}/device").as_str(),
            &format!("../../devices/pci0000:00/{bdf}"),
        );
        sys.file(format!("{dir}/temp1_input").as_str(), "55200\n"); // 55.2 °C
        sys.file(format!("{dir}/power1_input").as_str(), "180000000\n"); // 180 W
        sys.file(format!("{dir}/freq1_input").as_str(), "2100000000\n"); // 2100 MHz
        sys.file(format!("{dir}/freq2_input").as_str(), "1000000000\n"); // 1000 MHz
        sys.file(format!("{dir}/fan1_input").as_str(), "1500\n"); // RPM
        sys.file(format!("{dir}/fan1_max").as_str(), "3000\n"); // RPM
    }

    fn provider(sys: FixtureSys, bdf: &str) -> AmdGpuProvider<FixtureSys> {
        let hwmon = find_hwmon_for_bdf(&sys, bdf);
        AmdGpuProvider {
            gpu: amd_gpu(bdf, "card0"),
            sys,
            hwmon,
            index: 0,
        }
    }

    #[test]
    fn samples_amd_dgpu_full_sensor_set() {
        let mut sys = FixtureSys::default();
        add_pci(
            &mut sys,
            "0000:01:00.0",
            Some(42),
            Some(17),
            Some(16 * 1024 * 1024 * 1024),
        );
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0");
        let mut provider = provider(sys, "0000:01:00.0");

        let stats = provider.sample();

        assert!(stats.available);
        assert_eq!(stats.name, "card0"); // no product_name → card name
        assert_eq!(stats.utilization, Some(42.0));
        assert_eq!(stats.memory_utilization, Some(17.0));
        assert_eq!(stats.memory_total_mib, Some(16384.0));
        assert_eq!(stats.memory_used_mib, Some(4096.0));
        assert_eq!(stats.temperature_c, Some(55.2));
        assert_eq!(stats.power_w, Some(180.0));
        assert_eq!(stats.graphics_clock_mhz, Some(2100.0));
        assert_eq!(stats.memory_clock_mhz, Some(1000.0));
        assert_eq!(stats.fan_percent, Some(50.0));
    }

    #[test]
    fn degrades_gracefully_when_hwmon_is_absent() {
        let mut sys = FixtureSys::default();
        add_pci(
            &mut sys,
            "0000:01:00.0",
            Some(5),
            Some(2),
            Some(8 * 1024 * 1024 * 1024),
        );
        // No hwmon device at all.
        let mut provider = provider(sys, "0000:01:00.0");

        let stats = provider.sample();

        assert!(stats.available);
        assert_eq!(stats.utilization, Some(5.0));
        assert_eq!(stats.memory_total_mib, Some(8192.0));
        // hwmon-only metrics are explicitly unavailable.
        assert_eq!(stats.temperature_c, None);
        assert_eq!(stats.power_w, None);
        assert_eq!(stats.graphics_clock_mhz, None);
        assert_eq!(stats.fan_percent, None);
    }

    #[test]
    fn product_name_overrides_card_name_and_missing_vram_is_none() {
        let mut sys = FixtureSys::default();
        let dir = "/sys/bus/pci/devices/0000:01:00.0";
        sys.dir_entry(dir, "", false, false);
        sys.file(format!("{dir}/gpu_busy_percent").as_str(), "9\n");
        sys.file(
            format!("{dir}/product_name").as_str(),
            "Radeon RX 7800 XT\n",
        );
        // No mem_info_vram_* files.
        let mut provider = provider(sys, "0000:01:00.0");

        let stats = provider.sample();

        assert_eq!(stats.name, "Radeon RX 7800 XT");
        assert_eq!(stats.utilization, Some(9.0));
        assert_eq!(stats.memory_total_mib, None);
    }

    #[test]
    fn device_not_readable_reports_unavailable() {
        let sys = FixtureSys::default();
        let mut provider = provider(sys, "0000:01:00.0");
        let stats = provider.sample();
        assert!(!stats.available);
        assert!(stats.error.contains("not readable"));
    }

    #[test]
    fn hwmon_is_matched_by_bdf_component() {
        let mut sys = FixtureSys::default();
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0");
        add_hwmon(&mut sys, "hwmon1", "0000:02:00.0");

        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:01:00.0"),
            Some("hwmon0".to_string())
        );
        assert_eq!(
            find_hwmon_for_bdf(&sys, "0000:02:00.0"),
            Some("hwmon1".to_string())
        );
        assert_eq!(find_hwmon_for_bdf(&sys, "0000:09:99.9"), None);
    }

    #[test]
    fn converts_bytes_to_mib() {
        assert_eq!(bytes_to_mib(1024 * 1024), 1.0);
        assert_eq!(bytes_to_mib(16 * 1024 * 1024 * 1024), 16384.0);
    }

    #[test]
    fn malformed_sysfs_values_degrade_to_none_not_fakes() {
        let mut sys = FixtureSys::default();
        // A device whose numeric attributes are corrupt (truncated writes,
        // non-numeric garbage, or missing) must degrade each affected metric to
        // `None` — the provider must not parse a partial value or invent one.
        let dir = "/sys/bus/pci/devices/0000:01:00.0";
        sys.dir_entry(dir, "", false, false);
        sys.file(format!("{dir}/gpu_busy_percent").as_str(), "100%\n"); // not a number
        sys.file(format!("{dir}/mem_info_vram_total").as_str(), "16G\n"); // not a number
        sys.file(format!("{dir}/product_name").as_str(), ""); // empty → card fallback
        let mut provider = provider(sys, "0000:01:00.0");
        let stats = provider.sample();

        assert!(stats.available, "a readable device dir is available");
        assert_eq!(stats.name, "card0"); // empty product_name falls back to the card
        assert_eq!(stats.utilization, None); // "100%" is not a u64
        assert_eq!(stats.memory_utilization, None); // absent
        assert_eq!(stats.memory_total_mib, None); // "16G" is not a u64
        assert_eq!(stats.memory_used_mib, None); // absent
                                                 // No hwmon registered → the thermal/clock/power/fan metrics are `None`.
        assert_eq!(stats.temperature_c, None);
        assert_eq!(stats.power_w, None);
        assert_eq!(stats.graphics_clock_mhz, None);
    }
}
