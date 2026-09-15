//! Normalized telemetry domain shared by the workers, the app snapshot and
//! the UI. Rendering code consumes these structures without knowing which
//! vendor API, kernel interface or HTTP endpoint produced a value.
//!
//! Re-exports the per-subsystem telemetry structs so consumers (and the UI)
//! import from this module only.

pub use crate::gpu::GpuStats;
pub use crate::llama::LlmStats;

/// Minimum UI/worker refresh interval.
pub const MIN_REFRESH_MS: u64 = 100;
/// Maximum UI/worker refresh interval.
pub const MAX_REFRESH_MS: u64 = 10_000;
/// Step used by the +/- refresh controls.
pub const REFRESH_STEP_MS: u64 = 100;
/// Floor for the LLM worker's /metrics poll. LLM token counters do not
/// change meaningfully faster than this, so fast UI refresh rates should not
/// multiply the HTTP polling overhead.
pub const MIN_LLM_POLL_MS: u64 = 250;

#[derive(Clone, Debug, Default)]
pub struct ProcessStats {
    pub pid: u32,
    pub program: String,
    pub command: String,
    pub cpu_pct: f64,
    pub memory_bytes: u64,
    pub threads: usize,
}

/// Stable identity for a GPU device, independent of enumeration order.
///
/// PCI BDF is the primary Linux selector (present for all PCI devices,
/// stable across reboots and enumeration changes). A vendor UUID (e.g.
/// NVIDIA's `GPU-...`) is kept as an optional secondary selector; the
/// human-readable name is never part of the identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub struct DeviceId {
    /// PCI bus:device.function (e.g. `0000:01:00.0`), when known.
    pub pci_bdf: Option<String>,
    /// Vendor-provided UUID, when the driver exposes one.
    pub uuid: Option<String>,
}

#[allow(dead_code)]
impl DeviceId {
    pub fn new(pci_bdf: Option<String>, uuid: Option<String>) -> Self {
        Self { pci_bdf, uuid }
    }

    /// Primary stable key for maps and equality: BDF, then UUID, then
    /// neither (devices with neither are compared as distinct by
    /// reference — callers must attach a BDF or UUID to track them).
    pub fn key(&self) -> &str {
        self.pci_bdf
            .as_deref()
            .or(self.uuid.as_deref())
            .unwrap_or("")
    }
}

/// A user-facing GPU selection, independent of enumeration order.
///
/// The UI and config store a [`GpuSelector`] rather than a bare NVML index:
/// the index is only a fallback for legacy configs and the auto-default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GpuSelector {
    /// No explicit selection — the first available NVIDIA GPU.
    #[default]
    Auto,
    /// Select by vendor UUID (e.g. `GPU-…`).
    Uuid(String),
    /// Select by PCI bus ID (e.g. `0000:01:00.0`).
    PciBusId(String),
    /// Legacy NVML enumeration index (0-based).
    Index(u32),
}

impl GpuSelector {
    /// Parse a raw selector string. Empty/blank → [`GpuSelector::Auto`].
    /// A purely numeric value is a legacy index; anything else is a UUID or
    /// PCI bus ID (whichever it happens to be).
    pub fn parse(raw: &str) -> Self {
        let value = raw.trim();
        if value.is_empty() {
            return Self::Auto;
        }
        if let Ok(index) = value.parse::<u32>() {
            return Self::Index(index);
        }
        if value.starts_with("GPU-") || value.starts_with("gpu-") {
            return Self::Uuid(value.to_string());
        }
        Self::PciBusId(value.to_string())
    }

    /// Resolve the selector against the discovered NVIDIA devices (in NVML
    /// order, by index). Returns the NVML index to sample, or `None` when the
    /// selection does not match any device. `Auto` yields the first device.
    pub fn resolve(&self, devices: &[(u32, Option<String>, Option<String>)]) -> Option<u32> {
        match self {
            Self::Auto => devices.first().map(|(index, _, _)| *index),
            Self::Index(index) => devices
                .iter()
                .find(|(i, _, _)| i == index)
                .map(|(i, _, _)| *i),
            Self::Uuid(uuid) => devices
                .iter()
                .find(|(_, u, _)| u.as_deref() == Some(uuid.as_str()))
                .map(|(i, _, _)| *i),
            Self::PciBusId(bdf) => devices
                .iter()
                .find(|(_, _, b)| b.as_deref() == Some(bdf.as_str()))
                .map(|(i, _, _)| *i),
        }
    }

    /// Canonical string form for config/CLI/UI. `Auto` is the empty string.
    pub fn as_string(&self) -> String {
        match self {
            Self::Auto => String::new(),
            Self::Index(index) => index.to_string(),
            Self::Uuid(uuid) | Self::PciBusId(uuid) => uuid.clone(),
        }
    }
}

impl GpuSelector {
    /// Trim and normalize: blank strings become [`GpuSelector::Auto`].
    pub fn sanitized(&self) -> Self {
        match self {
            Self::Uuid(uuid) => {
                let value = uuid.trim();
                if value.is_empty() {
                    Self::Auto
                } else {
                    Self::Uuid(value.to_string())
                }
            }
            Self::PciBusId(bdf) => {
                let value = bdf.trim();
                if value.is_empty() {
                    Self::Auto
                } else {
                    Self::PciBusId(value.to_string())
                }
            }
            other => other.clone(),
        }
    }
}

/// Explicit availability of a single metric value.
///
/// `None` fields that used to mean "unavailable" are replaced by this so
/// the UI can distinguish a real zero from a sensor that does not exist.
#[derive(Clone, Debug, PartialEq, Default)]
#[allow(dead_code)]
pub enum Metric<T> {
    Available(T),
    /// The device/driver does not expose this metric at all.
    #[default]
    Unsupported,
    /// Exposed, but not readable right now (reset, transient driver state).
    Unavailable,
    /// Not readable with the current user's permissions.
    PermissionDenied,
    /// Last known value, kept after the source started failing.
    Stale(T),
    /// The source returned an explicit error.
    Error(String),
}

#[allow(dead_code)]
impl<T> Metric<T> {
    pub fn available(value: T) -> Self {
        Self::Available(value)
    }

    pub const fn unsupported() -> Self {
        Self::Unsupported
    }

    pub const fn unavailable() -> Self {
        Self::Unavailable
    }

    pub const fn permission_denied() -> Self {
        Self::PermissionDenied
    }

    pub fn stale(value: T) -> Self {
        Self::Stale(value)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    /// The value when the metric carries one (available or stale).
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Available(value) | Self::Stale(value) => Some(value),
            _ => None,
        }
    }

    /// True when the metric has a usable value right now (not stale/error).
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    pub fn is_unavailable(&self) -> bool {
        !matches!(self, Self::Available(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_distinguishes_available_from_unavailable_states() {
        assert_eq!(Metric::available(42).value(), Some(&42));
        assert!(Metric::available(0).is_live());
        assert_eq!(Metric::<f64>::stale(1.5).value(), Some(&1.5));
        assert!(!Metric::<f64>::stale(1.5).is_live());

        for metric in [
            Metric::<u8>::unsupported(),
            Metric::<u8>::unavailable(),
            Metric::<u8>::permission_denied(),
            Metric::<u8>::error("no such sensor".to_string()),
        ] {
            assert!(metric.is_unavailable());
            assert_eq!(metric.value(), None);
        }
        assert_eq!(Metric::default(), Metric::<u8>::unsupported());
    }

    #[test]
    fn device_id_prefers_bdf_over_uuid() {
        let id = DeviceId::new(Some("0000:01:00.0".into()), Some("GPU-abc".into()));
        assert_eq!(id.key(), "0000:01:00.0");

        let uuid_only = DeviceId::new(None, Some("GPU-abc".into()));
        assert_eq!(uuid_only.key(), "GPU-abc");

        assert_eq!(uuid_only, DeviceId::new(None, Some("GPU-abc".into())));
    }

    #[test]
    fn gpu_selector_parses_and_resolves() {
        assert_eq!(GpuSelector::parse(""), GpuSelector::Auto);
        assert_eq!(GpuSelector::parse("  "), GpuSelector::Auto);
        assert_eq!(GpuSelector::parse("2"), GpuSelector::Index(2));
        assert_eq!(
            GpuSelector::parse("GPU-1a2b3c"),
            GpuSelector::Uuid("GPU-1a2b3c".to_string())
        );
        assert_eq!(
            GpuSelector::parse("0000:41:00.0"),
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
        assert_eq!(GpuSelector::default(), GpuSelector::Auto);

        let devices = vec![
            (
                0,
                Some("GPU-a".to_string()),
                Some("0000:01:00.0".to_string()),
            ),
            (
                1,
                Some("GPU-b".to_string()),
                Some("0000:41:00.0".to_string()),
            ),
        ];
        assert_eq!(GpuSelector::Auto.resolve(&devices), Some(0));
        assert_eq!(GpuSelector::Index(1).resolve(&devices), Some(1));
        assert_eq!(GpuSelector::Index(9).resolve(&devices), None);
        assert_eq!(
            GpuSelector::Uuid("GPU-b".to_string()).resolve(&devices),
            Some(1)
        );
        assert_eq!(
            GpuSelector::PciBusId("0000:01:00.0".to_string()).resolve(&devices),
            Some(0)
        );
        assert_eq!(
            GpuSelector::PciBusId("0000:99:00.0".to_string()).resolve(&devices),
            None
        );
        assert_eq!(GpuSelector::Auto.resolve(&[]), None);
    }
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub per_cpu_usage: Vec<f64>,
    pub cpu_topology: crate::cpu::CpuTopology,
    pub cpu_frequency_mhz: Option<f64>,
    pub cpu_temperature_c: Option<f64>,
    pub io_wait_pct: Option<f64>,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub processes: Vec<ProcessStats>,
}

/// Coherent per-cycle snapshot published by the fast worker.
#[derive(Clone, Debug, Default)]
pub struct FastSnapshot {
    pub gpu: GpuStats,
    pub system: SystemStats,
}

/// Latest coherent snapshot held by the app and rendered by the UI.
#[derive(Clone, Debug, Default)]
pub struct DashboardSnapshot {
    pub gpu: GpuStats,
    pub system: SystemStats,
    pub llm: LlmStats,
}
