//! Normalized telemetry domain shared by the workers, the app snapshot and
//! the UI. Rendering code consumes these structures without knowing which
//! vendor API, kernel interface or HTTP endpoint produced a value.
//!
//! Re-exports the per-subsystem telemetry structs so consumers (and the UI)
//! import from this module only.

pub use crate::drm::DrmProcessGpu;
pub use crate::llama::{LlmSlotInfo, LlmStats};
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
/// Upper bound on concurrently monitored llama.cpp servers. Bound the worker's
/// per-cycle work and the publish channel so a pathological discovery result
/// cannot cause unbounded polling.
pub const MAX_MONITORED_SERVERS: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct ProcessStats {
    pub pid: u32,
    pub program: String,
    pub command: String,
    pub cpu_pct: f64,
    pub memory_bytes: u64,
    /// Thread count from `/proc/<pid>/status`, re-read every process refresh
    /// because it is dynamic. `None` when the counter could not be read on
    /// this cycle (unknown — never a fabricated substitute); the next refresh
    /// retries.
    pub threads: Option<usize>,
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

    /// `(gpu_instance_id, compute_instance_id)` when this is a MIG child whose
    /// UUID encodes them in the older NVIDIA slash form
    /// `MIG-<GPU-UUID>/<GI>/<CI>`. A modern opaque `MIG-<uuid>` does not encode
    /// the ids, so this is `None` there — and for the synthetic
    /// `<parent>#mig-<slot>` fallback — never a fabricated pair.
    pub fn mig_instance(&self) -> Option<(u32, u32)> {
        self.uuid.as_deref().and_then(mig_instance_of_mig_uuid)
    }

    /// The physical parent's GPU UUID when this is a MIG child. For the older
    /// slash form `MIG-<GPU-UUID>/<GI>/<CI>` this strips the `MIG-` prefix and
    /// the instance fields; for a synthetic fallback key
    /// (`<parent-uuid>#mig-<slot>`) it strips the suffix. A modern opaque
    /// `MIG-<uuid>` does not encode its parent, so this is `None`; the parent
    /// association comes from the NVML device topology, not the UUID string.
    pub fn mig_parent_uuid(&self) -> Option<String> {
        let value = self.uuid.as_deref()?;
        if let Some(stripped) = value.strip_prefix("MIG-") {
            // `MIG-<parent>/<GI>/<CI>`: parent is everything before the final
            // two slash-separated NUMERIC fields; anything else (e.g. a modern
            // opaque UUID that happens to contain a slash) encodes no parent.
            let (without_ci, ci) = stripped.rsplit_once('/')?;
            let (parent, gi) = without_ci.rsplit_once('/')?;
            let numeric = gi.parse::<u32>().is_ok() && ci.parse::<u32>().is_ok();
            return (numeric && !parent.is_empty()).then(|| parent.to_string());
        }
        value
            .rfind("#mig-")
            .map(|pos| value[..pos].to_string())
            .filter(|parent| !parent.is_empty())
    }
}

/// Normalize a PCI BDF string to the canonical 4-digit domain lower-case form
/// (`0000:01:00.0`). Some vendor APIs (NVML) report an 8-digit domain
/// (`00000000:01:00.0`) and callers may supply upper-case hex; lower-casing and
/// collapsing the domain at the ingress point keeps identity matching
/// consistent across providers, against `lspci` output and for user-supplied
/// selectors. 12-character BDFs are already canonical except for case, as are
/// domains of `0x10000` or higher (not representable in 4 digits) and strings
/// that are not BDFs.
pub(crate) fn normalize_pci_bdf(value: &str) -> String {
    let value = value.to_ascii_lowercase();
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
    value
}

/// Parse and validate a PCI BDF string, returning its canonical form.
///
/// Accepts both the 4-digit (`0000:01:00.0`) and the 8-digit
/// (`00000000:01:00.0`) domain form; the result is put through
/// [`normalize_pci_bdf`], so a domain below `0x10000` comes back as 4 digits
/// while a domain at or above it keeps the 8-digit form. The function digit
/// must be `0`–`7`; anything else returns `None`. This is the single BDF
/// grammar shared by GPU discovery and process/GPU attribution, so the two
/// cannot drift apart on which domain forms they accept.
pub(crate) fn parse_pci_bdf(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    let valid = match bytes.len() {
        12 => {
            (0..4).all(|i| is_hex(bytes[i]))
                && bytes[4] == b':'
                && (5..7).all(|i| is_hex(bytes[i]))
                && bytes[7] == b':'
                && (8..10).all(|i| is_hex(bytes[i]))
                && bytes[10] == b'.'
                && (b'0'..=b'7').contains(&bytes[11])
        }
        16 => {
            (0..8).all(|i| is_hex(bytes[i]))
                && bytes[8] == b':'
                && (9..11).all(|i| is_hex(bytes[i]))
                && bytes[11] == b':'
                && (12..14).all(|i| is_hex(bytes[i]))
                && bytes[14] == b'.'
                && (b'0'..=b'7').contains(&bytes[15])
        }
        _ => false,
    };
    valid.then(|| normalize_pci_bdf(&text.to_ascii_lowercase()))
}

/// True when `value` is an NVIDIA MIG device UUID.
///
/// The `MIG-` prefix is the only universal marker: modern drivers return
/// opaque `MIG-<uuid>` identities (which do not encode the GPU/compute
/// instance ids), and older drivers used `MIG-<GPU-UUID>/<GI>/<CI>`. Physical
/// GPU UUIDs (`GPU-...`) are never MIG, and a bare `MIG-` is not an identity.
pub(crate) fn is_mig_uuid(value: &str) -> bool {
    value
        .strip_prefix("MIG-")
        .is_some_and(|rest| !rest.is_empty())
}

/// Extract `(gpu_instance_id, compute_instance_id)` from a MIG UUID, but only
/// when it carries them in the older NVIDIA slash form
/// `MIG-<GPU-UUID>/<GI>/<CI>`. A modern opaque `MIG-<uuid>` yields `None`: the
/// ids are not encoded, and inventing them would mis-attribute processes.
pub(crate) fn mig_instance_of_mig_uuid(value: &str) -> Option<(u32, u32)> {
    let rest = value.strip_prefix("MIG-")?;
    let (without_ci, ci) = rest.rsplit_once('/')?;
    let (parent, gi) = without_ci.rsplit_once('/')?;
    if parent.is_empty() || gi.is_empty() || ci.is_empty() {
        return None;
    }
    let gi = gi.parse::<u32>().ok()?;
    let ci = ci.parse::<u32>().ok()?;
    Some((gi, ci))
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
        // Canonicalize only the case of the vendor prefix; the UUID body is
        // left as typed (NVML emits upper-case hex, and silently lower-casing
        // every byte could match a different, non-existent identity).
        if let Some(rest) = value.strip_prefix("gpu-") {
            return Self::Uuid(format!("GPU-{rest}"));
        }
        if let Some(rest) = value.strip_prefix("mig-") {
            return Self::Uuid(format!("MIG-{rest}"));
        }
        if value.starts_with("GPU-") || is_mig_uuid(value) {
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

/// Stable identity key for a monitored llama.cpp server.
///
/// The key is a normalized endpoint: scheme and host are lower-cased by the URL
/// parser, the default port is dropped, the path is kept (with a single
/// canonical form for a trailing slash) and the query is kept so two endpoints
/// that only differ by routing/query are not silently merged. Userinfo
/// (credentials) is deliberately removed: it must never define identity nor
/// appear in a key that might be logged or displayed. Two different host names
/// stay distinct even if they resolve to the same address; only syntactically
/// identical endpoints collapse. Falls back to the trimmed string when the
/// endpoint does not parse.
pub(crate) fn server_key(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    let Ok(mut url) = reqwest::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let _ = url.set_password(None);
    let _ = url.set_username("");
    // `Url` already lower-cases the host and drops a default port. Normalize a
    // trailing slash on a non-root path so `…/v1` and `…/v1/` share a key.
    let path = url.path().to_string();
    if path != "/" && path.ends_with('/') {
        url.set_path(path.trim_end_matches('/'));
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_prefers_bdf_over_uuid() {
        let id = DeviceId::new(Some("0000:01:00.0".into()), Some("GPU-abc".into()));
        assert_eq!(id.key(), "0000:01:00.0");

        let uuid_only = DeviceId::new(None, Some("GPU-abc".into()));
        assert_eq!(uuid_only.key(), "GPU-abc");

        assert_eq!(uuid_only, DeviceId::new(None, Some("GPU-abc".into())));
    }

    #[test]
    fn server_key_normalizes_without_merging_distinct_hosts() {
        // Same endpoint, different surface forms -> one identity.
        assert_eq!(
            server_key("http://127.0.0.1:8081"),
            server_key("http://127.0.0.1:8081/")
        );
        assert_eq!(
            server_key("HTTP://LocalHost:8081"),
            server_key("http://localhost:8081")
        );
        assert_eq!(server_key("http://h:80"), server_key("http://h"));
        assert_eq!(server_key("http://h/v1/"), server_key("http://h/v1"));
        assert_eq!(
            server_key("http://[::1]:8081"),
            server_key("http://[::1]:8081/")
        );
        // Different hosts are never merged.
        assert_ne!(
            server_key("http://127.0.0.1:8081"),
            server_key("http://localhost:8081")
        );
        // Credentials are not part of identity.
        assert_eq!(
            server_key("http://user:pass@h:8081"),
            server_key("http://h:8081")
        );
        // Query is preserved, so distinct routing stays distinct.
        assert_ne!(
            server_key("http://h:8081/?token=a"),
            server_key("http://h:8081/?token=b")
        );
        // Unparseable endpoints fall back to the trimmed text.
        assert_eq!(server_key("  not a url  "), "not a url");
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
    fn normalize_pci_bdf_lowercases_uppercase_hex() {
        // A user-supplied selector in upper case must reach the same identity
        // as the lower-case sysfs key.
        assert_eq!(normalize_pci_bdf("0000:AB:CD.0"), "0000:ab:cd.0");
        assert_eq!(normalize_pci_bdf("0000:AB:CD.E"), "0000:ab:cd.e");
        assert_eq!(
            DeviceId::new(Some("0000:AB:CD.0".into()), None).key(),
            DeviceId::new(Some("0000:ab:cd.0".into()), None).key()
        );
    }

    #[test]
    fn parse_pci_bdf_accepts_and_normalizes_both_domain_forms() {
        // Both domain forms are valid input; the canonical form is BDF keyed.
        assert_eq!(
            parse_pci_bdf("0000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            parse_pci_bdf("00000000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        // A domain >= 0x10000 is not representable in 4 digits: keep 8.
        assert_eq!(
            parse_pci_bdf("00010000:01:00.0").as_deref(),
            Some("00010000:01:00.0")
        );
        // Input is lowercased.
        assert_eq!(
            parse_pci_bdf("0000:AB:CD.0").as_deref(),
            Some("0000:ab:cd.0")
        );
        // Function digit must be 0-7 and the shape must be exact.
        assert_eq!(parse_pci_bdf("0000:01:00.8"), None);
        assert_eq!(parse_pci_bdf("00000000:01:00.8"), None);
        assert_eq!(parse_pci_bdf("0000:01:00"), None);
        assert_eq!(parse_pci_bdf("0000-01-00-0"), None);
        assert_eq!(parse_pci_bdf("zzzz:01:00.0"), None);
        assert_eq!(parse_pci_bdf(""), None);
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
        // Modern opaque MIG identity: the `MIG-` prefix is the only marker.
        assert!(is_mig_uuid("MIG-1a2b3c4d5e6f7890abcdef0123456789"));
        assert!(is_mig_uuid("MIG-1a2b3c4d-5e6f-7890-abcd-ef0123456789"));
        // Legacy slash form is also a MIG identity.
        assert!(is_mig_uuid(
            "MIG-GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789/1/2"
        ));
        // Physical GPU is never MIG; a bare prefix is not an identity.
        assert!(!is_mig_uuid("GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789"));
        assert!(!is_mig_uuid("MIG-"));
        assert!(!is_mig_uuid("MIG"));
        assert!(!is_mig_uuid(""));
    }

    #[test]
    fn mig_instance_and_parent_come_only_from_the_legacy_slash_form() {
        let legacy = DeviceId::new(None, Some("MIG-GPU-parent/1/2".into()));
        assert_eq!(legacy.mig_instance(), Some((1, 2)));
        assert_eq!(legacy.mig_parent_uuid().as_deref(), Some("GPU-parent"));
        assert_eq!(legacy.key(), "MIG-GPU-parent/1/2");

        // A modern opaque MIG UUID must not fabricate GI/CI or a parent.
        let opaque = DeviceId::new(None, Some("MIG-1a2b3c4d5e6f7890abcdef0123456789".into()));
        assert!(is_mig_uuid(opaque.key()));
        assert_eq!(opaque.mig_instance(), None);
        assert_eq!(opaque.mig_parent_uuid(), None);

        // The synthetic fallback keeps its parent and carries no instance ids.
        let synthetic = DeviceId::new(None, Some("GPU-parent#mig-3".into()));
        assert_eq!(synthetic.mig_instance(), None);
        assert_eq!(synthetic.mig_parent_uuid().as_deref(), Some("GPU-parent"));

        // A non-numeric slash tail is not a legacy instance tuple: no parent.
        let bogus = DeviceId::new(None, Some("MIG-a/b/c".into()));
        assert_eq!(bogus.mig_instance(), None);
        assert_eq!(bogus.mig_parent_uuid(), None);
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
        // A modern opaque MIG UUID is a UUID selector, never a PCI BDF.
        assert_eq!(
            GpuSelector::parse("MIG-1a2b3c4d5e6f7890abcdef0123456789"),
            GpuSelector::Uuid("MIG-1a2b3c4d5e6f7890abcdef0123456789".to_string())
        );
        assert!(matches!(
            GpuSelector::parse("MIG-1a2b3c4d5e6f7890abcdef0123456789"),
            GpuSelector::Uuid(_)
        ));
        // Legacy slash form too.
        assert_eq!(
            GpuSelector::parse("MIG-GPU-111/1/0"),
            GpuSelector::Uuid("MIG-GPU-111/1/0".to_string())
        );
    }

    #[test]
    fn selector_parse_canonicalizes_only_the_vendor_prefix_case() {
        assert_eq!(
            GpuSelector::parse("gpu-abc"),
            GpuSelector::Uuid("GPU-abc".to_string())
        );
        assert_eq!(
            GpuSelector::parse("mig-1a2b3c4d5e6f7890abcdef0123456789"),
            GpuSelector::Uuid("MIG-1a2b3c4d5e6f7890abcdef0123456789".to_string())
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
    /// 1/5/15-minute load averages. `None` when `/proc/loadavg` could not be
    /// read (unknown, rendered `—`), which is distinct from a real load of
    /// `Some(0.0)`.
    pub load_one: Option<f64>,
    pub load_five: Option<f64>,
    pub load_fifteen: Option<f64>,
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
    /// Session generation of the server identity the `gpu_map` was computed
    /// for. The app applies `gpu_map` only when this matches its current
    /// mapping generation, so a snapshot produced for a previous server cannot
    /// overwrite the new state. `gpu`/`system` are applied regardless: they do
    /// not belong to the server session.
    pub mapping_generation: u64,
}

/// Compact per-server status for the multi-server overview and selector.
///
/// This carries only what the overview needs; the full [`LlmStats`] for the
/// selected server is rendered by the existing LLM panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerSummary {
    /// Stable identity key (`domain::server_key` of the endpoint).
    pub key: String,
    /// Configured/advertised endpoint (may contain credentials; the UI must
    /// redact it before display with `redact::safe_endpoint`).
    pub endpoint: String,
    /// Display label: the model name (sanitized) or the compact endpoint.
    pub label: String,
    pub connected: bool,
    pub reconnecting: bool,
    pub slot_count: u64,
    pub busy_slots: u64,
    pub context_used: Option<u64>,
    pub context_size: u64,
    pub generation_tps: Option<f64>,
}

/// Latest coherent snapshot held by the app and rendered by the UI.
#[derive(Clone, Debug, Default)]
pub struct DashboardSnapshot {
    pub gpu: GpuStats,
    pub system: SystemStats,
    /// Full telemetry of the currently selected server.
    pub llm: LlmStats,
    /// Compact status of every known server, in stable display order.
    pub servers: Vec<ServerSummary>,
    /// Identity key of the selected server (matches `servers[].key`).
    pub selected_server: String,
    /// Which GPU(s) the inference server process uses, when determined.
    pub gpu_map: GpuMapping,
}
