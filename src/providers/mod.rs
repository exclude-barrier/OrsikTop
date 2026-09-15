//! Vendor-specific GPU telemetry providers.
//!
//! Each backend (NVIDIA NVML, AMD, Intel, ...) implements [`GpuProvider`] and
//! samples into the normalized [`GpuStats`] model. The application and UI only
//! ever see [`GpuProvider`] + [`GpuStats`]; they never reference a vendor API.
//! Missing metrics stay `None` (explicit "unavailable"), never fake zeroes.

pub mod nvidia;

use crate::domain::DeviceId;

/// Normalized per-sample GPU telemetry, vendor-agnostic.
///
/// `None` fields mean the provider/driver does not expose that metric.
/// `utilization`/`memory_*` default to 0.0 (an idle GPU really is at 0%);
/// everything else is optional because many devices lack the sensor.
#[derive(Clone, Debug, Default)]
pub struct GpuStats {
    /// True when a device was sampled this cycle (even if some metrics are missing).
    pub available: bool,
    /// NVML (or vendor) enumeration index of the sampled device, for display.
    pub index: u32,
    /// Stable device identity (PCI BDF and/or vendor UUID), when known.
    /// Rendered in the heterogeneous-hardware UX pass (S23).
    #[allow(dead_code)]
    pub device: DeviceId,
    /// Vendor model name.
    pub name: String,
    /// GPU core utilization, 0..=100.
    pub utilization: f64,
    /// Memory bandwidth utilization, 0..=100.
    pub memory_utilization: f64,
    /// Used VRAM in MiB.
    pub memory_used_mib: f64,
    /// Total VRAM in MiB.
    pub memory_total_mib: f64,
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
