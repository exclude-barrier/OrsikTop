//! Capability-based CPU sensor telemetry: frequency, temperature, power.
//!
//! Sensor *discovery* (which files exist) is done once and cached; each sample
//! only re-reads the dynamic counter values. A sensor that is absent or
//! unreadable yields `None` — an honest "unavailable" — never a fake zero.
//!
//! Sources, preferred first:
//! - Frequency: cpufreq policy `scaling_cur_freq` (kHz, per policy), weighted by
//!   each policy's CPU count. The caller falls back to `/proc/cpuinfo` `cpu MHz`
//!   when no policy reports a current frequency.
//! - Temperature: hwmon `temp*_input` (millidegrees) from CPU thermal drivers
//!   (`coretemp`, `k10temp`, `zenpower`, `x86_pkg`, …), preferring package/Tctl
//!   sensors over per-core readings.
//! - Power: RAPL `intel-rapl` package zone. Two `energy_uj` reads a short bounded
//!   window apart; a counter that reads lower (kernel reset) clamps to 0 W.
//!   `energy_uj` is root-only on current kernels, so a non-root OrsikTop reads
//!   `None` — the path is still implemented and fixture-tested.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::system::Sys;

/// Upper bound for the RAPL two-read measurement window (µs). Well below any
/// realistic `energy_uj` wrap window; also the floor for the legacy `udelay`
/// clamp below.
const RAPL_SAMPLE_US: u64 = 50_000;

/// One cpufreq policy discovered at init.
#[derive(Clone, Debug)]
struct FreqPolicy {
    dir: PathBuf,
    /// CPUs this policy covers — weights the per-policy frequency by its share
    /// of the logical CPU count.
    cpus: Vec<usize>,
}

/// One hwmon temperature input discovered at init.
#[derive(Clone, Debug)]
struct TempSensor {
    path: PathBuf,
    /// Package/Tctl-style sensor (preferred over per-core readings).
    preferred: bool,
}

/// One RAPL power zone discovered at init.
#[derive(Clone, Debug)]
struct PowerZone {
    dir: PathBuf,
    /// Bounded measurement window (µs): the RAPL read window, clamped to stay
    /// under the kernel's no-wrap window (`udelay/2`) when the legacy file is
    /// present, else [`RAPL_SAMPLE_US`].
    window_us: u64,
}

/// Discovered CPU sensor layout, built once (see [`CpuSensors::discover`]).
#[derive(Debug, Default)]
struct SensorLayout {
    freq_policies: Vec<FreqPolicy>,
    temp_sensors: Vec<TempSensor>,
    power_zones: Vec<PowerZone>,
}

/// Cached CPU sensor state. [`discover`] runs the (relatively expensive)
/// directory walks once; [`sample`] re-reads only the dynamic counter values.
#[derive(Debug, Default)]
pub struct CpuSensors {
    layout: Option<SensorLayout>,
}

impl CpuSensors {
    /// Discovers cpufreq policies, CPU temperature sensors, and RAPL power
    /// zones. Call once at worker start; the layout is stable for the life of
    /// the process (hot-plugging sensors is out of scope).
    pub fn discover<S: Sys>(&mut self, sys: &S) {
        self.layout = Some(SensorLayout {
            freq_policies: discover_freq_policies(sys),
            temp_sensors: discover_temp_sensors(sys),
            power_zones: discover_power_zones(sys),
        });
    }

    fn layout(&self) -> Option<&SensorLayout> {
        self.layout.as_ref()
    }

    /// Average current CPU frequency in MHz across all cpufreq policies,
    /// weighted by each policy's CPU count. `None` when no policy reports a
    /// current frequency (the caller falls back to `/proc/cpuinfo`).
    pub fn sample_frequency_mhz<S: Sys>(&self, sys: &S) -> Option<f64> {
        let layout = self.layout()?;
        let mut weighted_sum = 0.0;
        let mut weight = 0usize;
        for policy in &layout.freq_policies {
            let Some(khz) = read_u64_file(sys, &policy.dir.join("scaling_cur_freq")) else {
                continue;
            };
            if khz == 0 {
                continue;
            }
            let mhz = khz as f64 / 1000.0;
            if !mhz.is_finite() || mhz <= 0.0 {
                continue;
            }
            let w = policy.cpus.len().max(1);
            weighted_sum += mhz * w as f64;
            weight += w;
        }
        (weight > 0).then_some(weighted_sum / weight as f64)
    }

    /// Maximum CPU temperature in degrees Celsius, preferring package/Tctl
    /// sensors over per-core readings. `None` when no CPU hwmon is readable.
    pub fn sample_temperature_c<S: Sys>(&self, sys: &S) -> Option<f64> {
        let layout = self.layout()?;
        let mut preferred: Vec<f64> = Vec::new();
        let mut fallback: Vec<f64> = Vec::new();
        for sensor in &layout.temp_sensors {
            let Some(raw) = read_u64_file(sys, &sensor.path) else {
                continue;
            };
            let celsius = (raw as f64) / 1000.0;
            if !(-20.0..=150.0).contains(&celsius) {
                continue;
            }
            if sensor.preferred {
                preferred.push(celsius);
            } else {
                fallback.push(celsius);
            }
        }
        preferred
            .into_iter()
            .reduce(f64::max)
            .or_else(|| fallback.into_iter().reduce(f64::max))
    }

    /// CPU package power in watts from the RAPL package zone: two `energy_uj`
    /// reads a short bounded window apart. `None` when no package zone is
    /// readable — `energy_uj` is root-only on current kernels, so a non-root
    /// OrsikTop reports no power. A counter that reads lower than the first
    /// read (kernel reset) clamps to 0 W.
    pub fn sample_power_w<S: Sys>(&self, sys: &S) -> Option<f64> {
        let layout = self.layout()?;
        for zone in &layout.power_zones {
            let Some(e0) = read_u64_file(sys, &zone.dir.join("energy_uj")) else {
                continue;
            };
            std::thread::sleep(Duration::from_micros(zone.window_us));
            let Some(e1) = read_u64_file(sys, &zone.dir.join("energy_uj")) else {
                continue;
            };
            return Some(rapl_watts(e0, e1, zone.window_us));
        }
        None
    }
}

/// RAPL power from an energy delta over a measurement window. The µJ and µs
/// factors cancel, so `watts == delta_uj / window_us`. A negative delta
/// (counter wrap/reset within the short window) clamps to 0 W.
fn rapl_watts(previous_uj: u64, energy_uj: u64, window_us: u64) -> f64 {
    if window_us == 0 || energy_uj < previous_uj {
        return 0.0;
    }
    (energy_uj - previous_uj) as f64 / window_us as f64
}

/// True for entries that name a directory we can walk: a real directory or a
/// symlink (class entries like `/sys/class/hwmon/*` are symlinks to the real
/// device directories; a symlink to a file simply fails `read_dir` and is
/// skipped).
fn is_dir_entry(entry: &crate::system::SysEntry) -> bool {
    entry.is_dir || entry.is_symlink
}

fn discover_freq_policies<S: Sys>(sys: &S) -> Vec<FreqPolicy> {
    let Some(entries) = sys.read_dir(Path::new("/sys/devices/system/cpu/cpufreq")) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|entry| is_dir_entry(entry) && entry.name.starts_with("policy"))
        .map(|entry| {
            let dir = PathBuf::from(format!("/sys/devices/system/cpu/cpufreq/{}", entry.name));
            // `affected_cpus` is authoritative for the policy's CPU set; fall
            // back to a single-CPU policy when it is unreadable.
            let cpus = sys
                .read_to_string(&dir.join("affected_cpus"))
                .and_then(|text| crate::cpu::parse_cpu_list(&text))
                .unwrap_or_else(|| vec![0]);
            FreqPolicy { dir, cpus }
        })
        .collect()
}

fn discover_temp_sensors<S: Sys>(sys: &S) -> Vec<TempSensor> {
    let Some(hwmons) = sys.read_dir(Path::new("/sys/class/hwmon")) else {
        return Vec::new();
    };
    let mut sensors = Vec::new();
    for hwmon in hwmons {
        if !is_dir_entry(&hwmon) || !hwmon.name.starts_with("hwmon") || hwmon.name == "hwmon" {
            continue;
        }
        let name = read_trimmed(sys, &hwmon_path(&hwmon.name, "name")).to_ascii_lowercase();
        if !is_cpu_thermal_driver(&name) {
            continue;
        }
        let Some(entries) = sys.read_dir(&hwmon_path(&hwmon.name, "")) else {
            continue;
        };
        for entry in entries {
            if !entry.name.starts_with("temp") || !entry.name.ends_with("_input") {
                continue;
            }
            let stem = entry.name.trim_end_matches("_input").to_string();
            let preferred = is_package_label(&read_trimmed(
                sys,
                &hwmon_path(&hwmon.name, &format!("{stem}_label")),
            ));
            sensors.push(TempSensor {
                path: hwmon_path(&hwmon.name, &entry.name),
                preferred,
            });
        }
    }
    sensors
}

fn discover_power_zones<S: Sys>(sys: &S) -> Vec<PowerZone> {
    let Some(zones) = sys.read_dir(Path::new("/sys/class/powercap")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for zone in zones {
        if !is_dir_entry(&zone) || !zone.name.starts_with("intel-rapl") {
            continue;
        }
        // The MMIO variant mirrors the plain `intel-rapl` package zone; skip it
        // so the same package is not counted twice.
        if zone.name.starts_with("intel-rapl-mmio") {
            continue;
        }
        let dir = PathBuf::from(format!("/sys/class/powercap/{}", zone.name));
        // Only the package zone (not the core/uncore/dram sub-zones).
        let name = read_trimmed(sys, &dir.join("name"));
        if !name.starts_with("package") {
            continue;
        }
        // A bounded window is required: the legacy `udelay` file gives the
        // no-wrap window directly; the newer `max_energy_range_uj` (with no
        // `udelay`) still bounds it — accept either as evidence of a real zone.
        let udelay = read_u64_file(sys, &dir.join("udelay"));
        let max_range = read_u64_file(sys, &dir.join("max_energy_range_uj"));
        if udelay.is_none() && max_range.is_none() {
            continue;
        }
        let window_us = udelay
            .map(|u| (u.saturating_sub(1)) / 2)
            .unwrap_or(RAPL_SAMPLE_US)
            .min(RAPL_SAMPLE_US);
        out.push(PowerZone { dir, window_us });
    }
    out
}

/// True for hwmon drivers that report CPU (not GPU/disk/USB) temperatures.
fn is_cpu_thermal_driver(name: &str) -> bool {
    name.contains("coretemp")
        || name.contains("k10temp")
        || name.contains("zenpower")
        || name.contains("x86_pkg")
        || name == "cpu"
        || (name.contains("cpu") && !name.contains("gpu"))
}

/// Package-level sensor labels are preferred over per-core readings.
fn is_package_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.contains("package") || label.contains("tctl") || label.contains("cpu")
}

fn hwmon_path(hwmon: &str, file: &str) -> PathBuf {
    if file.is_empty() {
        PathBuf::from(format!("/sys/class/hwmon/{hwmon}"))
    } else {
        PathBuf::from(format!("/sys/class/hwmon/{hwmon}/{file}"))
    }
}

fn read_trimmed<S: Sys>(sys: &S, path: &Path) -> String {
    sys.read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_u64_file<S: Sys>(sys: &S, path: &Path) -> Option<u64> {
    sys.read_to_string(path)
        .and_then(|text| text.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    fn cpufreq_fixture() -> FixtureSys {
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/sys/devices/system/cpu/cpufreq", "policy0", true, false)
            .dir_entry("/sys/devices/system/cpu/cpufreq", "policy1", true, false)
            .file(
                "/sys/devices/system/cpu/cpufreq/policy0/affected_cpus",
                "0-3\n",
            )
            .file(
                "/sys/devices/system/cpu/cpufreq/policy0/scaling_cur_freq",
                "2000000\n",
            )
            .file(
                "/sys/devices/system/cpu/cpufreq/policy1/affected_cpus",
                "4-5\n",
            )
            .file(
                "/sys/devices/system/cpu/cpufreq/policy1/scaling_cur_freq",
                "1000000\n",
            );
        fixture
    }

    #[test]
    fn frequency_is_weighted_average_over_policies() {
        let fixture = cpufreq_fixture();
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        // (4 CPUs × 2000 MHz + 2 CPUs × 1000 MHz) / 6 = 1666.67 MHz
        let mhz = sensors.sample_frequency_mhz(&fixture).unwrap();
        assert!((mhz - 1666.667).abs() < 0.01, "got {mhz}");
    }

    #[test]
    fn frequency_none_when_policies_report_zero() {
        let mut fixture = cpufreq_fixture();
        fixture.file(
            "/sys/devices/system/cpu/cpufreq/policy0/scaling_cur_freq",
            "0\n",
        );
        fixture.file(
            "/sys/devices/system/cpu/cpufreq/policy1/scaling_cur_freq",
            "0\n",
        );
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_frequency_mhz(&fixture), None);
    }

    #[test]
    fn frequency_none_without_cpufreq_dir() {
        let fixture = FixtureSys::default();
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_frequency_mhz(&fixture), None);
    }

    fn hwmon_fixture() -> FixtureSys {
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/sys/class/hwmon", "hwmon0", true, false)
            .dir_entry("/sys/class/hwmon", "hwmon1", true, false)
            .file("/sys/class/hwmon/hwmon0/name", "coretemp\n")
            .file("/sys/class/hwmon/hwmon0/temp1_input", "63000\n")
            .file("/sys/class/hwmon/hwmon0/temp1_label", "Package id 0\n")
            .file("/sys/class/hwmon/hwmon0/temp2_input", "55000\n")
            .file("/sys/class/hwmon/hwmon0/temp2_label", "Core 0\n")
            .file("/sys/class/hwmon/hwmon1/name", "nvme\n")
            .file("/sys/class/hwmon/hwmon1/temp1_input", "49000\n")
            // The hwmon inner listing is what the walker sees; temp inputs are
            // regular files (not directories).
            .dir_entry("/sys/class/hwmon/hwmon0", "name", false, false)
            .dir_entry("/sys/class/hwmon/hwmon0", "temp1_input", false, false)
            .dir_entry("/sys/class/hwmon/hwmon0", "temp1_label", false, false)
            .dir_entry("/sys/class/hwmon/hwmon0", "temp2_input", false, false)
            .dir_entry("/sys/class/hwmon/hwmon0", "temp2_label", false, false)
            .dir_entry("/sys/class/hwmon/hwmon1", "name", false, false)
            .dir_entry("/sys/class/hwmon/hwmon1", "temp1_input", false, false);
        fixture
    }

    #[test]
    fn temperature_prefers_package_sensor_and_skips_non_cpu() {
        let fixture = hwmon_fixture();
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_temperature_c(&fixture), Some(63.0));
    }

    #[test]
    fn temperature_ignores_impossible_values_and_falls_back_to_core() {
        let mut fixture = hwmon_fixture();
        fixture.file("/sys/class/hwmon/hwmon0/temp1_input", "-100000\n");
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        // Package reading is out of range; the per-core sensor is the fallback.
        assert_eq!(sensors.sample_temperature_c(&fixture), Some(55.0));
    }

    #[test]
    fn temperature_none_without_cpu_hwmon() {
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/sys/class/hwmon", "hwmon0", true, false)
            .file("/sys/class/hwmon/hwmon0/name", "nvme\n")
            .file("/sys/class/hwmon/hwmon0/temp1_input", "49000\n")
            .dir_entry("/sys/class/hwmon/hwmon0", "name", false, false)
            .dir_entry("/sys/class/hwmon/hwmon0", "temp1_input", false, false);
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_temperature_c(&fixture), None);
    }

    #[test]
    fn rapl_watts_delta_math_and_reset_clamp() {
        // 250_000 µJ over 50_000 µs (50 ms) = 5 W.
        assert!((rapl_watts(1_000_000, 1_250_000, 50_000) - 5.0).abs() < 0.001);
        // Counter reset (read lower than baseline) clamps to 0 W.
        assert_eq!(rapl_watts(1_000_000, 100, 50_000), 0.0);
        // Zero window guards against divide-by-zero.
        assert_eq!(rapl_watts(1_000_000, 1_250_000, 0), 0.0);
    }

    fn rapl_fixture() -> FixtureSys {
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/sys/class/powercap", "intel-rapl:0", true, false)
            .dir_entry("/sys/class/powercap", "intel-rapl:0:0", true, false)
            .file("/sys/class/powercap/intel-rapl:0/name", "package-0\n")
            .file(
                "/sys/class/powercap/intel-rapl:0/max_energy_range_uj",
                "262143328850\n",
            )
            .file("/sys/class/powercap/intel-rapl:0:0/name", "core\n");
        fixture
    }

    #[test]
    fn power_discovers_package_zone_and_ignores_subzones_and_mmio() {
        let mut fixture = rapl_fixture();
        fixture
            .dir_entry("/sys/class/powercap", "intel-rapl-mmio:0", true, false)
            .file("/sys/class/powercap/intel-rapl-mmio:0/name", "package-0\n")
            .file(
                "/sys/class/powercap/intel-rapl-mmio:0/max_energy_range_uj",
                "262143328850\n",
            );
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        // Only the non-MMIO package zone is kept (core sub-zone + MMIO dropped).
        let layout = sensors.layout().unwrap();
        assert_eq!(layout.power_zones.len(), 1);
        assert_eq!(
            layout.power_zones[0]
                .dir
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "intel-rapl:0"
        );
    }

    #[test]
    fn power_none_without_rapl() {
        let fixture = FixtureSys::default();
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_power_w(&fixture), None);
    }

    #[test]
    fn hwmon_and_powercap_entries_are_symlinks_and_still_discovered() {
        // On real systems /sys/class/hwmon/* and /sys/class/powercap/* are
        // symlinks to the device directories; discovery must follow them.
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/sys/class/hwmon", "hwmon0", false, true)
            .file("/sys/class/hwmon/hwmon0/name", "coretemp\n")
            .file("/sys/class/hwmon/hwmon0/temp1_input", "63000\n")
            .file("/sys/class/hwmon/hwmon0/temp1_label", "Package id 0\n")
            .dir_entry("/sys/class/hwmon/hwmon0", "temp1_input", false, false)
            .dir_entry("/sys/class/powercap", "intel-rapl:0", false, true)
            .file("/sys/class/powercap/intel-rapl:0/name", "package-0\n")
            .file(
                "/sys/class/powercap/intel-rapl:0/max_energy_range_uj",
                "262143328850\n",
            );
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_temperature_c(&fixture), Some(63.0));
        // Zone discovered (via symlink); power is None only because the
        // root-only `energy_uj` counter is absent in this fixture.
        assert_eq!(sensors.layout().unwrap().power_zones.len(), 1);
    }

    #[test]
    fn power_none_when_energy_counter_unreadable() {
        // Package zone discovered but `energy_uj` is absent (the root-only case
        // on current kernels) → no power.
        let fixture = rapl_fixture();
        let mut sensors = CpuSensors::default();
        sensors.discover(&fixture);
        assert_eq!(sensors.sample_power_w(&fixture), None);
    }

    #[test]
    fn cpu_list_parser_rejects_empty_input() {
        assert_eq!(crate::cpu::parse_cpu_list("  "), None);
        assert_eq!(
            crate::cpu::parse_cpu_list("1,3,5-7"),
            Some(vec![1, 3, 5, 6, 7])
        );
    }
}
