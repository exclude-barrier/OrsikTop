use nvml_wrapper::{
    bitmasks::device::ThrottleReasons,
    enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor},
    Nvml,
};

#[derive(Clone, Debug, Default)]
pub struct GpuStats {
    pub available: bool,
    pub index: u32,
    pub name: String,
    pub utilization: f64,
    pub memory_utilization: f64,
    pub memory_used_mib: f64,
    pub memory_total_mib: f64,
    pub temperature_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub pstate: String,
    pub limit_reason: String,
    pub graphics_clock_mhz: Option<f64>,
    pub memory_clock_mhz: Option<f64>,
    pub encoder_utilization: Option<f64>,
    pub decoder_utilization: Option<f64>,
    pub fan_percent: Option<f64>,
    pub pcie_rx_mb_s: Option<f64>,
    pub pcie_tx_mb_s: Option<f64>,
    pub pcie_link_speed_gts: Option<f64>,
    pub pcie_link_width: Option<u32>,
    pub error: String,
}

pub struct GpuMonitor {
    nvml: Option<Nvml>,
    init_error: String,
    gpu_index: u32,
}

impl GpuMonitor {
    pub fn new(gpu_index: u32) -> Self {
        match Nvml::init() {
            Ok(nvml) => Self {
                nvml: Some(nvml),
                init_error: String::new(),
                gpu_index,
            },
            Err(err) => Self {
                nvml: None,
                init_error: format!("NVML unavailable: {err}"),
                gpu_index,
            },
        }
    }

    pub fn sample(&self) -> GpuStats {
        let Some(nvml) = &self.nvml else {
            return GpuStats {
                index: self.gpu_index,
                error: self.init_error.clone(),
                ..Default::default()
            };
        };

        let device = match nvml.device_by_index(self.gpu_index) {
            Ok(device) => device,
            Err(err) => {
                return GpuStats {
                    index: self.gpu_index,
                    error: format!("cannot access NVIDIA GPU {}: {err}", self.gpu_index),
                    ..Default::default()
                }
            }
        };

        let utilization = device.utilization_rates().ok();
        let memory = device.memory_info().ok();
        let encoder = device.encoder_utilization().ok();
        let decoder = device.decoder_utilization().ok();

        let mut stats = GpuStats {
            available: true,
            index: self.gpu_index,
            name: device.name().unwrap_or_else(|_| "NVIDIA GPU".to_string()),
            utilization: utilization.as_ref().map(|v| v.gpu as f64).unwrap_or(0.0),
            memory_utilization: utilization.as_ref().map(|v| v.memory as f64).unwrap_or(0.0),
            memory_used_mib: memory.as_ref().map(|m| bytes_to_mib(m.used)).unwrap_or(0.0),
            memory_total_mib: memory
                .as_ref()
                .map(|m| bytes_to_mib(m.total))
                .unwrap_or(0.0),
            temperature_c: device
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|v| v as f64),
            power_w: device.power_usage().ok().map(|mw| mw as f64 / 1000.0),
            power_limit_w: device
                .enforced_power_limit()
                .ok()
                .map(|mw| mw as f64 / 1000.0),
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
            pcie_link_speed_gts: device
                .pcie_link_speed()
                .ok()
                .map(|mt_per_s| mt_per_s as f64 / 1000.0),
            pcie_link_width: device.current_pcie_link_width().ok(),
            error: String::new(),
        };

        sanitize(&mut stats);
        stats
    }
}

impl Default for GpuMonitor {
    fn default() -> Self {
        Self::new(0)
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
    stats.utilization = clamp_percent(stats.utilization);
    stats.memory_utilization = clamp_percent(stats.memory_utilization);
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

fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn nonnegative(value: Option<f64>) -> Option<f64> {
    value
        .filter(|value| value.is_finite())
        .map(|value| value.max(0.0))
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
        .map(clamp_percent)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            utilization: 120.0,
            memory_utilization: -4.0,
            fan_percent: Some(115.0),
            encoder_utilization: Some(140.0),
            ..Default::default()
        };
        sanitize(&mut stats);
        assert_eq!(stats.utilization, 100.0);
        assert_eq!(stats.memory_utilization, 0.0);
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
}
