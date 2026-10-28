//! Background workers: the fast system/GPU sampling loop and the LLM poll loop.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::cpu::{detect_cpu_topology, CpuTopology};
use crate::cpu_sensors::CpuSensors;
use crate::discovery::discover_gpus;
use crate::domain::LocalServerIdentity;
use crate::domain::{
    FastSnapshot, GpuMapping, GpuSelector, ProcessStats, SystemStats, MAX_REFRESH_MS,
    MIN_LLM_POLL_MS, MIN_REFRESH_MS,
};
use crate::drm::DrmSamplerState;
use crate::gpu::new_gpu_provider;
use crate::gpu_map::{nvml_compute_gpus, process_render_gpus};
use crate::llama::LlmStats;
use crate::providers::nvidia::discover_mig_children;
use crate::system::RealSys;
use nvml_wrapper::Nvml;

use super::*;

use super::try_send_snapshot;

/// Clears a server's in-flight flag even if the poll thread unwinds.
struct PollFlag(Arc<AtomicBool>);

impl Drop for PollFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn spawn_fast_worker(
    gpu_selector: Arc<RwLock<GpuSelector>>,
    process_refresh_ms: Arc<AtomicU64>,
    refresh_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<FastSnapshot>,
    drops: Arc<AtomicU64>,
    server_identity_rx: Receiver<(u64, Option<LocalServerIdentity>)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut current_selector = gpu_selector
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let static_gpus = discover_gpus(&RealSys);
        let mut gpu = new_gpu_provider(current_selector.clone(), &static_gpus);
        let mut system = System::new();
        let mut system_stats = SystemStats::default();
        let mut last_system_refresh: Option<Instant> = None;
        let mut last_process_refresh: Option<Instant> = None;
        let mut process_cache = ProcessCache::default();
        // S8: baseline for DRM fdinfo engine-utilization deltas, keyed by
        // (process identity, BDF, engine). Pruned on the process cadence.
        let mut drm_state = DrmSamplerState::default();
        let mut process_stats = Vec::<ProcessStats>::new();
        let mut previous_cpu_times = read_cpu_times();
        let mut cpu_topology: Option<CpuTopology> = None;
        // S12: slow-changing CPU sensors (temperature, power) sampled on their
        // own cadence; cached values are reused on the fast system cycles.
        let mut last_sensor_refresh: Option<Instant> = None;
        let mut cpu_temperature_c: Option<f64> = None;
        let mut cpu_power_w: Option<f64> = None;
        // S11: one-shot sensor discovery (cpufreq policies, CPU hwmon, RAPL
        // package zone); samples only re-read the dynamic counters.
        let mut cpu_sensors = CpuSensors::default();
        cpu_sensors.discover(&RealSys);

        // S16: server→GPU mapping state. Static topology and NVML are cached
        // once; the mapping itself is recomputed on the process-refresh cadence.
        let mut server_identity: Option<LocalServerIdentity> = None;
        // Mapping generation the current `server_identity` was published with;
        // stamped into every snapshot so the app can drop stale mappings.
        let mut server_generation: u64 = 0;
        let nvml_handle = Nvml::init().ok();
        // MIG topology is static: captured once at startup and never re-probed
        // on the process-refresh cadence (no static NVML polling in the loop).
        let mig_children = nvml_handle
            .as_ref()
            .map(discover_mig_children)
            .unwrap_or_default();
        let mut gpu_map: GpuMapping = GpuMapping::None;
        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            let requested_selector = gpu_selector
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if requested_selector != current_selector {
                current_selector = requested_selector;
                gpu = new_gpu_provider(current_selector.clone(), &static_gpus);
            }
            // Drain any server-identity updates (initial + resync/settings).
            while let Ok((generation, identity)) = server_identity_rx.try_recv() {
                server_generation = generation;
                server_identity = identity;
                gpu_map = GpuMapping::None;
            }
            let process_interval =
                Duration::from_millis(process_refresh_ms.load(Ordering::Relaxed).clamp(
                    config::MIN_PROCESS_REFRESH_MS,
                    config::MAX_PROCESS_REFRESH_MS,
                ));

            if last_process_refresh
                .map(|at| at.elapsed() >= process_interval)
                .unwrap_or(true)
            {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::All,
                    true,
                    ProcessRefreshKind::nothing()
                        .with_memory()
                        .with_cpu()
                        // Refresh on every cycle: `execve` keeps the PID and
                        // start time, so `OnlyIfNotSet` would pin the pre-exec
                        // image forever.
                        .with_exe(UpdateKind::Always),
                );
                process_stats = collect_process_stats(&system, &mut process_cache, &mut drm_state);
                last_process_refresh = Some(Instant::now());

                // S16: recompute the server→GPU mapping on the process cadence.
                // The mapping is only trusted while the PID still carries the
                // start time read at discovery, so a reused PID is invalidated
                // immediately — before the next server resync.
                if let Some(expected) = server_identity {
                    let current_start =
                        crate::discovery_llm::read_process_start_time(&RealSys, expected.pid);
                    let render = process_render_gpus(&RealSys, expected.pid, &static_gpus);
                    let nvml = nvml_compute_gpus(nvml_handle.as_ref(), expected.pid, &mig_children);
                    gpu_map =
                        map_server_identity(expected, current_start, render, nvml, &static_gpus);
                } else {
                    // No local server process (configured/remote endpoint) →
                    // there is no process to attribute; keep the mapping empty.
                    gpu_map = GpuMapping::None;
                }
            }

            // S12: slow-changing CPU thermal/power on their own cadence. The
            // RAPL power sample blocks ~50 ms for its two-read window, so it
            // must not run on the 250 ms system cadence.
            if last_sensor_refresh
                .map(|at| at.elapsed() >= SLOW_SENSOR_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                cpu_temperature_c = cpu_sensors.sample_temperature_c(&RealSys);
                cpu_power_w = cpu_sensors.sample_power_w(&RealSys);
                last_sensor_refresh = Some(Instant::now());
            }

            if last_system_refresh
                .map(|at| at.elapsed() >= SYSTEM_REFRESH_INTERVAL)
                .unwrap_or(true)
            {
                system.refresh_cpu_usage();
                system.refresh_memory();

                let current_cpu_times = read_cpu_times();
                let io_wait_pct = previous_cpu_times
                    .zip(current_cpu_times)
                    .and_then(|(previous, current)| io_wait_percent(previous, current));
                if current_cpu_times.is_some() {
                    previous_cpu_times = current_cpu_times;
                }

                // Unknown when /proc/loadavg is unreadable: kept as `None`
                // and rendered `—`, never silently turned into a fake zero.
                let load_average = read_load_average();
                let per_cpu_pairs = system
                    .cpus()
                    .iter()
                    .map(|cpu| (cpu.name(), cpu.cpu_usage()))
                    .collect::<Vec<_>>();
                let per_cpu_usage = per_cpu_usage_by_id(&per_cpu_pairs);
                let topology = cpu_topology
                    .get_or_insert_with(|| detect_cpu_topology(per_cpu_usage.len()))
                    .clone();
                system_stats = SystemStats {
                    cpu_usage: system.global_cpu_usage() as f64,
                    per_cpu_usage,
                    cpu_topology: topology,
                    // S11: cpufreq first, /proc/cpuinfo as fallback (cheap,
                    // so it stays on the fast system cadence).
                    cpu_frequency_mhz: cpu_sensors
                        .sample_frequency_mhz(&RealSys)
                        .or_else(|| read_cpu_frequency_mhz(&RealSys)),
                    // S12: reused from the slower sensor cadence.
                    cpu_temperature_c,
                    cpu_power_w,
                    io_wait_pct,
                    load_one: load_average.map(|(one, _, _)| one),
                    load_five: load_average.map(|(_, five, _)| five),
                    load_fifteen: load_average.map(|(_, _, fifteen)| fifteen),
                    memory_used_bytes: system.used_memory(),
                    memory_total_bytes: system.total_memory(),
                    swap_used_bytes: system.used_swap(),
                    swap_total_bytes: system.total_swap(),
                    processes: process_stats.clone(),
                };
                last_system_refresh = Some(Instant::now());
            }

            let snapshot = FastSnapshot {
                gpu: gpu.sample(),
                system: system_stats.clone(),
                gpu_map: gpu_map.clone(),
                mapping_generation: server_generation,
            };

            if !try_send_snapshot(&tx, snapshot, &drops) {
                break;
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_REFRESH_MS, &stop);
        }
    })
}

pub(super) fn spawn_llm_worker(
    initial_servers: Vec<ServerSpec>,
    refresh_ms: Arc<AtomicU64>,
    offline_grace_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<LlmSample>,
    drops: Arc<AtomicU64>,
    server_rx: Receiver<LlmCommand>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Session generation of the current server set; every published sample
        // is tagged with it so a result from a set the app already replaced can
        // be rejected. Each server additionally carries its own key, so samples
        // never cross server boundaries.
        let mut generation: u64 = 0;
        let mut servers: Vec<LlmPollSlot> =
            initial_servers.into_iter().map(LlmPollSlot::new).collect();
        let (sample_tx, sample_rx) = mpsc::channel::<LlmSample>();

        while !stop.load(Ordering::Relaxed) {
            let cycle_started = Instant::now();
            while let Ok(command) = server_rx.try_recv() {
                match command {
                    LlmCommand::SetServers {
                        generation: next_generation,
                        servers: list,
                    } => {
                        generation = next_generation;
                        servers = rebuild_poll_slots(servers, list);
                    }
                }
            }

            let offline_grace = Duration::from_millis(
                offline_grace_ms
                    .load(Ordering::Relaxed)
                    .min(config::MAX_OFFLINE_GRACE_MS),
            );
            // One poll per server at a time. A slow or hanging server keeps
            // its own in-flight slot and is simply skipped next cycle; other
            // servers continue on their own cadence.
            for slot in &servers {
                if slot
                    .in_flight
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                let monitor = Arc::clone(&slot.monitor);
                let in_flight = Arc::clone(&slot.in_flight);
                let sample_tx = sample_tx.clone();
                let generation = generation;
                thread::spawn(move || {
                    let _flag = PollFlag(in_flight);
                    let mut server = monitor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let raw_stats = match server.monitor.as_mut() {
                        Some(monitor) => monitor.sample(),
                        None => LlmStats {
                            error: server.init_error.clone(),
                            ..Default::default()
                        },
                    };
                    let stats = stabilize_llm_sample(
                        raw_stats,
                        &mut server.last_good,
                        Instant::now(),
                        offline_grace,
                    );
                    let _ = sample_tx.send(LlmSample {
                        server: server.key.clone(),
                        generation,
                        stats,
                    });
                });
            }

            let mut disconnected = false;
            while let Ok(sample) = sample_rx.try_recv() {
                if !try_send_snapshot(&tx, sample, &drops) {
                    disconnected = true;
                    break;
                }
            }
            if disconnected {
                return;
            }

            sleep_until_next_cycle(cycle_started, &refresh_ms, MIN_LLM_POLL_MS, &stop);
        }

        // Do not let detached poll threads outlive the worker: bounded by the
        // request timeout plus margin. A truly wedged request is left for
        // process exit, as before.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && servers
                .iter()
                .any(|slot| slot.in_flight.load(Ordering::Acquire))
        {
            thread::sleep(Duration::from_millis(25));
        }
    })
}

/// Sleeps until the next cycle boundary, clamping the live refresh value to
/// `[min_ms, MAX_REFRESH_MS]`. Callers pass their own floor: `MIN_REFRESH_MS`
/// for the fast worker and `MIN_LLM_POLL_MS` for the LLM worker, so a fast UI
/// refresh cannot drive the /metrics HTTP poll faster than 250 ms.
pub(super) fn next_cycle_target(refresh_ms: &AtomicU64, min_ms: u64) -> Duration {
    Duration::from_millis(
        refresh_ms
            .load(Ordering::Relaxed)
            .clamp(min_ms, MAX_REFRESH_MS),
    )
}

fn sleep_until_next_cycle(
    started: Instant,
    refresh_ms: &AtomicU64,
    min_ms: u64,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let target = next_cycle_target(refresh_ms, min_ms);
        let elapsed = started.elapsed();
        if elapsed >= target {
            break;
        }

        thread::sleep((target - elapsed).min(Duration::from_millis(25)));
    }
}
