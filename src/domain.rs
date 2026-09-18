//! Normalized telemetry domain shared by the workers, the app snapshot and
//! the UI. Rendering code consumes these structures without knowing which
//! vendor API, kernel interface or HTTP endpoint produced a value.
//!
//! Re-exports the per-subsystem telemetry structs so consumers (and the UI)
//! import from this module only.

pub use crate::drm::DrmProcessGpu;
pub use crate::llama::LlmStats;
pub use crate::providers::GpuStats;

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
    /// Process start time in epoch seconds (from /proc/<pid>/stat).
    /// Together with `pid` it forms a robust [`ProcessIdentity`] that
    /// survives kernel PID reuse.
    pub start_time: u64,
    /// Per-device GPU usage from DRM fdinfo (S8). One entry per PCI BDF the
    /// process has an open render node for; empty when the process holds no
    /// DRM fd (explicit, never faked).
    pub gpu: Vec<DrmProcessGpu>,
}

impl ProcessStats {
    /// Robust identity of this process instance.
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
    /// Total GPU-resident memory across all devices this process holds a
    /// render node for (S8). Zero when it holds none.
    pub fn gpu_bytes(&self) -> u64 {
        self.gpu.iter().map(|g| g.resident_bytes).sum()
    }
}

/// Robust identity for a running process.
///
/// The kernel recycles PIDs, so a bare PID can refer to an entirely
/// different process after a restart. Pairing it with the process start
/// time (`/proc/<pid>/stat` field 22, exposed here in epoch seconds) makes
/// the identity unique for the life of the process: a reused PID carries a
/// different start time.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcessIdentity {
    pub fn new(pid: u32, start_time: u64) -> Self {
        Self { pid, start_time }
    }
}

/// Stable identity for a GPU device, independent of enumeration order.
///
/// PCI BDF is the primary Linux selector (present for all PCI devices,
/// stable across reboots and enumeration changes). A vendor UUID (e.g.
/// NVIDIA's `GPU-...`) is kept as an optional secondary selector; the
/// human-readable name is never part of the identity.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DeviceId {
    /// PCI bus:device.function (e.g. `0000:01:00.0`), when known.
    pub pci_bdf: Option<String>,
    /// Vendor-provided UUID, when the driver exposes one.
    pub uuid: Option<String>,
}

impl DeviceId {
    pub fn new(pci_bdf: Option<String>, uuid: Option<String>) -> Self {
        Self {
            pci_bdf: pci_bdf.map(|value| normalize_pci_bdf(&value)),
            uuid,
        }
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

    /// `(gpu_instance_id, compute_instance_id)` when this is a MIG child
    /// whose UUID carries them (`MIG-GPU-<parent>-<gi>-<ci>`). Synthetic
    /// fallback keys carry no instance ids, so this is `None` for them.
    pub fn mig_instance(&self) -> Option<(u32, u32)> {
        let value = self.uuid.as_deref()?;
        if !is_mig_uuid(value) {
            return None;
        }
        let parts: Vec<&str> = value.rsplit('-').take(2).collect();
        let ci = parts[0].parse::<u32>().ok()?;
        let gi = parts[1].parse::<u32>().ok()?;
        Some((gi, ci))
    }

    /// The physical parent's GPU UUID when this is a MIG child. For a real
    /// MIG UUID (`MIG-GPU-<parent>-<gi>-<ci>`) this strips the `MIG-`
    /// prefix and the instance ids; for a synthetic fallback key
    /// (`<parent-uuid>#mig-<slot>`) it strips the suffix.
    pub fn mig_parent_uuid(&self) -> Option<String> {
        let value = self.uuid.as_deref()?;
        if let Some(stripped) = value.strip_prefix("MIG-") {
            let parent = stripped
                .rsplit('-')
                .skip(2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("-");
            return (!parent.is_empty()).then_some(parent);
        }
        value
            .rfind("#mig-")
            .map(|pos| value[..pos].to_string())
            .filter(|parent| !parent.is_empty())
    }
}

/// Normalize a PCI BDF string to the canonical 4-digit domain form
/// (`0000:01:00.0`). Some vendor APIs (NVML) report an 8-digit domain
/// (`00000000:01:00.0`) for the same physical device; normalizing at the
/// ingress point keeps identity matching consistent across providers and
/// against `lspci` output. 12-character BDFs are already canonical and pass
/// through unchanged, as do domains of `0x10000` or higher (not
/// representable in 4 digits) and strings that are not BDFs.
pub(crate) fn normalize_pci_bdf(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() == 16
        && bytes[8] == b':'
        && bytes[11] == b':'
        && bytes[14] == b'.'
        && (0..8).all(|i| bytes[i].is_ascii_hexdigit())
        && (9..11).all(|i| bytes[i].is_ascii_hexdigit())
        && (12..14).all(|i| bytes[i].is_ascii_hexdigit())
        && bytes[15].is_ascii_hexdigit()
    {
        if let Ok(domain) = u32::from_str_radix(&value[..8], 16) {
            if domain < 0x10000 {
                return format!("{domain:04x}{}", &value[8..]);
            }
        }
    }
    value.to_string()
}

/// True when `value` is an NVIDIA MIG device UUID: `MIG-GPU-<parent>-<gi>-<ci>`
/// — the `MIG-` prefix with a numeric compute-instance id and GPU-instance id
/// tail. Physical GPU UUIDs (`GPU-...`) are never MIG.
pub(crate) fn is_mig_uuid(value: &str) -> bool {
    if !value.starts_with("MIG-") {
        return false;
    }
    let parts: Vec<&str> = value.rsplit('-').take(2).collect();
    parts.len() == 2
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && !parts[0].is_empty()
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && !parts[1].is_empty()
}

/// Vendor family of a discovered GPU, independent of the provider backend.
///
/// This is a normalized domain concept (not discovery-specific); it is
/// defined here next to [`DeviceId`] and imported by the discovery module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Other,
    Unknown,
}

/// Evidence used to attribute a GPU to the inference server process.
///
/// Kept for diagnostics (S18) so a mapping decision can be explained:
/// [`NvmlCompute`] means the vendor API reported the process as a running
/// compute client of that device; [`RenderNodeFd`] means the process holds an
/// open DRM render node that resolves to the device's PCI BDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuEvidence {
    /// NVIDIA NVML reported the PID among the device's compute processes.
    NvmlCompute,
    /// The process opened a DRM render node that maps to the device BDF.
    RenderNodeFd,
}

/// One GPU attributed to the inference server process.
///
/// `device` (via [`DeviceId::key`]) is the stable identity; `name` and
/// `vendor` are display-only and never part of equality or dedup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedGpu {
    pub device: DeviceId,
    /// Human-readable card name (e.g. `card0`), for display only.
    pub name: String,
    pub vendor: GpuVendor,
    pub evidence: GpuEvidence,
}

#[allow(dead_code)] // consumed by the UI (S23) and diagnostics (S18)
impl MappedGpu {
    /// Stable display key: PCI BDF, else vendor UUID, else a stable
    /// unknown-marker so two unkeyed devices are not merged.
    pub fn key(&self) -> &str {
        if self.device.key().is_empty() {
            return "<unkeyed>";
        }
        self.device.key()
    }
}

/// The result of mapping the inference server process to the GPU(s) it uses.
///
/// Represented explicitly rather than guessed: a single device, several
/// devices, or `Unknown` when evidence is insufficient (e.g. the process is
/// not running locally or exposes no usable GPU evidence). `Default` is
/// [`GpuMapping::None`], matching a snapshot with no mapping computed yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GpuMapping {
    /// No mapping has been computed yet (e.g. before the first process scan).
    #[default]
    None,
    /// The server uses exactly one GPU.
    Single(MappedGpu),
    /// The server uses more than one GPU (e.g. tensor parallel across cards).
    Multi(Vec<MappedGpu>),
    /// Evidence is insufficient to determine which GPU(s) the server uses.
    Unknown,
}

#[allow(dead_code)] // consumed by the UI (S23) and diagnostics (S18)
impl GpuMapping {
    /// True when no GPU is attributed (either not computed or unknown).
    pub fn is_empty(&self) -> bool {
        matches!(self, GpuMapping::None | GpuMapping::Unknown)
    }

    /// Number of GPUs attributed, or `None` when unknown/not computed.
    pub fn len(&self) -> Option<usize> {
        match self {
            GpuMapping::None => Some(0),
            GpuMapping::Single(_) => Some(1),
            GpuMapping::Multi(devices) => Some(devices.len()),
            GpuMapping::Unknown => None,
        }
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
        if value.starts_with("GPU-") || value.starts_with("gpu-") || is_mig_uuid(value) {
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
                    Self::PciBusId(normalize_pci_bdf(value))
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

    #[test]
    fn normalize_pci_bdf_canonicalizes_nvml_eight_digit_domain() {
        // NVML reports the 8-digit domain; the canonical form is 4-digit.
        assert_eq!(normalize_pci_bdf("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_bdf("00000000:41:00.1"), "0000:41:00.1");
        // 12-character BDF is already canonical — identity.
        assert_eq!(normalize_pci_bdf("0000:01:00.0"), "0000:01:00.0");
        // Domain 0x10000 is the boundary: not representable in 4 digits.
        assert_eq!(normalize_pci_bdf("00010000:01:00.0"), "00010000:01:00.0");
        assert_eq!(normalize_pci_bdf("0000ffff:01:00.0"), "ffff:01:00.0");
        // Malformed input passes through unchanged.
        assert_eq!(normalize_pci_bdf("00000000:01:00"), "00000000:01:00");
        assert_eq!(normalize_pci_bdf("000000000:01:00.0"), "000000000:01:00.0");
        assert_eq!(normalize_pci_bdf("not-a-bdf"), "not-a-bdf");
        assert_eq!(normalize_pci_bdf(""), "");
    }

    #[test]
    fn device_id_normalizes_eight_digit_bdf_at_ingress() {
        // The same physical GPU must carry one identity regardless of the
        // BDF form its provider reported.
        let nvml_form = DeviceId::new(Some("00000000:01:00.0".into()), None);
        let sysfs_form = DeviceId::new(Some("0000:01:00.0".into()), None);
        assert_eq!(nvml_form.key(), sysfs_form.key());
        assert_eq!(nvml_form, sysfs_form);
    }

    #[test]
    fn gpu_selector_sanitizes_eight_digit_pci_bus_id() {
        // lspci form passes through unchanged.
        assert_eq!(
            GpuSelector::parse("0000:41:00.0").sanitized(),
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
        // NVML form is canonicalized so it resolves against the normalized
        // device enumeration.
        assert_eq!(
            GpuSelector::parse("00000000:41:00.0").sanitized(),
            GpuSelector::PciBusId("0000:41:00.0".to_string())
        );
    }

    fn mapped(bdf: &str) -> MappedGpu {
        MappedGpu {
            device: DeviceId::new(Some(bdf.into()), None),
            name: "card0".into(),
            vendor: GpuVendor::Amd,
            evidence: GpuEvidence::RenderNodeFd,
        }
    }

    #[test]
    fn gpu_mapping_states_are_explicit() {
        // Default is None (not yet computed) — distinct from Unknown.
        let default = GpuMapping::default();
        assert!(matches!(default, GpuMapping::None));
        assert_ne!(default, GpuMapping::Unknown);

        // None/Unknown report no attributed GPUs.
        assert!(default.is_empty());
        assert!(GpuMapping::Unknown.is_empty());
        assert_eq!(default.len(), Some(0));
        assert_eq!(GpuMapping::Unknown.len(), None);

        // Single and Multi report their device counts.
        assert_eq!(GpuMapping::Single(mapped("0000:01:00.0")).len(), Some(1));
        assert!(!GpuMapping::Single(mapped("0000:01:00.0")).is_empty());
        assert_eq!(
            GpuMapping::Multi(vec![mapped("0000:01:00.0"), mapped("0000:02:00.0")]).len(),
            Some(2)
        );

        // The display key is the stable BDF.
        assert_eq!(mapped("0000:01:00.0").key(), "0000:01:00.0");
    }

    #[test]
    fn mig_uuid_detection() {
        assert!(is_mig_uuid(
            "MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1-2"
        ));
        assert!(!is_mig_uuid("GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789"));
        assert!(!is_mig_uuid(
            "MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1"
        ));
        assert!(!is_mig_uuid("MIG-GPU-1a2b3c4d-x-2"));
        assert!(!is_mig_uuid("MIG-"));
    }

    #[test]
    fn mig_uuid_parses_instance_and_parent() {
        let id = DeviceId::new(
            None,
            Some("MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1-2".into()),
        );
        assert_eq!(id.mig_instance(), Some((1, 2)));
        assert_eq!(
            id.mig_parent_uuid().as_deref(),
            Some("GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789")
        );
        assert_eq!(id.key(), "MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1-2");
    }

    #[test]
    fn physical_uuid_is_never_mig() {
        let id = DeviceId::new(
            Some("0000:01:00.0".into()),
            Some("GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789".into()),
        );
        assert_eq!(id.mig_instance(), None);
        assert_eq!(id.mig_parent_uuid(), None);
    }

    #[test]
    fn selector_parse_mig_uuid_is_uuid() {
        assert_eq!(
            GpuSelector::parse("MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1-2"),
            GpuSelector::Uuid("MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789-1-2".to_string())
        );
    }
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub per_cpu_usage: Vec<f64>,
    pub cpu_topology: crate::cpu::CpuTopology,
    pub cpu_frequency_mhz: Option<f64>,
    pub cpu_temperature_c: Option<f64>,
    /// RAPL package power (W). `None` when no RAPL package zone is readable
    /// (`energy_uj` is root-only on current kernels).
    pub cpu_power_w: Option<f64>,
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
    /// Which GPU(s) the inference server process uses, when determined.
    pub gpu_map: GpuMapping,
}

/// Latest coherent snapshot held by the app and rendered by the UI.
#[derive(Clone, Debug, Default)]
pub struct DashboardSnapshot {
    pub gpu: GpuStats,
    pub system: SystemStats,
    pub llm: LlmStats,
    /// Which GPU(s) the inference server process uses, when determined.
    pub gpu_map: GpuMapping,
}
