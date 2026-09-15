//! GPU subsystem facade.
//!
//! The application obtains a GPU provider through [`new_gpu_provider`] and
//! samples it on the fast-worker cadence. The concrete vendor backend (currently
//! NVIDIA NVML, with AMD/Intel planned) is chosen here so the rest of the code
//! only ever sees the vendor-neutral [`GpuProvider`] trait and [`GpuStats`]
//! model. The shared PCIe link math is also defined here, independent of any
//! vendor.

use crate::domain::GpuSelector;
use crate::providers::{nvidia::NvidiaGpuProvider, GpuProvider};

/// Build the active GPU provider for `selector`.
///
/// Currently always an NVIDIA NVML provider. When S6/S7 land, this is the
/// single place that selects the backend (e.g. from vendor-neutral discovery).
pub fn new_gpu_provider(selector: GpuSelector) -> Box<dyn GpuProvider> {
    Box::new(NvidiaGpuProvider::new(selector))
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
}
