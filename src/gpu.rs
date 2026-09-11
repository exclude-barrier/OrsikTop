use nvml_wrapper::{
    enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor},
    Nvml,
};

#[derive(Clone, Debug, Default)]
pub struct GpuStats {
    pub available: bool,
    pub name: String,
    pub utilization: f64,
    pub memory_utilization: f64,
    pub memory_used_mib: f64,
    pub memory_total_mib: f64,
    pub temperature_c: f64,
    pub power_w: f64,
    pub power_limit_w: f64,
    pub pstate: String,
    pub graphics_clock_mhz: f64,
    pub memory_clock_mhz: f64,
    pub encoder_utilization: f64,
    pub decoder_utilization: f64,
    pub fan_percent: f64,
    pub pcie_rx_mib_s: f64,
    pub pcie_tx_mib_s: f64,
    pub error: String,
}

pub struct GpuMonitor {
    nvml: Option<Nvml>,
    init_error: String,
}

impl GpuMonitor {
    pub fn new() -> Self {
        match Nvml::init() {
            Ok(nvml) => Self {
                nvml: Some(nvml),
                init_error: String::new(),
            },
            Err(err) => Self {
                nvml: None,
                init_error: format!("NVML unavailable: {err}"),
            },
        }
    }

    pub fn sample(&self) -> GpuStats {
        let Some(nvml) = &self.nvml else {
            return GpuStats {
                error: self.init_error.clone(),
                ..Default::default()
            };
        };

        let device = match nvml.device_by_index(0) {
            Ok(device) => device,
            Err(err) => {
                return GpuStats {
                    error: format!("cannot access NVIDIA GPU 0: {err}"),
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
                .map(|v| v as f64)
                .unwrap_or(0.0),
            power_w: device
                .power_usage()
                .map(|mw| mw as f64 / 1000.0)
                .unwrap_or(0.0),
            power_limit_w: device
                .enforced_power_limit()
                .map(|mw| mw as f64 / 1000.0)
                .unwrap_or(0.0),
            pstate: device
                .performance_state()
                .map(|state| normalize_pstate(&format!("{state:?}")))
                .unwrap_or_else(|_| "—".to_string()),
            graphics_clock_mhz: device
                .clock_info(Clock::Graphics)
                .map(|mhz| mhz as f64)
                .unwrap_or(0.0),
            memory_clock_mhz: device
                .clock_info(Clock::Memory)
                .map(|mhz| mhz as f64)
                .unwrap_or(0.0),
            encoder_utilization: encoder
                .as_ref()
                .map(|info| info.utilization as f64)
                .unwrap_or(0.0),
            decoder_utilization: decoder
                .as_ref()
                .map(|info| info.utilization as f64)
                .unwrap_or(0.0),
            fan_percent: device.fan_speed(0).map(|speed| speed as f64).unwrap_or(0.0),
            pcie_rx_mib_s: device
                .pcie_throughput(PcieUtilCounter::Receive)
                .map(kib_per_second_to_mib)
                .unwrap_or(0.0),
            pcie_tx_mib_s: device
                .pcie_throughput(PcieUtilCounter::Send)
                .map(kib_per_second_to_mib)
                .unwrap_or(0.0),
            error: String::new(),
        };

        sanitize(&mut stats);
        stats
    }
}

impl Default for GpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

fn kib_per_second_to_mib(kib_per_second: u32) -> f64 {
    kib_per_second as f64 / 1024.0
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

fn sanitize(stats: &mut GpuStats) {
    stats.utilization = stats.utilization.clamp(0.0, 100.0);
    stats.memory_utilization = stats.memory_utilization.clamp(0.0, 100.0);
    stats.encoder_utilization = stats.encoder_utilization.clamp(0.0, 100.0);
    stats.decoder_utilization = stats.decoder_utilization.clamp(0.0, 100.0);
    stats.fan_percent = stats.fan_percent.clamp(0.0, 100.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_nvml_pcie_units_to_mib_per_second() {
        assert_eq!(kib_per_second_to_mib(1024), 1.0);
        assert_eq!(kib_per_second_to_mib(2048), 2.0);
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
    fn clamps_percentages() {
        let mut stats = GpuStats {
            utilization: 120.0,
            memory_utilization: -4.0,
            fan_percent: 101.0,
            ..Default::default()
        };
        sanitize(&mut stats);
        assert_eq!(stats.utilization, 100.0);
        assert_eq!(stats.memory_utilization, 0.0);
        assert_eq!(stats.fan_percent, 100.0);
    }
}
