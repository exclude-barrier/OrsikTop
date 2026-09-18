//! NVIDIA GPU telemetry provider backed by NVML.
//!
//! NVML is the primary enrichment source for NVIDIA devices. The provider
//! resolves the user's [`GpuSelector`] against the (cached) NVML device
//! enumeration — matched by vendor UUID or PCI bus id, never by bare ordinal
//! — and samples the selected device into the normalized [`GpuStats`] model.
//! Missing metrics stay `None`; a device with no matching selection or an
//! NVML init failure is reported through `GpuStats.error`, never a fake zero.

use std::time::{Duration, Instant};

use nvml_wrapper::{
    bitmasks::device::ThrottleReasons,
    enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor},
    Device, Nvml,
};

use super::{GpuProvider, GpuStats};
use crate::domain::{is_mig_uuid, normalize_pci_bdf, DeviceId, GpuSelector};

/// One MIG child device of a physical GPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MigChild {
    /// NVML index of the physical parent.
    pub parent_index: u32,
    /// MIG device slot within the parent (`0..mig_device_count`).
    pub slot: u32,
    /// Stable key: the MIG UUID the driver reports for the child handle, or
    /// the synthetic `<parent>#mig-<slot>` fallback.
    pub key: String,
}

impl MigChild {
    /// `(gpu_instance_id, compute_instance_id)` when the key is a real MIG
    /// UUID; `None` for synthetic fallback keys, which carry no instance ids.
    pub(crate) fn instance(&self) -> Option<(u32, u32)> {
        DeviceId::new(None, Some(self.key.clone())).mig_instance()
    }
}

/// Enumerate the MIG children of one physical device: MIG mode enabled,
/// then each probed MIG device handle's UUID. Any NVML failure yields no
/// children — never a panic. The topology is captured once at construction;
/// an admin re-partition requires a provider restart.
pub(crate) fn mig_children_of(
    parent_index: u32,
    device: &Device,
    parent: &DeviceId,
) -> Vec<MigChild> {
    let enabled = matches!(device.mig_mode(), Ok(mode) if mode.current == 1);
    if !enabled {
        return Vec::new();
    }
    let count = match device.mig_device_count() {
        Ok(count) => count,
        Err(_) => return Vec::new(),
    };
    (0..count)
        .filter_map(|slot| {
            let child = device.mig_device_by_index(slot).ok()?;
            Some(MigChild {
                parent_index,
                slot,
                key: mig_child_key(child.uuid().ok().as_deref(), parent, slot),
            })
        })
        .collect()
}

/// Stable key for a MIG child: the MIG UUID the driver reports for the
/// child handle, else `<parent-uuid>#mig-<slot>` (parent BDF when the
/// parent has no UUID). A driver UUID that is not MIG-shaped is treated as
/// absent.
fn mig_child_key(mig_uuid: Option<&str>, parent: &DeviceId, slot: u32) -> String {
    match mig_uuid {
        Some(uuid) if !uuid.is_empty() && is_mig_uuid(uuid) => uuid.to_string(),
        _ => {
            let parent_key = parent
                .uuid
                .as_deref()
                .or(parent.pci_bdf.as_deref())
                .unwrap_or("nvidia");
            format!("{parent_key}#mig-{slot}")
        }
    }
}

/// Build the full MIG topology — every MIG child of every NVML device — in
/// one pass. Called once at startup, never on the sampling path: the
/// topology is static and an admin re-partition requires a restart. Any NVML
/// failure yields fewer/no children, never a panic.
pub(crate) fn discover_mig_children(nvml: &Nvml) -> Vec<MigChild> {
    let Ok(count) = nvml.device_count() else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for index in 0..count {
        let Ok(device) = nvml.device_by_index(index) else {
            continue;
        };
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
        children.extend(mig_children_of(index, &device, &parent));
    }
    children
}

/// NVIDIA GPU telemetry provider backed by NVML.
pub(crate) struct NvidiaGpuProvider {
    nvml: Option<Nvml>,
    init_error: String,
    selector: GpuSelector,
    /// Cached NVML device enumeration: (index, uuid, pci bus id).
    /// Populated once at construction; the index→device mapping is stable for
    /// the life of the provider, so sampling does not re-enumerate NVML.
    devices: Vec<(u32, Option<String>, Option<String>)>,
    /// MIG children of the physical devices, keyed by stable MIG identity.
    /// Captured once at construction (see `mig_children_of`); a re-partition
    /// requires a provider restart.
    mig_children: Vec<MigChild>,
    /// Device name, fetched at most once. The vendor model name is immutable
    /// for the device's lifetime, so re-reading it on the sampling path (10 Hz
    /// at the default refresh) is pure waste.
    name: Option<String>,
    /// Slowly-changing properties (power limit, PCIe link parameters),
    /// refreshed at most once per second.
    slow: SlowProperties,
}

/// Slowly-changing NVML properties, refreshed at most once per second.
///
/// The enforced power limit and PCIe link speed/width only change on user
/// action or link retrain, so a 1 Hz refresh is indistinguishable from live
/// for display purposes while keeping four driver round-trips off the 10 Hz
/// sampling path. When every lookup fails the last good values are kept and
/// the refresh is retried on the next sample.
#[derive(Default)]
struct SlowProperties {
    power_limit_w: Option<f64>,
    pcie_link_speed_gts: Option<f64>,
    pcie_link_width: Option<u32>,
    fetched_at: Option<Instant>,
}

impl SlowProperties {
    const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

    fn refresh(&mut self, device: &Device) {
        if !due_to_refresh(self.fetched_at, Instant::now()) {
            return;
        }
        let power_limit_w = device
            .enforced_power_limit()
            .ok()
            .map(|mw| mw as f64 / 1000.0);
        let pcie_link_speed_gts = device
            .pcie_link_speed()
            .ok()
            .map(|mt_per_s| mt_per_s as f64 / 1000.0);
        let pcie_link_width = device.current_pcie_link_width().ok();
        if power_limit_w.is_none() && pcie_link_speed_gts.is_none() && pcie_link_width.is_none() {
            // All lookups failed: keep the last good values, retry next cycle.
            return;
        }
        self.power_limit_w = power_limit_w;
        self.pcie_link_speed_gts = pcie_link_speed_gts;
        self.pcie_link_width = pcie_link_width;
        self.fetched_at = Some(Instant::now());
    }
}

/// True when slow properties are due for a refresh: never fetched, or the
/// last successful fetch is at least one refresh interval old.
fn due_to_refresh(fetched_at: Option<Instant>, now: Instant) -> bool {
    match fetched_at {
        None => true,
        Some(fetched) => now.duration_since(fetched) >= SlowProperties::REFRESH_INTERVAL,
    }
}

/// A resolved sample target: the NVML index to display (the physical
/// parent's index when sampling a MIG child), the device handle to sample,
/// the stable identity, and the MIG slot when the target is a child.
struct ResolvedSample<'nvml> {
    index: u32,
    device: Device<'nvml>,
    device_id: DeviceId,
    mig_slot: Option<u32>,
}

impl NvidiaGpuProvider {
    pub(crate) fn new(selector: GpuSelector) -> Self {
        match Nvml::init() {
            Ok(nvml) => {
                let count = nvml.device_count().unwrap_or(0);
                let mut devices = Vec::with_capacity(count as usize);
                let mut mig_children = Vec::new();
                for index in 0..count {
                    if let Ok(device) = nvml.device_by_index(index) {
                        let uuid = device.uuid().ok();
                        let bus_id = device
                            .pci_info()
                            .ok()
                            .map(|pci| normalize_pci_bdf(&pci.bus_id));
                        let device_id = DeviceId::new(bus_id.clone(), uuid.clone());
                        mig_children.extend(mig_children_of(index, &device, &device_id));
                        devices.push((index, uuid, bus_id));
                    }
                }
                Self {
                    nvml: Some(nvml),
                    init_error: String::new(),
                    selector,
                    devices,
                    mig_children,
                    name: None,
                    slow: SlowProperties::default(),
                }
            }
            Err(err) => Self {
                nvml: None,
                init_error: format!("NVML unavailable: {err}"),
                selector,
                devices: Vec::new(),
                mig_children: Vec::new(),
                name: None,
                slow: SlowProperties::default(),
            },
        }
    }

    /// Resolve the selection to a sample target. `None` when the selection
    /// matches nothing; `Some(Err(..))` when the device handle cannot be
    /// opened.
    fn resolve_sample<'nvml>(
        &self,
        nvml: &'nvml Nvml,
    ) -> Option<Result<ResolvedSample<'nvml>, String>> {
        if let GpuSelector::Uuid(uuid) = &self.selector {
            if let Some(child) = self.mig_children.iter().find(|c| &c.key == uuid) {
                let parent = match nvml.device_by_index(child.parent_index) {
                    Ok(parent) => parent,
                    Err(err) => {
                        return Some(Err(format!(
                            "cannot access NVIDIA GPU {}: {err}",
                            child.parent_index
                        )))
                    }
                };
                let device = match parent.mig_device_by_index(child.slot) {
                    Ok(device) => device,
                    Err(err) => {
                        return Some(Err(format!(
                            "cannot access MIG device {}/{}: {err}",
                            child.parent_index, child.slot
                        )))
                    }
                };
                return Some(Ok(ResolvedSample {
                    index: child.parent_index,
                    device,
                    device_id: DeviceId::new(None, Some(child.key.clone())),
                    mig_slot: Some(child.slot),
                }));
            }
        }
        let index = self.selector.resolve(&self.devices)?;
        let device = match nvml.device_by_index(index) {
            Ok(device) => device,
            Err(err) => return Some(Err(format!("cannot access NVIDIA GPU {index}: {err}"))),
        };
        let device_id = self
            .devices
            .iter()
            .find(|(i, _, _)| *i == index)
            .map(|(_, uuid, bus_id)| DeviceId::new(bus_id.clone(), uuid.clone()))
            .unwrap_or_default();
        Some(Ok(ResolvedSample {
            index,
            device,
            device_id,
            mig_slot: None,
        }))
    }
}

impl GpuProvider for NvidiaGpuProvider {
    fn sample(&mut self) -> GpuStats {
        let Some(nvml) = &self.nvml else {
            return GpuStats {
                error: self.init_error.clone(),
                ..Default::default()
            };
        };

        let ResolvedSample {
            index,
            device,
            device_id,
            mig_slot,
        } = match self.resolve_sample(nvml) {
            Some(Ok(resolved)) => resolved,
            Some(Err(err)) => {
                return GpuStats {
                    error: err,
                    ..Default::default()
                }
            }
            None => {
                return GpuStats {
                    error: "no NVIDIA GPU matches the selection".to_string(),
                    ..Default::default()
                }
            }
        };
        self.slow.refresh(&device);

        let name = if let Some(cached) = &self.name {
            cached.clone()
        } else {
            match device.name() {
                Ok(name) => {
                    self.name = Some(name.clone());
                    name
                }
                Err(_) => "NVIDIA GPU".to_string(),
            }
        };
        let name = match mig_slot {
            Some(slot) => format!("{name} MIG {slot}"),
            None => name,
        };

        let utilization = device.utilization_rates().ok();
        let memory = device.memory_info().ok();
        let encoder = device.encoder_utilization().ok();
        let decoder = device.decoder_utilization().ok();

        let mut stats = GpuStats {
            available: true,
            index,
            device: device_id,
            name,
            utilization: utilization.as_ref().map(|v| v.gpu as f64),
            memory_utilization: utilization.as_ref().map(|v| v.memory as f64),
            memory_used_mib: memory.as_ref().map(|m| bytes_to_mib(m.used)),
            memory_total_mib: memory.as_ref().map(|m| bytes_to_mib(m.total)),
            temperature_c: device
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|v| v as f64),
            power_w: device.power_usage().ok().map(|mw| mw as f64 / 1000.0),
            power_limit_w: self.slow.power_limit_w,
            pstate: device
                .performance_state()
                .map(|state| normalize_pstate(&format!("{state:?}")))
                .unwrap_or_else(|_| "—".to_string()),
            limit_reason: device
                .current_throttle_reasons()
                .map(format_throttle_reasons)
                .unwrap_or_else(|_| "—".to_string()),
            graphics_clock_mhz: device
                .clock_info(Clock::Graphics)
                .ok()
                .map(|mhz| mhz as f64),
            memory_clock_mhz: device.clock_info(Clock::Memory).ok().map(|mhz| mhz as f64),
            encoder_utilization: encoder.as_ref().map(|info| info.utilization as f64),
            decoder_utilization: decoder.as_ref().map(|info| info.utilization as f64),
            fan_percent: device.fan_speed(0).ok().map(|speed| speed as f64),
            pcie_rx_mb_s: device
                .pcie_throughput(PcieUtilCounter::Receive)
                .ok()
                .map(kb_per_second_to_mb),
            pcie_tx_mb_s: device
                .pcie_throughput(PcieUtilCounter::Send)
                .ok()
                .map(kb_per_second_to_mb),
            pcie_link_speed_gts: self.slow.pcie_link_speed_gts,
            pcie_link_width: self.slow.pcie_link_width,
            error: String::new(),
        };

        sanitize(&mut stats);
        stats
    }
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

fn kb_per_second_to_mb(kb_per_second: u32) -> f64 {
    kb_per_second as f64 / 1000.0
}

fn normalize_pstate(raw: &str) -> String {
    let number = match raw {
        "Zero" => Some(0),
        "One" => Some(1),
        "Two" => Some(2),
        "Three" => Some(3),
        "Four" => Some(4),
        "Five" => Some(5),
        "Six" => Some(6),
        "Seven" => Some(7),
        "Eight" => Some(8),
        "Nine" => Some(9),
        "Ten" => Some(10),
        "Eleven" => Some(11),
        "Twelve" => Some(12),
        "Thirteen" => Some(13),
        "Fourteen" => Some(14),
        "Fifteen" => Some(15),
        _ => None,
    };

    number
        .map(|n| format!("P{n}"))
        .unwrap_or_else(|| raw.to_string())
}

fn format_throttle_reasons(reasons: ThrottleReasons) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }

    let mut labels = Vec::new();

    if reasons.contains(ThrottleReasons::SW_POWER_CAP) {
        labels.push("power");
    }
    if reasons
        .intersects(ThrottleReasons::SW_THERMAL_SLOWDOWN | ThrottleReasons::HW_THERMAL_SLOWDOWN)
    {
        labels.push("thermal");
    }
    if reasons.contains(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN) {
        labels.push("power-brake");
    }
    if reasons.contains(ThrottleReasons::HW_SLOWDOWN) {
        labels.push("hw");
    }
    if reasons.contains(ThrottleReasons::SYNC_BOOST) {
        labels.push("sync");
    }
    if reasons.contains(ThrottleReasons::APPLICATIONS_CLOCKS_SETTING) {
        labels.push("app-clock");
    }
    if reasons.contains(ThrottleReasons::DISPLAY_CLOCK_SETTING) {
        labels.push("display");
    }
    if reasons.contains(ThrottleReasons::GPU_IDLE) {
        labels.push("idle");
    }

    if labels.is_empty() {
        "other".to_string()
    } else {
        labels.join("+")
    }
}

fn sanitize(stats: &mut GpuStats) {
    stats.utilization = stats.utilization.map(clamp_percent);
    stats.memory_utilization = stats.memory_utilization.map(clamp_percent);
    stats.encoder_utilization = stats.encoder_utilization.map(clamp_percent);
    stats.decoder_utilization = stats.decoder_utilization.map(clamp_percent);
    stats.fan_percent = stats
        .fan_percent
        .filter(|value| value.is_finite())
        .map(|value| value.max(0.0));

    stats.temperature_c = nonnegative(stats.temperature_c);
    stats.power_w = nonnegative(stats.power_w);
    stats.power_limit_w = nonnegative(stats.power_limit_w);
    stats.graphics_clock_mhz = nonnegative(stats.graphics_clock_mhz);
    stats.memory_clock_mhz = nonnegative(stats.memory_clock_mhz);
    stats.pcie_rx_mb_s = nonnegative(stats.pcie_rx_mb_s);
    stats.pcie_link_speed_gts = nonnegative(stats.pcie_link_speed_gts);
    stats.pcie_link_width = stats.pcie_link_width.filter(|width| *width > 0);
}

fn nonnegative(value: Option<f64>) -> Option<f64> {
    value
        .filter(|value| value.is_finite())
        .map(|value| value.max(0.0))
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
    use nvml_wrapper::bitmasks::device::ThrottleReasons;

    #[test]
    fn slow_properties_refresh_due_only_after_interval() {
        let now = Instant::now();
        assert!(due_to_refresh(None, now));
        let fetched = now;
        assert!(!due_to_refresh(
            Some(fetched),
            fetched + Duration::from_millis(999)
        ));
        assert!(due_to_refresh(
            Some(fetched),
            fetched + SlowProperties::REFRESH_INTERVAL
        ));
    }
    #[test]
    fn converts_nvml_pcie_kb_to_mb_per_second() {
        assert_eq!(kb_per_second_to_mb(1000), 1.0);
        assert_eq!(kb_per_second_to_mb(2500), 2.5);
    }

    #[test]
    fn converts_bytes_to_mib() {
        assert_eq!(bytes_to_mib(1024 * 1024), 1.0);
    }

    #[test]
    fn normalizes_nvml_pstate_names() {
        assert_eq!(normalize_pstate("Zero"), "P0");
        assert_eq!(normalize_pstate("Two"), "P2");
        assert_eq!(normalize_pstate("Fifteen"), "P15");
        assert_eq!(normalize_pstate("Unknown"), "Unknown");
    }

    #[test]
    fn formats_nvml_throttle_reasons() {
        assert_eq!(format_throttle_reasons(ThrottleReasons::empty()), "none");
        assert_eq!(
            format_throttle_reasons(ThrottleReasons::SW_POWER_CAP),
            "power"
        );
        assert_eq!(
            format_throttle_reasons(
                ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_THERMAL_SLOWDOWN
            ),
            "power+thermal"
        );
        assert_eq!(format_throttle_reasons(ThrottleReasons::GPU_IDLE), "idle");
    }

    #[test]
    fn clamps_utilization_but_allows_fan_above_100() {
        let mut stats = GpuStats {
            utilization: Some(120.0),
            memory_utilization: Some(-4.0),
            fan_percent: Some(115.0),
            encoder_utilization: Some(140.0),
            ..Default::default()
        };
        sanitize(&mut stats);
        assert_eq!(stats.utilization, Some(100.0));
        assert_eq!(stats.memory_utilization, Some(0.0));
        assert_eq!(stats.encoder_utilization, Some(100.0));
        assert_eq!(stats.fan_percent, Some(115.0));
    }

    #[test]
    fn drops_non_finite_optional_values() {
        let mut stats = GpuStats {
            temperature_c: Some(f64::NAN),
            power_w: Some(f64::INFINITY),
            ..Default::default()
        };
        sanitize(&mut stats);
        assert_eq!(stats.temperature_c, None);
        assert_eq!(stats.power_w, None);
    }

    #[test]
    fn sanitize_drops_invalid_pcie_link_state() {
        let mut stats = GpuStats {
            pcie_link_speed_gts: Some(f64::NAN),
            pcie_link_width: Some(0),
            ..Default::default()
        };
        sanitize(&mut stats);
        assert_eq!(stats.pcie_link_speed_gts, None);
        assert_eq!(stats.pcie_link_width, None);
    }

    #[test]
    fn mig_child_key_prefers_driver_uuid() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        assert_eq!(
            mig_child_key(Some("MIG-GPU-parent-1-0"), &parent, 0),
            "MIG-GPU-parent-1-0"
        );
    }

    #[test]
    fn mig_child_key_falls_back_to_parent_uuid_then_bdf() {
        let with_uuid = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        assert_eq!(mig_child_key(None, &with_uuid, 2), "GPU-parent#mig-2");
        // A non-MIG driver UUID is treated as absent.
        assert_eq!(
            mig_child_key(Some("GPU-not-a-mig"), &with_uuid, 2),
            "GPU-parent#mig-2"
        );
        let bdf_only = DeviceId::new(Some("0000:41:00.0".into()), None);
        assert_eq!(mig_child_key(None, &bdf_only, 2), "0000:41:00.0#mig-2");
    }

    #[test]
    fn mig_child_key_fallback_is_parseable() {
        let parent = DeviceId::new(Some("0000:41:00.0".into()), Some("GPU-parent".into()));
        let key = DeviceId::new(None, Some(mig_child_key(None, &parent, 2)));
        assert_eq!(key.mig_parent_uuid().as_deref(), Some("GPU-parent"));
        assert_eq!(key.mig_instance(), None);
    }
}
