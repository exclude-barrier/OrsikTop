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
    stats.pcie_tx_mb_s = nonnegative(stats.pcie_tx_mb_s);
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
}
