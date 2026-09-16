//! GPU subsystem facade.
//!
//! The application obtains a GPU provider through [`new_gpu_provider`] and
//! samples it on the fast-worker cadence. The concrete vendor backend (NVIDIA
//! NVML, AMD sysfs/hwmon, ...) is chosen here — from the vendor-neutral
//! discovery result — so the rest of the code only ever sees the
//! vendor-neutral [`GpuProvider`] trait and [`GpuStats`] model. The shared
//! PCIe link math is also defined here, independent of any vendor.

use crate::discovery::DiscoveredGpu;
use crate::domain::{GpuSelector, GpuVendor};
use crate::providers::{
    amd::AmdGpuProvider, intel::IntelGpuProvider, nvidia::NvidiaGpuProvider, GpuProvider, GpuStats,
};

/// Build the active GPU provider for `selector`.
///
/// Resolves `selector` against the BDF-sorted discovery list and dispatches to
/// the vendor backend for the selected device: NVIDIA → NVML, AMD → amdgpu
/// sysfs/hwmon, Intel → i915/xe sysfs + hwmon. `Auto` keeps its pre-S6 meaning —
/// the first *NVIDIA* device (in BDF order), falling back to the first device of
/// any vendor when there is no NVIDIA one. `Uuid` resolves through the NVIDIA
/// provider's own NVML enumeration (vendor-neutral discovery keys are PCI BDFs,
/// not vendor UUIDs). Vendors without a backend yet (e.g. `Other`, `Unknown`)
/// get an explicit "no backend" error — never a fake reading from another
/// vendor's API.
pub fn new_gpu_provider(selector: GpuSelector, gpus: &[DiscoveredGpu]) -> Box<dyn GpuProvider> {
    if matches!(selector, GpuSelector::Uuid(_)) {
        return Box::new(NvidiaGpuProvider::new(selector));
    }

    let Some((index, gpu)) = resolve_discovered(&selector, gpus) else {
        return Box::new(UnavailableGpuProvider {
            error: "no GPU matches the selection".to_string(),
        });
    };

    match gpu.vendor {
        GpuVendor::Nvidia => Box::new(NvidiaGpuProvider::new(selector)),
        GpuVendor::Amd => Box::new(AmdGpuProvider::new(gpu.clone(), index)),
        GpuVendor::Intel => Box::new(IntelGpuProvider::new(gpu.clone(), index)),
        _ => Box::new(UnavailableGpuProvider {
            error: format!(
                "no telemetry backend yet for GPU {} ({:?})",
                gpu.device_id.key(),
                gpu.vendor
            ),
        }),
    }
}

/// Resolve a vendor-neutral [`GpuSelector`] against the BDF-sorted discovery
/// list. Returns the position and the matched device, or `None` when nothing
/// matches. `Auto` prefers the first NVIDIA device (BDF order) and falls back
/// to the first device overall. `Index` is the n-th *NVIDIA* device (the
/// legacy NVML ordinal — NVML only enumerates NVIDIA GPUs, so this preserves
/// the pre-vendor-neutral behavior where `0` was the first NVIDIA GPU even on
/// iGPU + dGPU machines); on a machine without any NVIDIA GPU it falls back to
/// the n-th device overall (the pre-0.2.1 positional behavior), so legacy
/// `gpu=0` configs keep working on Intel/AMD-only machines. `PciBusId`
/// matches on the device key. `Uuid` is not resolvable here and never reaches
/// this function.
fn resolve_discovered<'a>(
    selector: &GpuSelector,
    gpus: &'a [DiscoveredGpu],
) -> Option<(u32, &'a DiscoveredGpu)> {
    match selector {
        GpuSelector::Auto => gpus
            .iter()
            .enumerate()
            .find(|(_, gpu)| gpu.vendor == GpuVendor::Nvidia)
            .or_else(|| gpus.first().map(|gpu| (0, gpu)))
            .map(|(position, gpu)| (position as u32, gpu)),
        GpuSelector::Index(index) => {
            let index = *index as usize;
            if gpus.iter().any(|gpu| gpu.vendor == GpuVendor::Nvidia) {
                gpus.iter()
                    .enumerate()
                    .filter(|(_, gpu)| gpu.vendor == GpuVendor::Nvidia)
                    .nth(index)
                    .map(|(position, gpu)| (position as u32, gpu))
            } else {
                gpus.get(index).map(|gpu| (index as u32, gpu))
            }
        }
        GpuSelector::PciBusId(bdf) => gpus
            .iter()
            .enumerate()
            .find(|(_, gpu)| gpu.device_id.key() == bdf.as_str())
            .map(|(position, gpu)| (position as u32, gpu)),
        GpuSelector::Uuid(_) => None,
    }
}

/// A GPU provider that reports one fixed error on every sample. Used when the
/// selection matches no discovered device, or the selected vendor has no
/// telemetry backend yet.
struct UnavailableGpuProvider {
    error: String,
}

impl GpuProvider for UnavailableGpuProvider {
    fn sample(&mut self) -> GpuStats {
        GpuStats {
            available: false,
            error: self.error.clone(),
            ..Default::default()
        }
    }
}

/// Throughput in MB/s per lane for a PCIe link speed, by generation:
/// 2.5 -> Gen1, 5.0 -> Gen2, 8.0 -> Gen3, 16.0 -> Gen4, 32.0 -> Gen5.
/// Each generation encodes 16 data bits per 10 symbols.
pub(crate) fn pcie_mbps_per_lane(gts: f64) -> Option<f64> {
    if !gts.is_finite() {
        return None;
    }
    const GENERATIONS: &[(f64, f64)] = &[
        (2.5, 250.0),
        (5.0, 500.0),
        (8.0, 984.6),
        (16.0, 1969.2),
        (32.0, 3938.5),
    ];
    GENERATIONS
        .iter()
        .find(|(speed, _)| (speed - gts).abs() <= 0.01)
        .map(|(_, mbps)| *mbps)
}

/// Bidirectional PCIe utilization as a percentage of the link's theoretical
/// capacity: (rx + tx) over 2 lanes * per-lane throughput, clamped to 0..=100.
/// None if any input is missing or the capacity is not positive.
pub(crate) fn pcie_utilization_pct(
    rx_mb_s: Option<f64>,
    tx_mb_s: Option<f64>,
    link_speed_gts: Option<f64>,
    link_width: Option<u32>,
) -> Option<f64> {
    let (rx, tx, gts, width) = (rx_mb_s?, tx_mb_s?, link_speed_gts?, link_width?);
    let capacity_mbps = 2.0 * pcie_mbps_per_lane(gts)? * width as f64;
    if capacity_mbps <= 0.0 || !rx.is_finite() || !tx.is_finite() {
        return None;
    }
    Some((rx + tx) / capacity_mbps * 100.0)
        .filter(|pct| pct.is_finite())
        .map(|pct| pct.clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_pcie_lane_throughput_by_generation() {
        assert_eq!(pcie_mbps_per_lane(2.5), Some(250.0));
        assert_eq!(pcie_mbps_per_lane(5.0), Some(500.0));
        assert_eq!(pcie_mbps_per_lane(8.0), Some(984.6));
        assert_eq!(pcie_mbps_per_lane(16.0), Some(1969.2));
        assert_eq!(pcie_mbps_per_lane(32.0), Some(3938.5));
        assert_eq!(pcie_mbps_per_lane(2.51), Some(250.0));
        assert_eq!(pcie_mbps_per_lane(12.0), None);
        assert_eq!(pcie_mbps_per_lane(f64::NAN), None);
    }

    #[test]
    fn computes_pcie_utilization_from_throughput_and_link() {
        let full = pcie_utilization_pct(Some(31507.2), Some(31507.2), Some(16.0), Some(16));
        assert!((full.unwrap() - 100.0).abs() <= 1e-9);

        let quarter = pcie_utilization_pct(Some(7876.8), Some(7876.8), Some(16.0), Some(16));
        assert!((quarter.unwrap() - 25.0).abs() < 0.1);

        assert_eq!(
            pcie_utilization_pct(None, Some(100.0), Some(16.0), Some(16)),
            None
        );
        assert_eq!(
            pcie_utilization_pct(Some(100.0), None, Some(16.0), Some(16)),
            None
        );
        assert_eq!(
            pcie_utilization_pct(Some(100.0), Some(100.0), None, Some(16)),
            None
        );
        assert_eq!(
            pcie_utilization_pct(Some(100.0), Some(100.0), Some(16.0), None),
            None
        );
        assert_eq!(
            pcie_utilization_pct(Some(100.0), Some(100.0), Some(12.0), Some(16)),
            None
        );
        assert_eq!(
            pcie_utilization_pct(Some(100.0), Some(100.0), Some(16.0), Some(0)),
            None
        );
    }

    fn gpu(vendor: GpuVendor, bdf: &str) -> DiscoveredGpu {
        DiscoveredGpu {
            card: "card0".to_string(),
            device_id: crate::domain::DeviceId::new(Some(bdf.to_string()), None),
            vendor,
            pci_vendor_id: 0,
            pci_device_id: 0,
            pci_class_code: 0x030000,
            driver: String::new(),
            render_node: None,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn resolves_selector_against_discovered_gpus() {
        let gpus = vec![
            gpu(GpuVendor::Intel, "0000:00:02.0"),
            gpu(GpuVendor::Amd, "0000:01:00.0"),
            gpu(GpuVendor::Nvidia, "0000:41:00.0"),
        ];

        // Auto prefers the first NVIDIA device in BDF order.
        let (index, picked) = resolve_discovered(&GpuSelector::Auto, &gpus).unwrap();
        assert_eq!(index, 2);
        assert_eq!(picked.vendor, GpuVendor::Nvidia);

        // Index is the n-th NVIDIA device (legacy NVML ordinal). With the
        // single NVIDIA at position 2, Index(0) selects it; Index(1) is out
        // of range (only one NVIDIA device).
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(0), &gpus).map(|(i, g)| (i, g.vendor)),
            Some((2, GpuVendor::Nvidia))
        );
        assert_eq!(resolve_discovered(&GpuSelector::Index(1), &gpus), None);

        // PciBusId matches the device key; unknown BDF matches nothing.
        assert_eq!(
            resolve_discovered(&GpuSelector::PciBusId("0000:01:00.0".to_string()), &gpus)
                .map(|(i, g)| (i, g.vendor)),
            Some((1, GpuVendor::Amd))
        );
        assert_eq!(
            resolve_discovered(&GpuSelector::PciBusId("0000:99:00.0".to_string()), &gpus,),
            None
        );

        // Uuid is not resolvable against discovery.
        assert_eq!(
            resolve_discovered(&GpuSelector::Uuid("GPU-abc".to_string()), &gpus),
            None
        );

        // An empty discovery list matches nothing.
        let none: Vec<DiscoveredGpu> = Vec::new();
        assert_eq!(resolve_discovered(&GpuSelector::Auto, &none), None);
    }

    #[test]
    fn auto_falls_back_to_first_device_without_nvidia() {
        let gpus = vec![
            gpu(GpuVendor::Intel, "0000:00:02.0"),
            gpu(GpuVendor::Amd, "0000:01:00.0"),
        ];
        let (index, picked) = resolve_discovered(&GpuSelector::Auto, &gpus).unwrap();
        assert_eq!(index, 0);
        assert_eq!(picked.vendor, GpuVendor::Intel);
    }

    #[test]
    fn index_selects_the_nth_nvidia_on_igpu_dgpu_machines() {
        // The regression scenario: an iGPU (BDF-sorted first) plus a discrete
        // NVIDIA. The legacy NVML ordinal `0` must select the NVIDIA dGPU, not
        // the iGPU that sits at discovery position 0.
        let gpus = vec![
            gpu(GpuVendor::Intel, "0000:00:02.0"),
            gpu(GpuVendor::Nvidia, "0000:01:00.0"),
        ];
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(0), &gpus).map(|(i, g)| (i, g.vendor)),
            Some((1, GpuVendor::Nvidia))
        );
        assert_eq!(resolve_discovered(&GpuSelector::Index(1), &gpus), None);

        // Dual-NVIDIA: the ordinal is the n-th NVIDIA in BDF order.
        let dual = vec![
            gpu(GpuVendor::Intel, "0000:00:02.0"),
            gpu(GpuVendor::Nvidia, "0000:01:00.0"),
            gpu(GpuVendor::Nvidia, "0000:41:00.0"),
        ];
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(0), &dual).map(|(i, _)| i),
            Some(1)
        );
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(1), &dual).map(|(i, _)| i),
            Some(2)
        );
        assert_eq!(resolve_discovered(&GpuSelector::Index(2), &dual), None);

        // No NVIDIA device at all (e.g. an Intel APU laptop): the legacy
        // ordinal falls back to the n-th device overall, so `gpu=0` still
        // selects the (only) GPU instead of matching nothing.
        let no_nvidia = vec![
            gpu(GpuVendor::Intel, "0000:00:02.0"),
            gpu(GpuVendor::Amd, "0000:01:00.0"),
        ];
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(0), &no_nvidia).map(|(i, g)| (i, g.vendor)),
            Some((0, GpuVendor::Intel))
        );
        assert_eq!(
            resolve_discovered(&GpuSelector::Index(1), &no_nvidia).map(|(i, g)| (i, g.vendor)),
            Some((1, GpuVendor::Amd))
        );
        assert_eq!(resolve_discovered(&GpuSelector::Index(2), &no_nvidia), None);
    }

    #[test]
    fn dispatch_reports_explicit_errors_for_unbacked_vendors() {
        // Nothing discovered → no match.
        let mut provider = new_gpu_provider(GpuSelector::Auto, &[]);
        let stats = provider.sample();
        assert!(!stats.available);
        assert_eq!(stats.error, "no GPU matches the selection");

        // A discovered device whose vendor has no backend yet (e.g. an
        // unrecognized display class).
        let gpus = vec![gpu(GpuVendor::Other, "0000:00:02.0")];
        let mut provider = new_gpu_provider(GpuSelector::Auto, &gpus);
        let stats = provider.sample();
        assert!(!stats.available);
        assert!(stats.error.contains("no telemetry backend yet"));
        assert!(stats.error.contains("0000:00:02.0"));

        // An AMD device does NOT fall through to the error path: the AMD
        // provider is built and its own (fixture-free) sysfs read reports the
        // device as unavailable — a different, device-specific error.
        let gpus = vec![gpu(GpuVendor::Amd, "0000:01:00.0")];
        let mut provider = new_gpu_provider(GpuSelector::Auto, &gpus);
        let stats = provider.sample();
        assert!(!stats.available);
        assert!(stats.error.contains("0000:01:00.0"));
        assert!(!stats.error.contains("no telemetry backend yet"));

        // An Intel device is likewise backed: the Intel provider is built and
        // its own (fixture-free) sysfs read reports the device-specific error.
        // A card that does not exist on the box, so the i915 sysfs path is
        // deterministically unreadable regardless of the host's GPUs.
        let gpus = vec![DiscoveredGpu {
            card: "card999".to_string(),
            device_id: crate::domain::DeviceId::new(Some("0000:00:02.0".to_string()), None),
            vendor: GpuVendor::Intel,
            driver: "i915".to_string(),
            ..gpu(GpuVendor::Intel, "0000:00:02.0")
        }];
        let mut provider = new_gpu_provider(GpuSelector::Auto, &gpus);
        let stats = provider.sample();
        assert!(!stats.available);
        assert!(stats.error.contains("0000:00:02.0"));
        assert!(!stats.error.contains("no telemetry backend yet"));
    }
}
