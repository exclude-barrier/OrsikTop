//! Intel GPU/APU telemetry provider backed by the i915 and xe kernel sysfs
//! plus the Intel hwmon.
//!
//! The provider samples one already-discovered Intel GPU (identified by its PCI
//! BDF) into the normalized [`GpuStats`] model. It reads only stable, kernel
//! exposed attributes:
//!
//! - xe (per-GT sysfs under the PCI device, `/sys/bus/pci/devices/<BDF>/`):
//!   `tile0/gt0/freq0/cur_freq` (graphics clock, MHz) and
//!   `tile0/gt0/freq0/throttle/reasons` (throttle reason, `none` when idle).
//! - i915 (root-GT sysfs on the DRM card kobject — `gt_get_parent_obj` is the
//!   primary card device): `rps_cur_freq_mhz` (graphics clock, MHz) and the
//!   `throttle_reason_*` boolean files (throttle reason), directly under
//!   `/sys/class/drm/<card>/`.
//! - hwmon (the `xe`/`i915` hwmon device, registered on the PCI device so its
//!   `device` symlink resolves to the BDF): `temp1_input` (millidegrees) and
//!   `power1_max` (the PL1 package power limit, microwatts).
//!
//! Neither Intel driver exposes a GPU busy counter, a memory-bandwidth
//! utilization, VRAM total/used, an instantaneous power draw, or a fan-speed
//! maximum through a value sysfs file. Those `GpuStats` fields therefore stay
//! `None` (explicit "unavailable") rather than a fake zero — the same rule the
//! AMD and NVIDIA providers follow for any metric their driver does not report.
//! The hwmon exists only for dGfx (discrete) devices; on an APU it is absent,
//! in which case the temperature and power limit also degrade to `None` while
//! the clock and throttle reason remain available from the GT sysfs. All
//! filesystem access goes through [`Sys`] so the provider is fixture-testable.

use std::path::{Path, PathBuf};

use super::{GpuProvider, GpuStats};
use crate::discovery::DiscoveredGpu;
use crate::system::{RealSys, Sys};

/// Intel GPU/APU telemetry provider backed by i915/xe sysfs + hwmon.
///
/// `S` is the filesystem it samples; production uses [`RealSys`], fixture tests
/// use an in-memory `Sys`. The hwmon device is resolved once at construction
/// (the hwmon set is stable for the life of the process) so `sample` reads only
/// value files.
pub(crate) struct IntelGpuProvider<S: Sys> {
    /// The discovered device this provider samples (identity, BDF, card name).
    gpu: DiscoveredGpu,
    /// The filesystem to sample.
    sys: S,
    /// hwmon device name (`hwmonN`) parented to this GPU's PCI device, if found
    /// at construction. `None` when the hwmon is absent (APUs, or drivers that
    /// do not register one), in which case the temperature and power-limit
    /// metrics degrade to `None`.
    hwmon: Option<String>,
    /// Position of this device in the discovery list (display only).
    index: u32,
}

impl IntelGpuProvider<RealSys> {
    /// Build a provider over the real filesystem for one discovered Intel GPU.
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

/// Find the `hwmonN` whose parent is the PCI device with `bdf`. The Intel hwmon
/// is registered on the GPU's PCI device, so the hwmon `device` symlink
/// resolves to that device and the BDF is one of its path components. `None`
/// when no hwmon matches (then the thermal/power-limit metrics degrade to
/// `None`) — this is how APUs and driver differences are tolerated.
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

/// A short, stable display name for a handful of well-known Intel GPU device
/// IDs, so the panel can show e.g. `Intel Arc (Core Ultra 200V iGPU)` instead
/// of the bare `card0` sysfs name. The kernel exposes no product name for these
/// devices, so the map is the only name source; it is deliberately small.
/// Unknown device IDs fall back to the card name (the pre-existing behavior),
/// so nothing regresses and no name is invented for an unrecognized device.
fn display_name(gpu: &DiscoveredGpu) -> String {
    const KNOWN: &[(u16, &str)] = &[(0x64a0, "Intel Arc (Core Ultra 200V iGPU)")];
    KNOWN
        .iter()
        .find(|(id, _)| *id == gpu.pci_device_id)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| gpu.card.clone())
}

/// The directory the driver's GT sysfs attributes are read from. xe hangs them
/// under the PCI device (`tile0/gt0/freq0`); i915 puts the root-GT attributes on
/// the primary DRM card kobject, reachable through the card directory.
fn gt_sysfs_dir(gpu: &DiscoveredGpu) -> PathBuf {
    match gpu.driver.as_str() {
        "xe" => pci_device_dir(gpu.device_id.key()).join("tile0/gt0/freq0"),
        _ => Path::new("/sys/class/drm").join(&gpu.card),
    }
}

impl<S: Sys + Send> GpuProvider for IntelGpuProvider<S> {
    fn sample(&mut self) -> GpuStats {
        let bdf = self.gpu.device_id.key();
        let base = gt_sysfs_dir(&self.gpu);

        if self.sys.read_dir(&base).is_none() {
            return GpuStats {
                available: false,
                index: self.index,
                device: self.gpu.device_id.clone(),
                name: display_name(&self.gpu),
                error: format!("Intel GPU PCI device {bdf} not readable"),
                ..Default::default()
            };
        }

        let mut stats = GpuStats {
            available: true,
            index: self.index,
            device: self.gpu.device_id.clone(),
            name: display_name(&self.gpu),
            // No utilization / memory-bandwidth / VRAM source in either driver.
            utilization: None,
            memory_utilization: None,
            memory_used_mib: None,
            memory_total_mib: None,
            ..Default::default()
        };

        if self.gpu.driver == "xe" {
            self.sample_xe(&mut stats);
        } else {
            self.sample_i915(&mut stats);
        }

        self.fill_hwmon(&mut stats);

        sanitize(&mut stats);
        stats
    }
}

impl<S: Sys> IntelGpuProvider<S> {
    /// xe: graphics clock is the current requested frequency; the throttle
    /// reason is a space-separated word list (`none` when not throttled).
    fn sample_xe(&self, stats: &mut GpuStats) {
        let dir = gt_sysfs_dir(&self.gpu);
        stats.graphics_clock_mhz = read_u64(&self.sys, &dir.join("cur_freq")).map(|v| v as f64);
        stats.limit_reason =
            read_throttle_reason(read_text(&self.sys, &dir.join("throttle/reasons")));
    }

    /// i915: graphics clock is the current RPS frequency (MHz). The root-GT
    /// attributes live on the primary DRM card kobject (directly under
    /// `/sys/class/drm/<card>/`); the `gt/gt0` subdirectory is tried as a
    /// fallback. The throttle reason is assembled from the `throttle_reason_*`
    /// boolean files in the same directory the clock was read from.
    fn sample_i915(&self, stats: &mut GpuStats) {
        let card = Path::new("/sys/class/drm").join(&self.gpu.card);
        let freq_candidates = [
            card.join("rps_cur_freq_mhz"),
            card.join("gt/gt0/rps_cur_freq_mhz"),
        ];
        let mut base = card.to_path_buf();
        for path in &freq_candidates {
            if let Some(v) = read_u64(&self.sys, path) {
                stats.graphics_clock_mhz = Some(v as f64);
                base = path.parent().unwrap_or(&card).to_path_buf();
                break;
            }
        }

        let mut reasons: Vec<String> = Vec::new();
        for (attr, label) in [
            ("throttle_reason_pl1", "power"),
            ("throttle_reason_pl2", "power"),
            ("throttle_reason_pl4", "power"),
            ("throttle_reason_thermal", "thermal"),
            ("throttle_reason_prochot", "prochot"),
            ("throttle_reason_ratl", "ratl"),
            ("throttle_reason_vr_thermalert", "vr-thermalert"),
            ("throttle_reason_vr_tdc", "vr-tdc"),
        ] {
            if read_u64(&self.sys, &base.join(attr)) == Some(1) {
                reasons.push(label.to_string());
            }
        }
        stats.limit_reason = if reasons.is_empty() {
            "none".to_string()
        } else {
            reasons.join("+")
        };
    }

    /// Fill the hwmon-derived metrics (package temperature, PL1 power limit) on
    /// `stats`. Each field is set to `None` when the hwmon or its attribute is
    /// absent — explicit "unavailable", never a fake zero. Neither Intel hwmon
    /// exposes an instantaneous power draw or a fan-speed maximum, so
    /// `power_w` and `fan_percent` stay `None`.
    fn fill_hwmon(&self, stats: &mut GpuStats) {
        let Some(hwmon) = &self.hwmon else {
            return;
        };
        let dir = Path::new("/sys/class/hwmon").join(hwmon);
        stats.temperature_c =
            read_i64(&self.sys, &dir.join("temp1_input")).map(|v| v as f64 / 1000.0);
        stats.power_limit_w = read_i64(&self.sys, &dir.join("power1_max"))
            .filter(|v| *v > 0)
            .map(|v| v as f64 / 1_000_000.0);
    }
}

/// Map a raw throttle-reason source to the normalized `limit_reason` string: a
/// missing or empty reading is `none`, otherwise the text is passed through
/// (xe emits a space-separated word list, `none` when not throttled).
fn read_throttle_reason(raw: Option<String>) -> String {
    match raw {
        Some(text) if !text.is_empty() => text,
        _ => "none".to_string(),
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

/// Normalize optional readings: drop non-finite values and clamp the PL1 power
/// limit to a non-negative draw, mirroring the AMD provider's sanitization.
/// (The four utilization/memory fields are `None` for Intel and left as-is.)
fn sanitize(stats: &mut GpuStats) {
    stats.temperature_c = nonnegative(stats.temperature_c);
    stats.power_limit_w = nonnegative(stats.power_limit_w);
    stats.graphics_clock_mhz = nonnegative(stats.graphics_clock_mhz);
}

fn nonnegative(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite()).map(|v| v.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceId, GpuVendor};
    use crate::system::FixtureSys;

    fn intel_gpu(driver: &str, bdf: &str, card: &str) -> DiscoveredGpu {
        DiscoveredGpu {
            card: card.to_string(),
            device_id: DeviceId::new(Some(bdf.to_string()), None),
            vendor: GpuVendor::Intel,
            pci_vendor_id: 0x8086,
            pci_device_id: 0x064a,
            pci_class_code: 0x030000,
            driver: driver.to_string(),
            render_node: None,
            outputs: Vec::new(),
        }
    }

    fn provider(sys: FixtureSys, gpu: DiscoveredGpu) -> IntelGpuProvider<FixtureSys> {
        let hwmon = find_hwmon_for_bdf(&sys, gpu.device_id.key());
        IntelGpuProvider {
            gpu,
            sys,
            hwmon,
            index: 0,
        }
    }

    /// Lay down the xe per-GT frequency attributes for `bdf`.
    fn add_xe(sys: &mut FixtureSys, bdf: &str, cur_freq: Option<u32>, reasons: Option<&str>) {
        let dir = format!("/sys/bus/pci/devices/{bdf}");
        sys.dir_entry(&dir, "", false, false);
        let freq = format!("{dir}/tile0/gt0/freq0");
        sys.dir_entry(&dir, "tile0", true, false);
        sys.dir_entry(&freq, "", false, false);
        if let Some(f) = cur_freq {
            sys.file(format!("{freq}/cur_freq").as_str(), format!("{f}\n"));
        }
        if let Some(r) = reasons {
            sys.file(
                format!("{freq}/throttle/reasons").as_str(),
                format!("{r}\n"),
            );
        }
    }

    /// Lay down the i915 root-GT attributes on the DRM card.
    fn add_i915(
        sys: &mut FixtureSys,
        card: &str,
        bdf: &str,
        rps_cur: Option<u32>,
        throttled: bool,
    ) {
        let dir = format!("/sys/class/drm/{card}");
        sys.dir_entry("/sys/class/drm", card, false, false);
        sys.dir_entry(&dir, "", false, false);
        sys.symlink(
            format!("{dir}/device").as_str(),
            &format!("/sys/devices/pci0000:00/{bdf}"),
        );
        if let Some(f) = rps_cur {
            sys.file(format!("{dir}/rps_cur_freq_mhz").as_str(), format!("{f}\n"));
        }
        sys.file(
            format!("{dir}/throttle_reason_status").as_str(),
            format!("{}\n", if throttled { 1 } else { 0 }),
        );
        sys.file(
            format!("{dir}/throttle_reason_thermal").as_str(),
            format!("{}\n", if throttled { 1 } else { 0 }),
        );
    }

    /// Lay down the Intel hwmon device parented to `bdf` with a package temp and
    /// a PL1 power limit.
    fn add_hwmon(
        sys: &mut FixtureSys,
        hwmon: &str,
        bdf: &str,
        temp: Option<&str>,
        pl1: Option<&str>,
    ) {
        let dir = format!("/sys/class/hwmon/{hwmon}");
        sys.dir_entry("/sys/class/hwmon", hwmon, false, true);
        sys.symlink(
            format!("{dir}/device").as_str(),
            &format!("../../devices/pci0000:00/{bdf}"),
        );
        if let Some(t) = temp {
            sys.file(format!("{dir}/temp1_input").as_str(), format!("{t}\n"));
        }
        if let Some(p) = pl1 {
            sys.file(format!("{dir}/power1_max").as_str(), format!("{p}\n"));
        }
    }

    #[test]
    fn samples_xe_dgpu_with_hwmon() {
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:01:00.0", Some(1250), Some("none"));
        add_hwmon(
            &mut sys,
            "hwmon0",
            "0000:01:00.0",
            Some("48000"),
            Some("280000000"),
        );
        let mut provider = provider(sys, intel_gpu("xe", "0000:01:00.0", "card0"));

        let stats = provider.sample();

        assert!(stats.available);
        assert_eq!(stats.name, "card0");
        assert_eq!(stats.graphics_clock_mhz, Some(1250.0));
        assert_eq!(stats.limit_reason, "none");
        assert_eq!(stats.temperature_c, Some(48.0));
        assert_eq!(stats.power_limit_w, Some(280.0));
        // Neither driver exposes utilization, VRAM, a power draw, or a fan.
        assert_eq!(stats.utilization, None);
        assert_eq!(stats.memory_utilization, None);
        assert_eq!(stats.memory_used_mib, None);
        assert_eq!(stats.memory_total_mib, None);
        assert_eq!(stats.power_w, None);
        assert_eq!(stats.fan_percent, None);
    }

    #[test]
    fn display_name_maps_known_ids_and_falls_back_to_card() {
        // The Core Ultra 200V APU (this machine's 0x8086:0x64a0) maps to a real
        // name instead of the bare sysfs card name.
        let apu = intel_gpu("xe", "0000:00:02.0", "card0");
        let mut named = apu.clone();
        named.pci_device_id = 0x64a0;
        assert_eq!(
            display_name(&named),
            "Intel Arc (Core Ultra 200V iGPU)".to_string()
        );

        // A provider built over a known APU reports the mapped name on a real
        // (fixture) sample, and an unknown one keeps the card name.
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:00:02.0", Some(800), Some("none"));
        assert_eq!(
            provider(sys, named.clone()).sample().name,
            "Intel Arc (Core Ultra 200V iGPU)"
        );
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:00:02.0", Some(800), Some("none"));
        assert_eq!(provider(sys, apu).sample().name, "card0");
    }

    #[test]
    fn xe_apu_without_hwmon_keeps_clock_and_reason() {
        // The APU shape on this machine: GT sysfs present, no hwmon registered.
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:00:02.0", Some(800), Some("none"));
        // No hwmon device at all.
        let mut provider = provider(sys, intel_gpu("xe", "0000:00:02.0", "card0"));

        let stats = provider.sample();

        assert!(stats.available);
        assert_eq!(stats.graphics_clock_mhz, Some(800.0));
        assert_eq!(stats.limit_reason, "none");
        // hwmon-only metrics are explicitly unavailable, not zero.
        assert_eq!(stats.temperature_c, None);
        assert_eq!(stats.power_limit_w, None);
    }

    #[test]
    fn xe_reports_throttle_reason_when_throttled() {
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:01:00.0", Some(700), Some("thermal prochot"));
        let mut provider = provider(sys, intel_gpu("xe", "0000:01:00.0", "card0"));

        let stats = provider.sample();

        assert_eq!(stats.graphics_clock_mhz, Some(700.0));
        assert_eq!(stats.limit_reason, "thermal prochot");
    }

    #[test]
    fn samples_i915_from_card_directory() {
        let mut sys = FixtureSys::default();
        add_i915(&mut sys, "card0", "0000:00:02.0", Some(1000), false);
        let mut provider = provider(sys, intel_gpu("i915", "0000:00:02.0", "card0"));

        let stats = provider.sample();

        assert!(stats.available);
        assert_eq!(stats.graphics_clock_mhz, Some(1000.0));
        assert_eq!(stats.limit_reason, "none");
    }

    #[test]
    fn i915_reports_throttle_reason_from_bool_files() {
        let mut sys = FixtureSys::default();
        add_i915(&mut sys, "card0", "0000:00:02.0", Some(600), true);
        let mut provider = provider(sys, intel_gpu("i915", "0000:00:02.0", "card0"));

        let stats = provider.sample();

        assert_eq!(stats.limit_reason, "thermal");
    }

    #[test]
    fn device_not_readable_reports_unavailable() {
        // No GT sysfs directory at all for either driver.
        let sys = FixtureSys::default();
        let mut provider = provider(sys, intel_gpu("xe", "0000:01:00.0", "card0"));
        let stats = provider.sample();
        assert!(!stats.available);
        assert!(stats.error.contains("not readable"));
    }

    #[test]
    fn hwmon_is_matched_by_bdf_component() {
        let mut sys = FixtureSys::default();
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0", Some("40000"), None);
        add_hwmon(&mut sys, "hwmon1", "0000:02:00.0", Some("50000"), None);

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
    fn pl1_limit_of_zero_is_treated_as_unavailable() {
        // PL1 disabled: the driver reports 0, which is not a real limit.
        let mut sys = FixtureSys::default();
        add_xe(&mut sys, "0000:01:00.0", Some(900), Some("none"));
        add_hwmon(&mut sys, "hwmon0", "0000:01:00.0", Some("33000"), Some("0"));
        let mut provider = provider(sys, intel_gpu("xe", "0000:01:00.0", "card0"));

        let stats = provider.sample();

        assert_eq!(stats.temperature_c, Some(33.0));
        assert_eq!(stats.power_limit_w, None);
    }

    /// Smoke test against the real machine (run with
    /// `cargo test -- --ignored intel_real`). Not part of CI.
    #[test]
    #[ignore]
    fn intel_real_system_smoke() {
        use crate::discovery::discover_gpus;
        let gpus = discover_gpus(&RealSys);
        let intel = gpus
            .iter()
            .enumerate()
            .filter(|(_, g)| g.vendor == GpuVendor::Intel)
            .collect::<Vec<_>>();
        if intel.is_empty() {
            eprintln!("no Intel GPU on this machine; skipping");
            return;
        }
        for (i, gpu) in intel {
            let mut provider = IntelGpuProvider::new(gpu.clone(), i as u32);
            let stats = provider.sample();
            println!(
                "{:?} card={} bdf={} driver={} available={} clock={:?} reason={} temp={:?} limit={:?}",
                gpu.vendor,
                gpu.card,
                gpu.device_id.key(),
                gpu.driver,
                stats.available,
                stats.graphics_clock_mhz,
                stats.limit_reason,
                stats.temperature_c,
                stats.power_limit_w
            );
            assert!(stats.available);
            assert_eq!(stats.device.key(), gpu.device_id.key());
            assert!(stats.utilization.is_none());
            assert!(stats.memory_total_mib.is_none());
        }
    }
}
