use std::{collections::HashSet, path::PathBuf};

use crate::system::{RealSys, Sys};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CpuVendor {
    Intel,
    Amd,
    Other,
    #[default]
    Unknown,
}

impl CpuVendor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Intel => "Intel",
            Self::Amd => "AMD",
            Self::Other => "CPU",
            Self::Unknown => "CPU",
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CpuCoreKind {
    Performance,
    Efficiency,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuPhysicalCore {
    pub kind: CpuCoreKind,
    pub logical_cpus: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct CpuTopology {
    pub vendor: CpuVendor,
    pub model: String,
    pub logical_cpus: usize,
    pub physical_cores: Option<usize>,
    pub performance_cores: Option<usize>,
    pub efficiency_cores: Option<usize>,
    pub core_kinds: Vec<CpuCoreKind>,
    pub physical_core_groups: Vec<CpuPhysicalCore>,
}

impl CpuTopology {
    pub fn is_hybrid(&self) -> bool {
        self.core_kinds
            .iter()
            .any(|kind| matches!(kind, CpuCoreKind::Performance))
            && self
                .core_kinds
                .iter()
                .any(|kind| matches!(kind, CpuCoreKind::Efficiency))
    }

    pub fn performance_threads(&self) -> usize {
        self.core_kinds
            .iter()
            .filter(|kind| matches!(kind, CpuCoreKind::Performance))
            .count()
    }

    pub fn efficiency_threads(&self) -> usize {
        self.core_kinds
            .iter()
            .filter(|kind| matches!(kind, CpuCoreKind::Efficiency))
            .count()
    }
}

pub fn detect_cpu_topology(logical_cpus: usize) -> CpuTopology {
    detect_topology(logical_cpus, &RealSys)
}

pub fn detect_topology<S: Sys>(logical_cpus: usize, sys: &S) -> CpuTopology {
    let cpuinfo = sys
        .read_to_string(&PathBuf::from("/proc/cpuinfo"))
        .unwrap_or_default();
    let (vendor, model) = parse_cpu_identity(&cpuinfo);
    let core_groups = read_core_groups(logical_cpus, sys);
    let physical_cores = count_unique_groups(&core_groups);

    let mut core_kinds = detect_kernel_core_groups(logical_cpus, sys);

    // cpu_capacity is architecture-neutral Linux scheduler information. Prefer it
    // over model-name tables so future heterogeneous CPUs can work without an
    // OrsikTop update when the kernel exposes distinct capacities.
    if !has_both_core_kinds(&core_kinds) {
        if let Some(capacity_kinds) = detect_capacity_classes(logical_cpus, sys) {
            core_kinds = capacity_kinds;
        }
    }

    // Older Intel hybrid kernels may not expose cpu_core/cpu_atom or capacity.
    // SMT topology is a conservative fallback: P-cores have more sibling
    // threads than E-cores on the Intel generations this fallback targets.
    if !has_both_core_kinds(&core_kinds) && vendor == CpuVendor::Intel {
        if let Some(topology_kinds) = detect_smt_classes(logical_cpus, sys) {
            core_kinds = topology_kinds;
        }
    }

    if !has_both_core_kinds(&core_kinds) {
        core_kinds.fill(CpuCoreKind::Unknown);
    }

    let performance_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Performance);
    let efficiency_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Efficiency);
    let physical_core_groups = build_physical_core_groups(&core_groups, &core_kinds);

    CpuTopology {
        vendor,
        model,
        logical_cpus,
        physical_cores,
        performance_cores,
        efficiency_cores,
        core_kinds,
        physical_core_groups,
    }
}

fn parse_cpu_identity(cpuinfo: &str) -> (CpuVendor, String) {
    let mut vendor = CpuVendor::Unknown;
    let mut model = String::new();

    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if key == "vendor_id" && vendor == CpuVendor::Unknown {
            vendor = match value {
                "GenuineIntel" => CpuVendor::Intel,
                "AuthenticAMD" => CpuVendor::Amd,
                _ if !value.is_empty() => CpuVendor::Other,
                _ => CpuVendor::Unknown,
            };
        }

        if model.is_empty() && matches!(key, "model name" | "Processor" | "Hardware") {
            model = clean_model_name(value);
        }

        if vendor != CpuVendor::Unknown && !model.is_empty() {
            break;
        }
    }

    (vendor, model)
}

fn clean_model_name(value: &str) -> String {
    value
        .replace("(R)", "")
        .replace("(TM)", "")
        .replace(" CPU", "")
        .replace(" Processor", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn detect_kernel_core_groups<S: Sys>(logical_cpus: usize, sys: &S) -> Vec<CpuCoreKind> {
    let mut kinds = vec![CpuCoreKind::Unknown; logical_cpus];

    if let Some(cpus) = read_cpu_list_file(sys, "/sys/devices/cpu_core/cpus") {
        apply_kind(&mut kinds, &cpus, CpuCoreKind::Performance);
    }
    if let Some(cpus) = read_cpu_list_file(sys, "/sys/devices/cpu_atom/cpus") {
        apply_kind(&mut kinds, &cpus, CpuCoreKind::Efficiency);
    }
    if let Some(cpus) = read_cpu_list_file(sys, "/sys/devices/cpu_lowpower/cpus") {
        apply_kind(&mut kinds, &cpus, CpuCoreKind::Efficiency);
    }

    kinds
}

fn apply_kind(kinds: &mut [CpuCoreKind], cpus: &[usize], kind: CpuCoreKind) {
    for &cpu in cpus {
        if let Some(slot) = kinds.get_mut(cpu) {
            *slot = kind;
        }
    }
}

fn detect_capacity_classes<S: Sys>(logical_cpus: usize, sys: &S) -> Option<Vec<CpuCoreKind>> {
    let capacities = (0..logical_cpus)
        .map(|cpu| {
            read_u64(
                sys,
                &PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity")),
            )
        })
        .collect::<Vec<_>>();

    classify_capacity_values(&capacities)
}

fn classify_capacity_values(capacities: &[Option<u64>]) -> Option<Vec<CpuCoreKind>> {
    if capacities.is_empty() || capacities.iter().any(Option::is_none) {
        return None;
    }

    let values = capacities
        .iter()
        .map(|value| value.unwrap_or_default())
        .collect::<Vec<_>>();
    let min = *values.iter().min()?;
    let max = *values.iter().max()?;

    // Ignore tiny differences. Scheduler capacities are static capabilities,
    // but a 5% floor keeps this from turning minor calibration differences into
    // fake P/E classes.
    if max == 0 || min.saturating_mul(100) > max.saturating_mul(95) {
        return None;
    }

    let split = min.saturating_add(max).div_ceil(2);
    let kinds = values
        .into_iter()
        .map(|value| {
            if value >= split {
                CpuCoreKind::Performance
            } else {
                CpuCoreKind::Efficiency
            }
        })
        .collect::<Vec<_>>();

    has_both_core_kinds(&kinds).then_some(kinds)
}

fn detect_smt_classes<S: Sys>(logical_cpus: usize, sys: &S) -> Option<Vec<CpuCoreKind>> {
    let sibling_counts = (0..logical_cpus)
        .map(|cpu| {
            read_cpu_list_file(
                sys,
                format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"),
            )
            .map(|cpus| cpus.len())
        })
        .collect::<Vec<_>>();

    classify_sibling_counts(&sibling_counts)
}

fn classify_sibling_counts(counts: &[Option<usize>]) -> Option<Vec<CpuCoreKind>> {
    if counts.is_empty() || counts.iter().any(Option::is_none) {
        return None;
    }

    let values = counts
        .iter()
        .map(|value| value.unwrap_or_default())
        .collect::<Vec<_>>();
    let min = *values.iter().min()?;
    let max = *values.iter().max()?;
    if min == 0 || min == max {
        return None;
    }

    let kinds = values
        .into_iter()
        .map(|value| {
            if value == max {
                CpuCoreKind::Performance
            } else if value == min {
                CpuCoreKind::Efficiency
            } else {
                CpuCoreKind::Unknown
            }
        })
        .collect::<Vec<_>>();

    (!kinds
        .iter()
        .any(|kind| matches!(kind, CpuCoreKind::Unknown))
        && has_both_core_kinds(&kinds))
    .then_some(kinds)
}

fn read_core_groups<S: Sys>(logical_cpus: usize, sys: &S) -> Vec<Option<String>> {
    (0..logical_cpus)
        .map(|cpu| {
            let list_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list");
            if let Some(cpus) = read_cpu_list_file(sys, &list_path) {
                return Some(
                    cpus.into_iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }

            let package = sys.read_to_string(&PathBuf::from(format!(
                "/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id"
            )))?;
            let core = sys.read_to_string(&PathBuf::from(format!(
                "/sys/devices/system/cpu/cpu{cpu}/topology/core_id"
            )))?;
            Some(format!("{}:{}", package.trim(), core.trim()))
        })
        .collect()
}

fn build_physical_core_groups(
    groups: &[Option<String>],
    kinds: &[CpuCoreKind],
) -> Vec<CpuPhysicalCore> {
    if groups.len() != kinds.len() || groups.is_empty() || groups.iter().any(Option::is_none) {
        return Vec::new();
    }

    let mut keyed = Vec::<(String, CpuPhysicalCore)>::new();
    for (logical_cpu, (group, kind)) in groups.iter().zip(kinds).enumerate() {
        let Some(key) = group.as_ref() else {
            return Vec::new();
        };

        if let Some((_, core)) = keyed.iter_mut().find(|(existing, _)| existing == key) {
            if core.kind != *kind {
                core.kind = CpuCoreKind::Unknown;
            }
            core.logical_cpus.push(logical_cpu);
        } else {
            keyed.push((
                key.clone(),
                CpuPhysicalCore {
                    kind: *kind,
                    logical_cpus: vec![logical_cpu],
                },
            ));
        }
    }

    keyed
        .into_iter()
        .map(|(_, mut core)| {
            core.logical_cpus.sort_unstable();
            core
        })
        .collect()
}

fn count_unique_groups(groups: &[Option<String>]) -> Option<usize> {
    if groups.is_empty() || groups.iter().any(Option::is_none) {
        return None;
    }
    Some(
        groups
            .iter()
            .filter_map(Option::as_ref)
            .collect::<HashSet<_>>()
            .len(),
    )
}

fn count_kind_groups(
    groups: &[Option<String>],
    kinds: &[CpuCoreKind],
    target: CpuCoreKind,
) -> Option<usize> {
    if !has_both_core_kinds(kinds) || groups.len() != kinds.len() {
        return None;
    }

    let mut unique = HashSet::new();
    for (group, kind) in groups.iter().zip(kinds) {
        if *kind != target {
            continue;
        }
        unique.insert(group.as_ref()?);
    }
    Some(unique.len())
}

fn has_both_core_kinds(kinds: &[CpuCoreKind]) -> bool {
    kinds
        .iter()
        .any(|kind| matches!(kind, CpuCoreKind::Performance))
        && kinds
            .iter()
            .any(|kind| matches!(kind, CpuCoreKind::Efficiency))
}

fn read_cpu_list_file<S: Sys>(sys: &S, path: impl AsRef<std::path::Path>) -> Option<Vec<usize>> {
    let text = sys.read_to_string(path.as_ref())?;
    parse_cpu_list(&text)
}

fn read_u64<S: Sys>(sys: &S, path: &std::path::Path) -> Option<u64> {
    sys.read_to_string(path)?.trim().parse().ok()
}

fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            let start = start.trim().parse::<usize>().ok()?;
            let end = end.trim().parse::<usize>().ok()?;
            if end < start {
                return None;
            }
            cpus.extend(start..=end);
        } else {
            cpus.push(part.trim().parse::<usize>().ok()?);
        }
    }
    (!cpus.is_empty()).then_some(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11\n"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpu_list("7"), Some(vec![7]));
        assert_eq!(parse_cpu_list("3-1"), None);
    }

    #[test]
    fn parses_intel_identity_and_cleans_model() {
        let text = "vendor_id : GenuineIntel\nmodel name : 12th Gen Intel(R) Core(TM) i9-12900K\n";
        let (vendor, model) = parse_cpu_identity(text);
        assert_eq!(vendor, CpuVendor::Intel);
        assert_eq!(model, "12th Gen Intel Core i9-12900K");
    }

    #[test]
    fn parses_amd_identity() {
        let text = "vendor_id : AuthenticAMD\nmodel name : AMD Ryzen 9 9950X 16-Core Processor\n";
        let (vendor, model) = parse_cpu_identity(text);
        assert_eq!(vendor, CpuVendor::Amd);
        assert_eq!(model, "AMD Ryzen 9 9950X 16-Core");
    }

    #[test]
    fn groups_logical_threads_into_physical_cores() {
        let groups = vec![
            Some("0,4".to_string()),
            Some("1,5".to_string()),
            Some("2".to_string()),
            Some("3".to_string()),
            Some("0,4".to_string()),
            Some("1,5".to_string()),
        ];
        let kinds = vec![
            CpuCoreKind::Performance,
            CpuCoreKind::Performance,
            CpuCoreKind::Efficiency,
            CpuCoreKind::Efficiency,
            CpuCoreKind::Performance,
            CpuCoreKind::Performance,
        ];
        assert_eq!(
            build_physical_core_groups(&groups, &kinds),
            vec![
                CpuPhysicalCore {
                    kind: CpuCoreKind::Performance,
                    logical_cpus: vec![0, 4],
                },
                CpuPhysicalCore {
                    kind: CpuCoreKind::Performance,
                    logical_cpus: vec![1, 5],
                },
                CpuPhysicalCore {
                    kind: CpuCoreKind::Efficiency,
                    logical_cpus: vec![2],
                },
                CpuPhysicalCore {
                    kind: CpuCoreKind::Efficiency,
                    logical_cpus: vec![3],
                },
            ]
        );
    }

    #[test]
    fn capacity_classes_require_meaningful_difference() {
        assert_eq!(
            classify_capacity_values(&[Some(1024), Some(1024), Some(768), Some(768)]),
            Some(vec![
                CpuCoreKind::Performance,
                CpuCoreKind::Performance,
                CpuCoreKind::Efficiency,
                CpuCoreKind::Efficiency,
            ])
        );
        assert_eq!(
            classify_capacity_values(&[Some(1024), Some(1000), Some(1024), Some(1000)]),
            None
        );
    }

    #[test]
    fn smt_classes_detect_two_thread_and_single_thread_cores() {
        assert_eq!(
            classify_sibling_counts(&[Some(2), Some(2), Some(1), Some(1)]),
            Some(vec![
                CpuCoreKind::Performance,
                CpuCoreKind::Performance,
                CpuCoreKind::Efficiency,
                CpuCoreKind::Efficiency,
            ])
        );
        assert_eq!(classify_sibling_counts(&[Some(2), Some(2)]), None);
    }
}
#[cfg(test)]
mod fixture_tests {
    use super::*;
    use crate::system::FixtureSys;

    /// Intel hybrid layout: 4 P cores (2 threads each), 8 E cores (1 thread),
    /// with scheduler capacity exposed but no cpu_core/cpu_atom files.
    fn hybrid_sys(logical: usize) -> FixtureSys {
        let mut fixture = FixtureSys::default();
        fixture.file(
            "/proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Core(TM) Ultra 7\n",
        );
        for cpu in 0..logical {
            let is_p = cpu < 8;
            let list = if is_p {
                let pair = cpu / 2 * 2;
                format!("{pair},{}", pair + 1)
            } else {
                cpu.to_string()
            };
            fixture
                .file(
                    &format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"),
                    list,
                )
                .file(
                    &format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"),
                    if is_p { "1024" } else { "512" },
                );
        }
        fixture
    }

    #[test]
    fn fixture_detects_hybrid_topology_without_kernel_core_files() {
        let fixture = hybrid_sys(16);
        let topology = detect_topology(16, &fixture);

        assert_eq!(topology.vendor, CpuVendor::Intel);
        assert_eq!(topology.performance_threads(), 8);
        assert_eq!(topology.efficiency_threads(), 8);
        assert_eq!(topology.physical_cores, Some(12));
        assert!(topology.is_hybrid());

        // P core 0 groups threads 0 and 1.
        let core0 = &topology
            .physical_core_groups
            .iter()
            .find(|core| core.logical_cpus.contains(&0))
            .unwrap();
        assert_eq!(core0.kind, CpuCoreKind::Performance);
        assert_eq!(core0.logical_cpus, vec![0, 1]);
    }

    #[test]
    fn fixture_detects_homogeneous_topology_as_unknown_kinds() {
        let mut fixture = FixtureSys::default();
        fixture.file(
            "/proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 9 7950X\n",
        );
        for cpu in 0..32 {
            let pair = cpu % 16;
            let list = format!("{pair},{}", pair + 16);
            fixture.file(
                &format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"),
                list,
            );
        }

        let topology = detect_topology(32, &fixture);
        assert_eq!(topology.vendor, CpuVendor::Amd);
        assert!(!topology.is_hybrid());
        assert!(topology
            .core_kinds
            .iter()
            .all(|kind| { matches!(kind, CpuCoreKind::Unknown) }));
        assert_eq!(topology.performance_cores, None);
        assert_eq!(topology.physical_cores, Some(16));
    }
}
