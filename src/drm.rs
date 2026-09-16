//! Process-level GPU telemetry from the DRM **fdinfo** interface (S8).
//!
//! For each process we inspect the fds it holds open that point at
//! `/dev/dri/*` and read the matching `/proc/<pid>/fdinfo/<fd>` file. The DRM
//! core and each driver emit a stable set of `key: value` lines described in
//! `Documentation/gpu/drm-usage-stats.rst`:
//!
//! * Identification — `drm-driver`, `drm-pdev` (PCI BDF), `drm-client-id`.
//! * Engine usage — two driver families, both parsed generically:
//!     - i915 / amdgpu: `drm-engine-<name>: <ns>` (cumulative busy ns).
//!     - xe: `drm-cycles-<name>: <busy>` + `drm-total-cycles-<name>: <total>`.
//! * Memory — `drm-resident-<region>:` / `drm-total-<region>:` (default bytes,
//!   optional `KiB`/`MiB` suffix). The deprecated amdgpu `drm-memory-*` aliases
//!   of `drm-resident-*` are **skipped** to avoid double counting.
//!
//! Engine *names* and memory *region* names differ per driver (i915:
//! `render/copy/video` + `local/stolen`; xe: `rcs/vcs/bcs/ccs` + `local/gtt`;
//! amdgpu: `gfx/compute/dma` + `vram/gtt/cpu`), so they are stored generically
//! rather than hardcoded. Utilization is derived from the delta between
//! samples kept in [`DrmSamplerState`]; the first sample has no baseline and
//! reports `utilization_pct: None` (explicit, never faked).
//!
//! All filesystem access goes through [`Sys`] so the parser and sampler are
//! fixture-testable. This module only *collects and normalizes* per-process GPU
//! telemetry — device attribution is S16's [`crate::gpu_map`] concern, and the
//! richer engine-level UX is a later UX stage.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::domain::ProcessIdentity;
use crate::system::Sys;

/// One GPU device used by a process, keyed by its PCI BDF (from `drm-pdev`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmProcessGpu {
    /// PCI bus:device.function, the stable device identity.
    pub bdf: String,
    /// DRM driver name (`i915`, `xe`, `amdgpu`, …).
    pub driver: String,
    /// Number of DRM fds the process holds for this device.
    pub client_count: u32,
    /// Sum of `drm-resident-<region>` across all regions and fds (bytes).
    pub resident_bytes: u64,
    /// Sum of `drm-total-<region>` across all regions and fds (bytes).
    pub total_bytes: u64,
    /// Per-engine usage for this device, with utilization where known.
    pub engines: Vec<DrmEngine>,
}

/// Usage of one engine (or engine class) of a device.
///
/// Exactly one of the two counter pairs is populated, depending on the driver
/// family: i915/amdgpu use `busy_ns`, xe uses `cycles_busy`/`cycles_total`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmEngine {
    pub name: String,
    /// Cumulative busy nanoseconds (i915 / amdgpu).
    pub busy_ns: Option<u64>,
    /// Busy cycles (xe).
    pub cycles_busy: Option<u64>,
    /// Total cycles (xe).
    pub cycles_total: Option<u64>,
    /// Engine capacity / parallelism, when the driver exposes it.
    pub capacity: Option<u32>,
    /// Utilization percentage, derived from the delta between samples.
    /// `None` on the first sample (no baseline) — never faked as zero.
    pub utilization_pct: Option<f64>,
}

/// Prior engine counters used to compute utilization deltas between samples.
#[derive(Default)]
pub struct DrmSamplerState {
    counters: HashMap<EngineKey, EngineCounter>,
}

#[derive(Hash, PartialEq, Eq)]
struct EngineKey {
    identity: ProcessIdentity,
    bdf: String,
    engine: String,
}

#[derive(Clone)]
struct EngineCounter {
    busy_ns: Option<u64>,
    cycles_busy: Option<u64>,
    cycles_total: Option<u64>,
    sampled_at: Option<Instant>,
}

impl DrmSamplerState {
    /// Drop counters for processes that no longer exist (exited/reused PIDs).
    pub fn retain(&mut self, live: impl Iterator<Item = ProcessIdentity>) {
        let live: std::collections::HashSet<_> = live.collect();
        self.counters.retain(|key, _| live.contains(&key.identity));
    }
}

/// Sample the process's DRM fdinfo entries and return per-device GPU usage.
///
/// Returns one [`DrmProcessGpu`] per PCI BDF the process has an open render
/// node for (empty when the process holds no DRM fd). `now` is the sample
/// timestamp, used for the busy-ns/wall-time utilization delta.
pub fn sample_process_gpus<S: Sys>(
    sys: &S,
    now: Instant,
    identity: ProcessIdentity,
    state: &mut DrmSamplerState,
) -> Vec<DrmProcessGpu> {
    let pid_dir = Path::new("/proc").join(identity.pid.to_string());
    let fd_dir = pid_dir.join("fd");
    let fdinfo_dir = pid_dir.join("fdinfo");

    let Some(fd_entries) = sys.read_dir(&fd_dir) else {
        return Vec::new();
    };

    // Read the fdinfo of each open fd that points at a DRM device. Checking the
    // symlink target first avoids reading fdinfo for every non-DRM fd.
    let mut parsed: Vec<ParsedFd> = Vec::new();
    for entry in &fd_entries {
        if entry.is_dir {
            continue;
        }
        let Some(target) = sys.symlink_target(&fd_dir.join(&entry.name)) else {
            continue;
        };
        if !target.starts_with("/dev/dri/") {
            continue;
        }
        let Some(info) = sys.read_to_string(&fdinfo_dir.join(&entry.name)) else {
            continue;
        };
        if let Some(fd) = parse_fdinfo(&info) {
            parsed.push(fd);
        }
    }

    if parsed.is_empty() {
        return Vec::new();
    }

    // Aggregate the parsed fds per PCI BDF.
    let mut order: Vec<String> = Vec::new();
    let mut by_bdf: HashMap<String, AggGpu> = HashMap::new();
    for fd in parsed {
        let agg = by_bdf.entry(fd.bdf.clone()).or_insert_with(|| {
            order.push(fd.bdf.clone());
            AggGpu {
                driver: fd.driver,
                client_count: 0,
                resident: 0,
                total: 0,
                engines: HashMap::new(),
            }
        });
        agg.client_count += 1;
        agg.resident = agg.resident.saturating_add(fd.resident);
        agg.total = agg.total.saturating_add(fd.total);
        for (name, engine) in fd.engines {
            merge_engine(&mut agg.engines, &name, &engine);
        }
    }

    let mut result: Vec<DrmProcessGpu> = Vec::with_capacity(order.len());
    for bdf in &order {
        let agg = by_bdf.get_mut(bdf).expect("bdf present in map");
        let mut engines: Vec<DrmEngine> = Vec::with_capacity(agg.engines.len());
        for (name, engine) in agg.engines.iter_mut() {
            let utilization = engine_utilization(
                name,
                identity,
                bdf.as_str(),
                &mut state.counters,
                now,
                engine,
            );
            engines.push(DrmEngine {
                name: name.clone(),
                busy_ns: engine.busy_ns_value(),
                cycles_busy: engine.cycles_busy_value(),
                cycles_total: engine.cycles_total_value(),
                capacity: engine.capacity,
                utilization_pct: utilization,
            });
        }
        engines.sort_by(|a, b| a.name.cmp(&b.name));
        result.push(DrmProcessGpu {
            bdf: bdf.clone(),
            driver: agg.driver.clone(),
            client_count: agg.client_count,
            resident_bytes: agg.resident,
            total_bytes: agg.total,
            engines,
        });
    }
    result
}

/// Per-BDF accumulation of the parsed fds for one device.
struct AggGpu {
    driver: String,
    client_count: u32,
    resident: u64,
    total: u64,
    /// engine name -> accumulated counters (summed across the process's fds).
    engines: HashMap<String, AggEngine>,
}

/// Summed counters for one engine name across a process's fds to a device.
#[derive(Default)]
struct AggEngine {
    busy_ns_sum: u64,
    has_busy_ns: bool,
    cycles_busy_sum: u64,
    has_cycles_busy: bool,
    cycles_total_sum: u64,
    has_cycles_total: bool,
    capacity: Option<u32>,
}

impl AggEngine {
    fn busy_ns_value(&self) -> Option<u64> {
        self.has_busy_ns.then_some(self.busy_ns_sum)
    }
    fn cycles_busy_value(&self) -> Option<u64> {
        self.has_cycles_busy.then_some(self.cycles_busy_sum)
    }
    fn cycles_total_value(&self) -> Option<u64> {
        self.has_cycles_total.then_some(self.cycles_total_sum)
    }
}

/// Fold one parsed engine into the per-name accumulator for a device.
fn merge_engine(map: &mut HashMap<String, AggEngine>, name: &str, engine: &ParsedEngine) {
    let agg = map.entry(name.to_string()).or_default();
    if let Some(ns) = engine.busy_ns {
        agg.busy_ns_sum = agg.busy_ns_sum.saturating_add(ns);
        agg.has_busy_ns = true;
    }
    if let Some(busy) = engine.cycles_busy {
        agg.cycles_busy_sum = agg.cycles_busy_sum.saturating_add(busy);
        agg.has_cycles_busy = true;
    }
    if let Some(total) = engine.cycles_total {
        agg.cycles_total_sum = agg.cycles_total_sum.saturating_add(total);
        agg.has_cycles_total = true;
    }
    if let Some(cap) = engine.capacity {
        agg.capacity = Some(agg.capacity.map_or(cap, |c| c.max(cap)));
    }
}

/// Compute the utilization delta for one engine and store the new baseline in
/// `counters`. Returns `None` on the first sample (no prior baseline) — the
/// baseline is still stored so the next sample can compute a delta.
fn engine_utilization(
    name: &str,
    identity: ProcessIdentity,
    bdf: &str,
    counters: &mut HashMap<EngineKey, EngineCounter>,
    now: Instant,
    engine: &AggEngine,
) -> Option<f64> {
    let key = EngineKey {
        identity,
        bdf: bdf.to_string(),
        engine: name.to_string(),
    };
    let (busy_now, cycles_busy_now, cycles_total_now) = (
        engine.busy_ns_value(),
        engine.cycles_busy_value(),
        engine.cycles_total_value(),
    );

    // Read the prior baseline before inserting the current one.
    let prev = counters.get(&key).cloned();

    let utilization = match (busy_now, (cycles_busy_now, cycles_total_now)) {
        (Some(busy), _) if prev.as_ref().is_some_and(|p| p.busy_ns.is_some()) => {
            // i915 / amdgpu: busy ns over elapsed wall time.
            let prev = prev.unwrap();
            let elapsed_ns = now.duration_since(prev.sampled_at?).as_nanos() as u64;
            utilization_ns(busy, prev.busy_ns?, elapsed_ns)
        }
        (None, (Some(busy), Some(total)))
            if prev
                .as_ref()
                .is_some_and(|p| p.cycles_busy.is_some() && p.cycles_total.is_some()) =>
        {
            // xe: busy cycles over total cycles (wall-time independent).
            let prev = prev.unwrap();
            utilization_cycles(busy, prev.cycles_busy?, total, prev.cycles_total?)
        }
        _ => None,
    };

    counters.insert(
        key,
        EngineCounter {
            busy_ns: busy_now,
            cycles_busy: cycles_busy_now,
            cycles_total: cycles_total_now,
            sampled_at: Some(now),
        },
    );
    utilization
}

/// Utilization % from cumulative busy nanoseconds over elapsed wall time.
/// A counter that read lower than before (kernel reset) yields a zero delta.
fn utilization_ns(busy_now: u64, busy_prev: u64, elapsed_ns: u64) -> Option<f64> {
    if elapsed_ns == 0 {
        return None;
    }
    let delta = busy_now.saturating_sub(busy_prev);
    Some(delta as f64 / elapsed_ns as f64 * 100.0)
}

/// Utilization % from busy cycles over total cycles (xe scheme).
fn utilization_cycles(
    busy_now: u64,
    busy_prev: u64,
    total_now: u64,
    total_prev: u64,
) -> Option<f64> {
    let delta_busy = busy_now.saturating_sub(busy_prev);
    let delta_total = total_now.saturating_sub(total_prev);
    if delta_total == 0 {
        return None;
    }
    Some(delta_busy as f64 / delta_total as f64 * 100.0)
}

/// A fully parsed DRM fdinfo file. `None` when the file has no `drm-pdev` key
/// (i.e. the fd is not a DRM fd).
struct ParsedFd {
    bdf: String,
    driver: String,
    engines: Vec<(String, ParsedEngine)>,
    resident: u64,
    total: u64,
}

/// Engine counters parsed from one fdinfo file. The engine name is the key
/// of the [`ParsedFd::engines`] tuple and is not stored redundantly.
#[derive(Default)]
struct ParsedEngine {
    busy_ns: Option<u64>,
    cycles_busy: Option<u64>,
    cycles_total: Option<u64>,
    capacity: Option<u32>,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` file's DRM `key: value` lines.
fn parse_fdinfo(content: &str) -> Option<ParsedFd> {
    let mut bdf: Option<String> = None;
    let mut driver: Option<String> = None;
    let mut engines: HashMap<String, ParsedEngine> = HashMap::new();
    let mut resident: u64 = 0;
    let mut total: u64 = 0;

    for line in content.lines() {
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();

        if key == "drm-pdev" {
            bdf = Some(value.to_string());
        } else if key == "drm-driver" {
            driver = Some(value.to_string());
        } else if let Some(name) = key.strip_prefix("drm-engine-capacity-") {
            if let Some(n) = parse_u64(value) {
                if let Ok(cap) = u32::try_from(n) {
                    engines.entry(name.to_string()).or_default().capacity = Some(cap);
                }
            }
        } else if let Some(name) = key.strip_prefix("drm-total-cycles-") {
            if let Some(v) = parse_u64(value) {
                engines.entry(name.to_string()).or_default().cycles_total = Some(v);
            }
        } else if let Some(name) = key.strip_prefix("drm-cycles-") {
            if let Some(v) = parse_u64(value) {
                engines.entry(name.to_string()).or_default().cycles_busy = Some(v);
            }
        } else if let Some(name) = key.strip_prefix("drm-engine-") {
            if let Some(v) = parse_u64(value) {
                engines.entry(name.to_string()).or_default().busy_ns = Some(v);
            }
        } else if key.starts_with("drm-resident-") {
            if let Some(bytes) = value_to_bytes(value) {
                resident = resident.saturating_add(bytes);
            }
        } else if key.starts_with("drm-total-") {
            if let Some(bytes) = value_to_bytes(value) {
                total = total.saturating_add(bytes);
            }
        }
        // `drm-memory-*` (deprecated amdgpu aliases of `drm-resident-*`),
        // `drm-active-*`/`drm-purgeable-*`/`drm-shared-*`, and driver-specific
        // keys (`amd-*`, `pasid`, `i915-*`, …) are intentionally ignored.
    }

    let bdf = bdf?;
    let driver = driver.unwrap_or_default();
    let engine_vec = engines.into_iter().collect();
    Some(ParsedFd {
        bdf,
        driver,
        engines: engine_vec,
        resident,
        total,
    })
}

/// Parse a base-10 integer, ignoring any trailing unit token (e.g. `12345 ns`).
fn parse_u64(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Parse a byte value with an optional `KiB`/`MiB` suffix. The DRM ABI default
/// is bytes; the suffix (when present) selects the unit.
fn value_to_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(kib) = text.strip_suffix("KiB") {
        let n: u128 = kib.trim().parse().ok()?;
        return n.checked_mul(1024)?.try_into().ok();
    }
    if let Some(mib) = text.strip_suffix("MiB") {
        let n: u128 = mib.trim().parse().ok()?;
        return n.checked_mul(1024 * 1024)?.try_into().ok();
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::FixtureSys;

    const BDF: &str = "0000:02:00.0";

    /// Register a process with one fd (index 3) that opens a DRM render node,
    /// plus its fdinfo file.
    fn sys_with_drm_fd(sys: &mut FixtureSys, pid: u32, fdinfo: &str) {
        sys.dir_entry(&format!("/proc/{pid}/fd"), "3", false, true);
        sys.symlink(&format!("/proc/{pid}/fd/3"), "/dev/dri/renderD129");
        sys.file(&format!("/proc/{pid}/fdinfo/3"), fdinfo);
    }

    #[test]
    fn no_fdinfo_dir_means_no_gpu() {
        let sys = FixtureSys::default();
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(1, 100),
            &mut state,
        );
        assert!(gpus.is_empty());
    }

    #[test]
    fn non_drm_fd_is_ignored() {
        let mut sys = FixtureSys::default();
        let pid = 7u32;
        sys.dir_entry(&format!("/proc/{pid}/fd"), "3", false, true);
        sys.symlink(&format!("/proc/{pid}/fd/3"), "/usr/bin/llama-server");
        sys.file(
            &format!("/proc/{pid}/fdinfo/3"),
            "pos:\t0\nflags:\t02000002\n",
        );
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );
        assert!(gpus.is_empty());
    }

    #[test]
    fn i915_ns_format_parses_bdf_driver_engines_and_memory() {
        let mut sys = FixtureSys::default();
        let pid = 42u32;
        sys_with_drm_fd(
            &mut sys,
            pid,
            &format!(
                "pos:\t123\n\
                 drm-driver:\ti915\n\
                 drm-pdev:\t{BDF}\n\
                 drm-client-id:\t128\n\
                 drm-engine-render:\t1500000000 ns\n\
                 drm-engine-copy:\t100000000 ns\n\
                 drm-resident-local:\t104857600\n\
                 drm-resident-system:\t2097152\n\
                 drm-purgeable-local:\t0\n"
            ),
        );
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );

        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.bdf, BDF);
        assert_eq!(gpu.driver, "i915");
        assert_eq!(gpu.client_count, 1);
        assert_eq!(gpu.resident_bytes, 104857600 + 2097152);
        assert_eq!(gpu.engines.len(), 2);

        let render = gpu
            .engines
            .iter()
            .find(|e| e.name == "render")
            .expect("render engine");
        assert_eq!(render.busy_ns, Some(1_500_000_000));
        // First sample: no baseline, so utilization is explicitly None.
        assert_eq!(render.utilization_pct, None);

        let copy = gpu
            .engines
            .iter()
            .find(|e| e.name == "copy")
            .expect("copy engine");
        assert_eq!(copy.busy_ns, Some(100_000_000));
    }

    #[test]
    fn xe_cycles_format_and_first_sample_utilization_none() {
        let mut sys = FixtureSys::default();
        let pid = 42u32;
        sys_with_drm_fd(
            &mut sys,
            pid,
            &format!(
                "drm-driver:\txe\n\
                 drm-pdev:\t{BDF}\n\
                 drm-cycles-rcs:\t4000\n\
                 drm-total-cycles-rcs:\t10000\n\
                 drm-cycles-bcs:\t100\n\
                 drm-total-cycles-bcs:\t10000\n\
                 drm-resident-local:\t524288\n\
                 drm-resident-system:\t65536 KiB\n"
            ),
        );
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );

        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.driver, "xe");
        // 524288 bytes + 65536 KiB (65536 * 1024 bytes).
        assert_eq!(gpu.resident_bytes, 524288 + 65536 * 1024);
        let rcs = gpu
            .engines
            .iter()
            .find(|e| e.name == "rcs")
            .expect("rcs engine");
        assert_eq!(rcs.cycles_busy, Some(4000));
        assert_eq!(rcs.cycles_total, Some(10000));
        assert_eq!(rcs.utilization_pct, None);
    }

    #[test]
    fn amdgpu_skips_deprecated_memory_aliases() {
        let mut sys = FixtureSys::default();
        let pid = 42u32;
        sys_with_drm_fd(
            &mut sys,
            pid,
            &format!(
                "drm-driver:\tamdgpu\n\
                 drm-pdev:\t{BDF}\n\
                 pasid:\t3\n\
                 drm-resident-vram:\t209715200\n\
                 drm-resident-gtt:\t1048576\n\
                 drm-resident-cpu:\t4096\n\
                 drm-purgeable-vram:\t0\n\
                 drm-memory-vram:\t204800 KiB\n\
                 drm-memory-gtt: \t1024 KiB\n\
                 amd-evicted-vram:\t0 KiB\n\
                 amd-requested-vram:\t204800 KiB\n\
                 drm-engine-gfx:\t2000000000 ns\n"
            ),
        );
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );

        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.driver, "amdgpu");
        // Only the standard `drm-resident-*` keys count; `drm-memory-*` and
        // `amd-*` aliases are ignored to avoid double counting.
        assert_eq!(gpu.resident_bytes, 209715200 + 1048576 + 4096);
        assert_eq!(gpu.total_bytes, 0);
        let gfx = gpu
            .engines
            .iter()
            .find(|e| e.name == "gfx")
            .expect("gfx engine");
        assert_eq!(gfx.busy_ns, Some(2_000_000_000));
    }

    #[test]
    fn multiple_fds_to_same_bdf_are_aggregated() {
        let mut sys = FixtureSys::default();
        let pid = 42u32;
        let fdinfo = format!("drm-driver:\tamdgpu\ndrm-pdev:\t{BDF}\ndrm-resident-vram:\t1000\n");
        sys.dir_entry(&format!("/proc/{pid}/fd"), "3", false, true);
        sys.symlink(&format!("/proc/{pid}/fd/3"), "/dev/dri/renderD129");
        sys.file(&format!("/proc/{pid}/fdinfo/3"), fdinfo.clone());
        sys.dir_entry(&format!("/proc/{pid}/fd"), "9", false, true);
        sys.symlink(&format!("/proc/{pid}/fd/9"), "/dev/dri/renderD129");
        sys.file(&format!("/proc/{pid}/fdinfo/9"), fdinfo);
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].client_count, 2);
        assert_eq!(gpus[0].resident_bytes, 2000);
    }

    #[test]
    fn fdinfo_without_pdev_is_rejected() {
        // A DRM fdinfo always carries drm-pdev; without it we cannot identify
        // the device, so the fd is treated as non-DRM.
        let mut sys = FixtureSys::default();
        let pid = 42u32;
        sys.dir_entry(&format!("/proc/{pid}/fd"), "3", false, true);
        sys.symlink(&format!("/proc/{pid}/fd/3"), "/dev/dri/renderD129");
        sys.file(
            &format!("/proc/{pid}/fdinfo/3"),
            "drm-driver:\tamdgpu\ndrm-engine-gfx:\t5 ns\n",
        );
        let mut state = DrmSamplerState::default();
        let gpus = sample_process_gpus(
            &sys,
            Instant::now(),
            ProcessIdentity::new(pid, 100),
            &mut state,
        );
        assert!(gpus.is_empty());
    }

    #[test]
    fn utilization_ns_delta_math() {
        // 50% of 2_000_000_000 ns (2 s) busy.
        let pct = utilization_ns(1_000_000_000, 0, 2_000_000_000).unwrap();
        assert!((pct - 50.0).abs() < 1e-9);
        // Counter reset (read lower) yields a zero delta, not negative.
        let pct = utilization_ns(100, 1000, 2_000_000_000).unwrap();
        assert_eq!(pct, 0.0);
        // Zero elapsed wall time is undefined.
        assert_eq!(utilization_ns(100, 0, 0), None);
    }

    #[test]
    fn utilization_cycles_delta_math() {
        // (6000-4000) / (10000-9000) = 2000/1000 = 200% (multi-engine sum).
        let pct = utilization_cycles(6000, 4000, 10000, 9000).unwrap();
        assert!((pct - 200.0).abs() < 1e-9);
        // No total progress → undefined.
        assert_eq!(utilization_cycles(5000, 4000, 9000, 9000), None);
        // Counter reset yields zero busy delta.
        let pct = utilization_cycles(100, 4000, 10000, 9000).unwrap();
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn second_sample_yields_utilization_and_monotonic_reset_is_clamped() {
        let pid = 42u32;
        let identity = ProcessIdentity::new(pid, 100);

        // First sample establishes the baseline.
        let mut sys1 = FixtureSys::default();
        sys_with_drm_fd(
            &mut sys1,
            pid,
            &format!("drm-driver:\txe\ndrm-pdev:\t{BDF}\ndrm-cycles-rcs:\t1000\ndrm-total-cycles-rcs:\t10000\n"),
        );
        let mut state = DrmSamplerState::default();
        let t0 = Instant::now();
        let g1 = sample_process_gpus(&sys1, t0, identity, &mut state);
        assert!(g1[0]
            .engines
            .iter()
            .find(|e| e.name == "rcs")
            .unwrap()
            .utilization_pct
            .is_none());

        // Second sample: busy 1000→3000, total 10000→20000 → (2000)/(10000)=20%.
        let mut sys2 = FixtureSys::default();
        sys_with_drm_fd(
            &mut sys2,
            pid,
            &format!("drm-driver:\txe\ndrm-pdev:\t{BDF}\ndrm-cycles-rcs:\t3000\ndrm-total-cycles-rcs:\t20000\n"),
        );
        let t1 = t0 + std::time::Duration::from_secs(2);
        let g2 = sample_process_gpus(&sys2, t1, identity, &mut state);
        let rcs = g2[0].engines.iter().find(|e| e.name == "rcs").unwrap();
        let pct = rcs.utilization_pct.unwrap();
        assert!((pct - 20.0).abs() < 1e-9);

        // Third sample with a reset counter (busy reads lower) → 0%, not negative.
        let mut sys3 = FixtureSys::default();
        sys_with_drm_fd(
            &mut sys3,
            pid,
            &format!("drm-driver:\txe\ndrm-pdev:\t{BDF}\ndrm-cycles-rcs:\t50\ndrm-total-cycles-rcs:\t21000\n"),
        );
        let t2 = t1 + std::time::Duration::from_secs(2);
        let g3 = sample_process_gpus(&sys3, t2, identity, &mut state);
        let rcs3 = g3[0].engines.iter().find(|e| e.name == "rcs").unwrap();
        assert_eq!(rcs3.utilization_pct, Some(0.0));
    }

    #[test]
    fn retain_drops_exited_process_counters() {
        let pid = 42u32;
        let identity = ProcessIdentity::new(pid, 100);
        let mut sys = FixtureSys::default();
        sys_with_drm_fd(
            &mut sys,
            pid,
            &format!("drm-driver:\txe\ndrm-pdev:\t{BDF}\ndrm-cycles-rcs:\t10\ndrm-total-cycles-rcs:\t100\n"),
        );
        let mut state = DrmSamplerState::default();
        let _ = sample_process_gpus(&sys, Instant::now(), identity, &mut state);
        assert!(!state.counters.is_empty());

        // No live process with this identity → counter pruned.
        state.retain(std::iter::empty());
        assert!(state.counters.is_empty());
    }

    #[test]
    fn unknown_and_driver_prefixed_keys_are_ignored() {
        let parsed = parse_fdinfo(
            "drm-driver:\tamdgpu\n\
             drm-pdev:\t0000:01:00.0\n\
             i915-some-thing:\t1\n\
             random-key:\tfoo\n\
             : no-colon-line\n",
        )
        .expect("has drm-pdev");
        assert_eq!(parsed.driver, "amdgpu");
        assert_eq!(parsed.engines.len(), 0);
        assert_eq!(parsed.resident, 0);
    }

    #[test]
    fn value_to_bytes_handles_units() {
        assert_eq!(value_to_bytes("1024"), Some(1024));
        assert_eq!(value_to_bytes("2 KiB"), Some(2048));
        assert_eq!(value_to_bytes("3 MiB"), Some(3 * 1024 * 1024));
        assert_eq!(value_to_bytes("abc"), None);
    }

    /// Live smoke: enumerate any process that currently holds a DRM fd. Prints
    /// what it finds; always passes (a headless/quiet host may have none).
    #[test]
    #[ignore]
    fn drm_real_system_smoke() {
        use crate::system::RealSys;
        let sys = RealSys;
        let root = Path::new("/proc");
        let Some(entries) = sys.read_dir(root) else {
            eprintln!("cannot read /proc");
            return;
        };
        let mut found = 0;
        for entry in entries {
            if !entry.name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let pid: u32 = match entry.name.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let identity = ProcessIdentity::new(pid, 0);
            let mut state = DrmSamplerState::default();
            let gpus = sample_process_gpus(&sys, Instant::now(), identity, &mut state);
            if !gpus.is_empty() {
                found += 1;
                for gpu in &gpus {
                    eprintln!(
                        "pid={pid} bdf={} driver={} clients={} resident={}B engines={}",
                        gpu.bdf,
                        gpu.driver,
                        gpu.client_count,
                        gpu.resident_bytes,
                        gpu.engines.len()
                    );
                }
            }
        }
        eprintln!("processes with DRM fds: {found}");
    }
}
