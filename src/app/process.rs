//! Process metadata cache, CPU-time helpers and the per-refresh collector.
//!
//! This is the production collector used by the fast worker. It reads the
//! process table and DRM fdinfo but owns no sampling loop; cadence and
//! snapshot delivery stay in `app::spawn_fast_worker`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use sysinfo::System;

use crate::domain::{ProcessIdentity, ProcessStats};
use crate::drm::{sample_process_gpus, DrmSamplerState};
use crate::system::{RealSys, Sys};

/// Cached *static* process metadata, keyed by [`ProcessIdentity`].
///
/// The kernel recycles PIDs, so identity is `pid + start_time`. Once a
/// process has been seen, its static metadata (program name, executable
/// path) is cached and reused; only the dynamic counters (CPU, memory,
/// thread count) are resampled on each process refresh.
///
/// The thread count is deliberately **not** cached here: a process can create
/// and destroy threads at any time, so a cached value goes stale immediately,
/// and a one-off read failure must not pin a fabricated count forever. It is
/// re-read from `/proc/<pid>/status` on every process refresh (not per UI
/// frame) and reported as `Option`: an unreadable process is `None` and the
/// next refresh retries.
#[derive(Default)]
pub(crate) struct ProcessCache {
    pub(crate) entries: HashMap<ProcessIdentity, CachedProcess>,
}

pub(crate) struct CachedProcess {
    program: String,
    command: String,
}

impl ProcessCache {
    /// Return the cached static metadata for `identity`, computing and storing
    /// it on first sight. Only identity-bound, stable fields are cached.
    pub(crate) fn stable(
        &mut self,
        identity: ProcessIdentity,
        program: &str,
        command: &str,
    ) -> (String, String) {
        let entry = self
            .entries
            .entry(identity)
            .or_insert_with(|| CachedProcess {
                program: program.to_string(),
                command: command.to_string(),
            });
        // `execve` keeps the PID and start time, so the same identity can name a
        // new image. Refresh the cached names when the freshly read ones change
        // (an empty read is transient and keeps the last good value).
        if !program.is_empty() && entry.program != program {
            entry.program = program.to_string();
        }
        if !command.is_empty() && entry.command != command {
            entry.command = command.to_string();
        }
        (entry.program.clone(), entry.command.clone())
    }

    /// Drop entries for identities that no longer exist (exited processes).
    pub(crate) fn retain(&mut self, live: impl Iterator<Item = ProcessIdentity>) {
        let live: HashSet<_> = live.collect();
        self.entries.retain(|identity, _| live.contains(identity));
    }
}

/// Per-CPU usage indexed by kernel CPU id (matching the id-indexed topology
/// vectors), so a non-prefix online set does not misalign the heatmap. sysinfo
/// only reports the online CPUs; offline holes stay `0.0`. When the ids are
/// not recoverable from the names the compact order is kept.
pub(crate) fn per_cpu_usage_by_id(cpus: &[(&str, f32)]) -> Vec<f64> {
    let ids: Option<Vec<usize>> = cpus
        .iter()
        .map(|(name, _)| {
            name.strip_prefix("cpu")
                .and_then(|id| id.parse::<usize>().ok())
        })
        .collect();
    match ids {
        Some(ids) => {
            let span = ids.iter().copied().max().map_or(0, |max| max + 1);
            let mut usage = vec![0.0; span];
            for ((_, value), id) in cpus.iter().zip(ids) {
                if let Some(slot) = usage.get_mut(id) {
                    *slot = *value as f64;
                }
            }
            usage
        }
        None => cpus.iter().map(|(_, value)| *value as f64).collect(),
    }
}

/// Static metadata from the cache plus a freshly read dynamic thread count.
///
/// `read_threads` is injected so the refresh behavior is testable without a
/// live `/proc`: the count is read on every call (never taken from the
/// cache), and a failed read yields `None` rather than a substitute value.
pub(crate) fn refresh_process_metadata<F>(
    cache: &mut ProcessCache,
    identity: ProcessIdentity,
    program: &str,
    command: &str,
    read_threads: F,
) -> (String, String, Option<usize>)
where
    F: FnOnce(u32) -> Option<usize>,
{
    let (program, command) = cache.stable(identity, program, command);
    (program, command, read_threads(identity.pid))
}

pub(crate) fn collect_process_stats(
    system: &System,
    cache: &mut ProcessCache,
    drm: &mut DrmSamplerState,
) -> Vec<ProcessStats> {
    let now = Instant::now();
    let mut stats = Vec::with_capacity(system.processes().len());
    let mut live = Vec::new();
    for (pid, process) in system.processes().iter() {
        let identity = ProcessIdentity::new(pid.as_u32(), process.start_time());
        live.push(identity);

        let program = process.name().to_string_lossy().into_owned();
        let command = process
            .exe()
            .map(|path| path.display().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| program.clone());

        let (program, command, threads) = refresh_process_metadata(
            cache,
            identity,
            &program,
            &command,
            read_process_thread_count,
        );

        // S8: per-device GPU usage from the process's DRM fdinfo entries.
        let gpu = sample_process_gpus(&RealSys, now, identity, drm);

        stats.push(ProcessStats {
            pid: identity.pid,
            program,
            command,
            cpu_pct: process.cpu_usage() as f64,
            memory_bytes: process.memory(),
            threads,
            start_time: identity.start_time,
            gpu,
        });
    }
    cache.retain(live.iter().copied());
    drm.retain(live.into_iter());
    stats
}

pub(crate) fn read_process_thread_count(pid: u32) -> Option<usize> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| {
        let value = line.strip_prefix("Threads:")?;
        value.trim().parse().ok()
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuTimes {
    pub(crate) total: u64,
    pub(crate) io_wait: u64,
}

pub(crate) fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_times(&text)
}

pub(crate) fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 5 {
        return None;
    }

    Some(CpuTimes {
        total: values.iter().copied().sum(),
        io_wait: values[4],
    })
}

pub(crate) fn io_wait_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total_delta = current.total.saturating_sub(previous.total);
    if total_delta == 0 {
        return None;
    }
    let io_wait_delta = current.io_wait.saturating_sub(previous.io_wait);
    Some(io_wait_delta as f64 / total_delta as f64 * 100.0)
}

pub(crate) fn read_load_average() -> Option<(f64, f64, f64)> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    Some((
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ))
}

pub(crate) fn read_cpu_frequency_mhz(sys: &dyn Sys) -> Option<f64> {
    let text = sys.read_to_string(&PathBuf::from("/proc/cpuinfo"))?;
    let mut total = 0.0;
    let mut count = 0u64;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "cpu MHz" {
            continue;
        }
        let Ok(mhz) = value.trim().parse::<f64>() else {
            continue;
        };
        if mhz.is_finite() && mhz > 0.0 {
            total += mhz;
            count += 1;
        }
    }

    (count > 0).then_some(total / count as f64)
}
