//! Worker, channel, redraw-scheduler and benchmark tests.
use super::{
    accepts_llm_sample, bench_emit, build_server_summaries, change_refresh, collect_process_stats,
    drain_try_recv, join_bounded, mock_body, next_cycle_target, read_process_thread_count,
    read_request_path, self_status_field, spawn_gated_server, spawn_llm_worker, spawn_plain_server,
    test_spec, try_send_snapshot, ui, write_http, DashboardSnapshot, DrmSamplerState, LlmCommand,
    LlmSample, LlmStats, ProcessCache, ProcessStats, RealSys, RedrawScheduler, SystemStats,
    UiState, IDLE_REDRAW_INTERVAL, MAX_POLL_INTERVAL, MAX_REFRESH_MS, MIN_LLM_POLL_MS,
    MIN_REFRESH_MS,
};

use crate::domain::{ProcessIdentity, ServerBackend};
use crate::drm::sample_process_gpus;
use crate::llama::LlamaMonitor;
use ratatui::Terminal;
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

// ------------------------------------------------------------------
// CFA-35 — reproducible runtime performance baseline
// ------------------------------------------------------------------
//
// Two benchmarks drive the *real* runtime paths (the synchronous
// `LlamaMonitor` HTTP poll, the ratatui render closure over a full
// 16-server dashboard, the production `RedrawScheduler` model and the
// `/proc` process collector) and emit machine-readable
// `ORSKTOP_BENCH|<key>=<value>` lines on the raw process stdout: libtest
// captures `println!`, but a direct `stdout().write_all` still reaches the
// `cargo test` log.
//
//  * `perf_baseline_benchmark_for_dirty_only_redraw_and_polling` — the cheap
//    in-gate part. It exercises the poll path against blocking-accept mock
//    servers, the render closure and TUI input latency, reports thread-local
//    CPU, and reads *no* `/proc` process tables. It is small enough (~0.5 s)
//    to run in every sanctioned gate, which is exactly what lets an
//    independent `pc-project cargo test --all-targets --locked` run
//    reproduce its numbers.
//
//  * `perf_baseline_soak_and_process_collection_benchmark` — `#[ignore]`d. It
//    holds the expensive parts (the full host `/proc`+DRM process-table
//    collection and a longer-operation window) that must not run in every
//    gate. It needs a targeted, single-threaded invocation:
//      cargo test --all-targets --locked -- --ignored --test-threads=1 \
//        perf_baseline_soak_and_process_collection_benchmark
//    The sanctioned `pc-project cargo test` gate only accepts the fixed
//    argument list and cannot forward those flags, so running it needs an
//    operator-approved benchmark runner; the limitation is recorded in
//    `docs/PERFORMANCE_BASELINE.md`.
//
// Every timing is *reported*, never asserted: the tests assert only
// structural invariants, so they cannot flake and they do not turn the
// shared suite into a brittle wall-clock gate. The wall-clock figures are a
// per-machine baseline (see `docs/PERFORMANCE_BASELINE.md`), not a CI
// threshold.

/// Assumed Linux `USER_HZ` for converting `/proc/.../stat` jiffies to
/// microseconds. 100 Hz is the near-universal default; it is documented
/// rather than probed so the benchmark has no extra dependency.
const BENCH_CLK_TCK: f64 = 100.0;

#[test]
fn refresh_change_is_clamped() {
    let shared = AtomicU64::new(MIN_REFRESH_MS);
    let mut value = MIN_REFRESH_MS;
    change_refresh(&mut value, false, &shared);
    assert_eq!(value, MIN_REFRESH_MS);

    value = MAX_REFRESH_MS;
    change_refresh(&mut value, true, &shared);
    assert_eq!(value, MAX_REFRESH_MS);
}

#[test]
fn refresh_change_updates_shared_value() {
    let shared = AtomicU64::new(1000);
    let mut value = 1000;
    change_refresh(&mut value, false, &shared);
    assert_eq!(value, 900);
    assert_eq!(shared.load(Ordering::Relaxed), 900);
}

#[test]
fn cycle_target_clamps_to_worker_floor() {
    let fast = AtomicU64::new(100);
    assert_eq!(
        next_cycle_target(&fast, MIN_LLM_POLL_MS),
        Duration::from_millis(250)
    );
    assert_eq!(
        next_cycle_target(&fast, MIN_REFRESH_MS),
        Duration::from_millis(100)
    );

    let slow = AtomicU64::new(5000);
    assert_eq!(
        next_cycle_target(&slow, MIN_LLM_POLL_MS),
        Duration::from_millis(5000)
    );
    assert_eq!(
        next_cycle_target(&slow, MIN_REFRESH_MS),
        Duration::from_millis(5000)
    );

    let unbounded = AtomicU64::new(60_000);
    assert_eq!(
        next_cycle_target(&unbounded, MIN_LLM_POLL_MS),
        Duration::from_millis(MAX_REFRESH_MS)
    );
}

#[test]
fn real_worker_tags_samples_per_server_across_a_set_change() {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (a_addr, a_stop, a_handle) = spawn_gated_server(Arc::clone(&release), started_tx);
    let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 10_000, 20_000);

    let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
    let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(2_500));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(8);
    let (cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();

    let worker = spawn_llm_worker(
        vec![test_spec(&key_a, &format!("http://{a_addr}"))],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    // The real worker has an A request in flight, held by the mock.
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker must contact server A");

    // Replace the monitored set with B under a new generation.
    cmd_tx
        .send(LlmCommand::SetServers {
            generation: 1,
            servers: vec![test_spec(&key_b, &format!("http://{b_addr}"))],
        })
        .unwrap();

    // Release A's delayed response.
    {
        let (lock, condvar) = &*release;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }

    // Consume published samples through the production acceptance path.
    let mut saw_delayed_a = false;
    let mut accepted_b = false;
    let mut saw_old_generation = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !(saw_delayed_a && accepted_b) {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            assert!(
                sample.server == key_a || sample.server == key_b,
                "every sample must carry a known server key"
            );
            if sample.server == key_a {
                saw_delayed_a = true;
                assert!(
                    !accepts_llm_sample(1, &sample),
                    "a delayed A result must be rejected under the new generation"
                );
            }
            if accepts_llm_sample(1, &sample) && sample.server == key_b {
                accepted_b = true;
            }
            if sample.generation != 1 {
                saw_old_generation = true;
            }
        }
    }
    assert!(saw_delayed_a, "the delayed A result must be published");
    assert!(accepted_b, "B must be accepted under the new generation");
    assert!(
        saw_old_generation,
        "a pre-switch-generation sample must be observable"
    );

    // A buffered result from the previous generation stays rejected, and a
    // sample for a server no longer in the set is dropped by the membership
    // check (covered here for the generation half).
    let buffered_stale = LlmSample {
        server: key_a.clone(),
        generation: 0,
        stats: LlmStats {
            model: "model-A".to_string(),
            ..Default::default()
        },
    };
    assert!(!accepts_llm_sample(1, &buffered_stale));

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    a_stop.store(true, Ordering::Relaxed);
    b_stop.store(true, Ordering::Relaxed);
    let _ = a_handle.join();
    let _ = b_handle.join();
}

#[test]
fn real_worker_polls_multiple_servers_independently() {
    let (a_addr, a_stop, a_handle) = spawn_plain_server("model-A", 4096, 1000, 500);
    let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 2000, 600);
    let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
    let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(2_500));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        vec![
            test_spec(&key_a, &format!("http://{a_addr}")),
            test_spec(&key_b, &format!("http://{b_addr}")),
        ],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    let mut a_model = None;
    let mut b_model = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && (a_model.is_none() || b_model.is_none()) {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            if sample.server == key_a {
                a_model = Some(sample.stats.model.clone());
            }
            if sample.server == key_b {
                b_model = Some(sample.stats.model.clone());
            }
        }
    }
    assert_eq!(a_model.as_deref(), Some("model-A"));
    assert_eq!(b_model.as_deref(), Some("model-B"));

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    a_stop.store(true, Ordering::Relaxed);
    b_stop.store(true, Ordering::Relaxed);
    let _ = a_handle.join();
    let _ = b_handle.join();
}

#[test]
fn an_unreachable_server_does_not_block_a_healthy_one() {
    let (a_addr, a_stop, a_handle) = spawn_plain_server("model-A", 4096, 1000, 500);
    // Bind then drop a port so connections are refused immediately.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
    let key_b = crate::domain::server_key(&format!("http://{dead_addr}"));

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(2_500));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        vec![
            test_spec(&key_b, &format!("http://{dead_addr}")),
            test_spec(&key_a, &format!("http://{a_addr}")),
        ],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    // Server A (listed after the dead one) must still report its model.
    let mut a_model = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && a_model.is_none() {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            if sample.server == key_a && sample.stats.connected {
                a_model = Some(sample.stats.model.clone());
            }
        }
    }
    assert_eq!(a_model.as_deref(), Some("model-A"));

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    a_stop.store(true, Ordering::Relaxed);
    let _ = a_handle.join();
}

#[test]
fn a_slow_server_does_not_delay_a_healthy_servers_delivery() {
    // A is gated (holds its response); B answers immediately. B's sample
    // must reach the app while A is still hanging.
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, _started_rx) = mpsc::channel::<()>();
    let (a_addr, a_stop, a_handle) = spawn_gated_server(Arc::clone(&release), started_tx);
    let (b_addr, b_stop, b_handle) = spawn_plain_server("model-B", 8192, 10_000, 20_000);
    let key_a = crate::domain::server_key(&format!("http://{a_addr}"));
    let key_b = crate::domain::server_key(&format!("http://{b_addr}"));

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(2_500));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(16);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        vec![
            test_spec(&key_a, &format!("http://{a_addr}")),
            test_spec(&key_b, &format!("http://{b_addr}")),
        ],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    let mut b_model = None;
    let mut b_samples = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && b_samples < 2 {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            if sample.server == key_b && sample.stats.connected {
                b_model = Some(sample.stats.model.clone());
                b_samples += 1;
            }
        }
    }
    assert_eq!(
        b_model.as_deref(),
        Some("model-B"),
        "healthy B must be delivered while A hangs"
    );
    assert!(
        b_samples >= 2,
        "healthy B must keep polling while A hangs, got {b_samples} samples"
    );

    // Release A so the worker's scope can finish and shut down.
    {
        let (lock, condvar) = &*release;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    a_stop.store(true, Ordering::Relaxed);
    b_stop.store(true, Ordering::Relaxed);
    let _ = a_handle.join();
    let _ = b_handle.join();
}

#[test]
fn idle_repaint_is_skipped_until_the_tick() {
    let start = Instant::now();
    let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);

    // The first frame always paints; with no state change the view is then
    // left untouched until the idle tick elapses.
    assert!(scheduler.should_paint(start));
    assert!(!scheduler.should_paint(start + Duration::from_millis(50)));
    assert!(!scheduler.should_paint(start + Duration::from_millis(249)));

    // A real change paints immediately instead of waiting for the tick.
    scheduler.mark_dirty();
    assert!(scheduler.should_paint(start + Duration::from_millis(100)));
    assert!(!scheduler.should_paint(start + Duration::from_millis(101)));
}

#[test]
fn poll_timeout_never_delays_a_repaint_and_bounds_the_drain_latency() {
    let start = Instant::now();
    let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);

    // Dirty view: `event::poll` must not block, so the frame is drawn now.
    assert_eq!(scheduler.poll_timeout(start), Duration::ZERO);
    assert!(scheduler.should_paint(start));
    // Clean view: the poll is capped, so newly arrived samples are drained
    // promptly even when the next idle tick is still far away.
    assert_eq!(scheduler.poll_timeout(start), MAX_POLL_INTERVAL);
    assert_eq!(
        scheduler.poll_timeout(start + Duration::from_millis(240)),
        Duration::from_millis(10)
    );
}

/// Simulates the production loop for one second and returns how many times
/// it painted. The loop advances by `poll_timeout`, exactly as the real
/// `event::poll` bounds an iteration; `snapshot_interval` models the fast
/// worker pushing a (repaint-forcing) snapshot on that cadence, and `None`
/// models a loop that receives no new data at all.
fn simulated_idle_redraws(snapshot_interval: Option<Duration>) -> usize {
    let one_second = Duration::from_secs(1);
    let start = Instant::now();
    let mut scheduler = RedrawScheduler::new(start, IDLE_REDRAW_INTERVAL);
    let mut now = Duration::ZERO;
    let mut next_snapshot = snapshot_interval;
    let mut redraws = 0usize;
    while now < one_second {
        if let (Some(interval), Some(at)) = (snapshot_interval, next_snapshot) {
            if now >= at {
                scheduler.mark_dirty();
                next_snapshot = Some(at + interval);
            }
        }
        if scheduler.should_paint(start + now) {
            redraws += 1;
        }
        now += scheduler.poll_timeout(start + now);
    }
    redraws
}

#[test]
fn idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick() {
    // Before: an idle iteration was bounded by `event::poll(50 ms)` and the
    // loop painted every iteration => 20 full redraws per second.
    let before =
        Duration::from_secs(1).as_millis() as usize / MAX_POLL_INTERVAL.as_millis() as usize;
    assert_eq!(before, 20, "documented pre-change idle redraw rate");

    // After: with no state change and no input, only the idle tick paints.
    let idle = simulated_idle_redraws(None);
    assert!(idle < before, "idle redraws {idle} not below {before}");
    assert!(idle <= 4, "idle redraws {idle}/s exceed the ~4/s budget");

    // A fast snapshot on the default 1000 ms cadence lands on the idle tick,
    // so it adds nothing; a 100 ms cadence roughly doubles the rate. These
    // bound the real idle rate (not just the scheduler floor).
    assert_eq!(
        simulated_idle_redraws(Some(Duration::from_millis(1_000))),
        idle
    );
    let fast = simulated_idle_redraws(Some(Duration::from_millis(100)));
    assert_eq!(fast, 10, "100 ms snapshots should yield ~10 redraws/s");
    assert!(fast < before);
}

#[test]
fn drain_try_recv_distinguishes_empty_from_disconnected() {
    let (tx, rx) = mpsc::channel::<u8>();
    let mut seen = Vec::new();

    assert!(
        drain_try_recv(&rx, |value| seen.push(value)).is_ok(),
        "an empty but connected queue is healthy"
    );

    tx.send(1).unwrap();
    tx.send(2).unwrap();
    drop(tx);

    assert!(
        drain_try_recv(&rx, |value| seen.push(value)).is_err(),
        "a disconnected worker must be surfaced"
    );
    assert_eq!(seen, [1, 2], "queued values are applied before the error");
}

#[test]
fn try_send_snapshot_counts_full_channel_and_stops_on_disconnect() {
    let (tx, rx) = mpsc::sync_channel::<u8>(1);
    let drops = AtomicU64::new(0);

    assert!(try_send_snapshot(&tx, 1, &drops));
    assert!(try_send_snapshot(&tx, 2, &drops));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(rx.try_recv().unwrap(), 1, "the pending snapshot is kept");

    drop(rx);
    assert!(!try_send_snapshot(&tx, 3, &drops));
    assert_eq!(
        drops.load(Ordering::Relaxed),
        1,
        "a disconnect must not be counted as a dropped snapshot"
    );
}

#[test]
fn join_bounded_reports_clean_exit() {
    let worker = thread::spawn(|| {});
    assert_eq!(join_bounded(worker, "test", Duration::from_secs(2)), Ok(()));
}

#[test]
fn join_bounded_reports_panicked_worker() {
    let worker = thread::spawn(|| panic!("simulated worker panic"));
    let error = join_bounded(worker, "test", Duration::from_secs(2)).unwrap_err();
    assert!(error.contains("panicked"), "{error}");
}

#[test]
fn join_bounded_reports_timeout() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        let _ = release_rx.recv();
    });
    let error = join_bounded(worker, "test", Duration::from_millis(50)).unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    release_tx.send(()).unwrap();
}

/// `utime + stime` of the *calling thread* in clock ticks
/// (`/proc/thread-self/stat`, fields 14+15). Reading `/proc/self/stat`
/// instead would sum every thread of the parallel libtest process and report
/// CPU this test did not spend; thread-self isolates it. Parsing resumes
/// after the final `)` so a `comm` with spaces or parentheses cannot shift
/// the fields.
fn thread_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

fn bench_ticks_to_us(ticks: u64) -> u128 {
    (u128::from(ticks) * 1_000_000) / BENCH_CLK_TCK as u128
}

/// Nearest-rank percentile of an ascending-sorted slice.
fn bench_percentile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (((sorted.len() - 1) as f64) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn bench_mean(durations: &[Duration]) -> Duration {
    if durations.is_empty() {
        return Duration::ZERO;
    }
    let total: Duration = durations.iter().sum();
    total / durations.len() as u32
}

/// A mock server with a **blocking** `accept`. Unlike `spawn_plain_server`
/// (non-blocking listener + 2 ms `WouldBlock` sleep), it answers as soon as a
/// request arrives, so a measured poll cycle reflects OrsikTop's HTTP client
/// work instead of the mock's sleep quantum. Stop it with
/// `stop_blocking_mock`, which wakes the blocked `accept` with one
/// connection.
fn spawn_blocking_mock(
    model: &'static str,
    n_ctx: u64,
    prompt_tokens: u64,
    predicted: u64,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // The stop wake-up connects without sending a request;
                    // break before the 2 s read timeout would stall the join.
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(path) = read_request_path(&mut stream) {
                        let (body, content_type) =
                            mock_body(&path, model, n_ctx, prompt_tokens, predicted);
                        write_http(&mut stream, &body, content_type);
                    }
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

fn stop_blocking_mock(stop: &Arc<AtomicBool>, addr: SocketAddr, handle: thread::JoinHandle<()>) {
    stop.store(true, Ordering::Relaxed);
    // Wake the blocked `accept`; the loop then observes the stop flag.
    let _ = TcpStream::connect(addr);
    let _ = handle.join();
}

/// A representative full dashboard: `server_count` servers and a
/// `process_count`-row process table (the "large process list" scenario).
fn bench_dashboard(server_count: usize, process_count: u32) -> DashboardSnapshot {
    let processes: Vec<ProcessStats> = (0..process_count)
        .map(|i| ProcessStats {
            pid: 1000 + i,
            program: format!("worker-{i}"),
            command: format!("/usr/bin/worker-{i} --slot {i}"),
            cpu_pct: f64::from(i % 100),
            memory_bytes: u64::from(i + 1) * 1_048_576,
            threads: Some(((i % 16) + 1) as usize),
            start_time: 1_700_000_000 + u64::from(i),
            gpu: Vec::new(),
        })
        .collect();

    let order: Vec<String> = (0..server_count)
        .map(|i| format!("http://10.0.0.{i}:8080/"))
        .collect();
    let mut endpoints = BTreeMap::new();
    let mut registry = BTreeMap::new();
    for (i, key) in order.iter().enumerate() {
        endpoints.insert(key.clone(), key.trim_end_matches('/').to_string());
        registry.insert(
            key.clone(),
            LlmStats {
                connected: true,
                metrics_available: true,
                slots_available: true,
                slot_count: 4,
                busy_slots: (i % 5) as u64,
                context_used: Some(1000 + i as u64),
                context_size: 8192,
                model: format!("model-{i}"),
                generation_tps: Some(20.0),
                backend: ServerBackend::LlamaCpp,
                ..Default::default()
            },
        );
    }
    let servers = build_server_summaries(
        &order,
        &endpoints,
        &registry,
        &UiState::default(),
        Instant::now(),
    );
    DashboardSnapshot {
        system: SystemStats {
            memory_used_bytes: 32 * 1024 * 1024 * 1024,
            memory_total_bytes: 64 * 1024 * 1024 * 1024,
            load_one: Some(1.0),
            load_five: Some(0.8),
            load_fifteen: Some(0.5),
            processes,
            ..Default::default()
        },
        servers,
        selected_server: order[0].clone(),
        llm: registry[&order[0]].clone(),
        ..Default::default()
    }
}

#[test]
fn perf_baseline_benchmark_for_dirty_only_redraw_and_polling() {
    let wall_start = Instant::now();
    let cpu_start = thread_cpu_ticks();

    // Redraw frequency is a deterministic *model* of the production
    // `RedrawScheduler`, not a wall-clock measurement. Keys carry a `model_`
    // prefix so the document cannot present them as measured data; the real
    // assertion lives in
    // `idle_redraw_rate_is_cut_from_the_50ms_poll_to_the_tick`.
    bench_emit("model_redraw_pre_optimization_per_s", 20);
    bench_emit("model_redraw_idle_per_s", simulated_idle_redraws(None));
    for interval_ms in [100u64, 250, 1000] {
        bench_emit(
            &format!("model_redraw_per_s_interval_{interval_ms}ms"),
            simulated_idle_redraws(Some(Duration::from_millis(interval_ms))),
        );
    }

    let snapshot = bench_dashboard(16, 400);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
    let mut ui_state = UiState::default();
    let render_frame = |terminal: &mut Terminal<ratatui::backend::TestBackend>,
                        ui_state: &mut UiState| {
        terminal
            .draw(|frame| {
                ui::draw(
                    frame,
                    &snapshot.system,
                    &snapshot.llm,
                    &snapshot.gpu,
                    &snapshot.gpu_map,
                    &snapshot.servers,
                    &snapshot.selected_server,
                    &[],
                    ui_state,
                    "http://10.0.0.0:8080",
                    1000,
                    false,
                )
            })
            .unwrap();
    };

    // First frame *in this test process* — not full program start: GPU
    // discovery, NVML init and sensor discovery run in `spawn_fast_worker`
    // before any frame is drawn and are not part of this measurement.
    let cold_start = Instant::now();
    render_frame(&mut terminal, &mut ui_state);
    bench_emit(
        "render_first_frame_in_process_us",
        cold_start.elapsed().as_micros(),
    );

    // Warm steady state. CPU comes from *this thread*, not the process: a
    // single ≤6 ms frame is below the 10 ms jiffy resolution, so the
    // per-frame CPU is derived from the sum over the whole loop.
    let warm_iterations = 60usize;
    let mut warm_render = Vec::with_capacity(warm_iterations);
    let render_cpu_before = thread_cpu_ticks();
    for _ in 0..warm_iterations {
        let frame_start = Instant::now();
        render_frame(&mut terminal, &mut ui_state);
        warm_render.push(frame_start.elapsed());
    }
    let render_cpu = render_cpu_before
        .zip(thread_cpu_ticks())
        .map(|(before, after)| after.saturating_sub(before));
    warm_render.sort();
    let warm_mean = bench_mean(&warm_render);
    bench_emit("render_warm_mean_us", warm_mean.as_micros());
    bench_emit(
        "render_warm_p95_us",
        bench_percentile(&warm_render, 0.95).as_micros(),
    );
    bench_emit(
        "render_warm_max_us",
        warm_render.last().map_or(0, |d| d.as_micros()),
    );
    bench_emit(
        "render_warm_max_fps",
        1_000_000u128 / warm_mean.as_micros().max(1),
    );
    if let Some(ticks) = render_cpu {
        // Thread CPU of this thread only, over the whole 60-frame loop: a
        // single ≤6 ms frame is below the 10 ms jiffy resolution, so the
        // per-frame CPU is *derived* from the sum (no per-frame jiffy
        // percentage is reported — at this window it would only show
        // quantisation).
        let cpu_us_per_frame = bench_ticks_to_us(ticks) as f64 / warm_iterations as f64;
        bench_emit("render_thread_cpu_ticks_total", ticks);
        bench_emit(
            "render_cpu_us_per_frame_derived",
            format!("{cpu_us_per_frame:.0}"),
        );
        // Derived idle-TUI CPU: redraws/s × CPU/frame. The pre-optimization
        // loop painted every ~50 ms poll iteration (20/s); the scheduler now
        // paints ~4/s idle. Labelled `derived_` because it multiplies a
        // *modelled* rate by the measured per-frame CPU.
        let idle_per_s = simulated_idle_redraws(None) as f64;
        bench_emit(
            "derived_idle_cpu_pct",
            format!("{:.3}", idle_per_s * cpu_us_per_frame / 1_000_000.0 * 100.0),
        );
        bench_emit(
            "derived_pre_optimization_cpu_pct",
            format!("{:.3}", 20.0 * cpu_us_per_frame / 1_000_000.0 * 100.0),
        );
    }
    if let Some(rss_kb) = self_status_field("VmRSS:") {
        bench_emit("rss_kb_after_render", rss_kb);
    }
    if let Some(threads) = self_status_field("Threads:") {
        bench_emit("threads_after_render", threads);
    }

    // TUI input latency: a selection change plus the immediate repaint it
    // triggers, repeated so a single scheduling hiccup cannot define the
    // figure. The loop's own terminal event poll (`event::poll`) is not
    // reproducible in a `TestBackend` harness, so this is the repaint share
    // only — documented in `docs/PERFORMANCE_BASELINE.md`.
    let input_reps = 30usize;
    let mut input_latencies = Vec::with_capacity(input_reps);
    for _ in 0..input_reps {
        let input_start = Instant::now();
        ui_state.move_process_selection(1, &snapshot.system.processes);
        render_frame(&mut terminal, &mut ui_state);
        input_latencies.push(input_start.elapsed());
    }
    input_latencies.sort();
    bench_emit(
        "input_to_frame_mean_us",
        bench_mean(&input_latencies).as_micros(),
    );
    bench_emit(
        "input_to_frame_p95_us",
        bench_percentile(&input_latencies, 0.95).as_micros(),
    );
    bench_emit(
        "input_to_frame_max_us",
        input_latencies.last().map_or(0, |d| d.as_micros()),
    );

    // HTTP poll latency for 1, 4 and 16 blocking-accept mock servers. One
    // cycle polls every server sequentially through `LlamaMonitor::sample`;
    // the mock answers on arrival, so the value is OrsikTop's client cost,
    // not a sleep quantum.
    for server_count in [1usize, 4, 16] {
        let mut addrs = Vec::new();
        let mut stops = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..server_count {
            let (addr, stop, handle) = spawn_blocking_mock("model-A", 4096, 1000, 500);
            addrs.push(addr);
            stops.push(stop);
            handles.push(handle);
        }
        let mut monitors: Vec<LlamaMonitor> = addrs
            .iter()
            .map(|addr| LlamaMonitor::new(&format!("http://{addr}")).unwrap())
            .collect();
        // Discard one warm-up cycle (connection setup, first parse).
        for monitor in &mut monitors {
            let _ = monitor.sample();
        }
        let cycles = 8usize;
        let mut cycle_times = Vec::with_capacity(cycles);
        for _ in 0..cycles {
            let cycle_start = Instant::now();
            for monitor in &mut monitors {
                let _ = monitor.sample();
            }
            cycle_times.push(cycle_start.elapsed());
        }
        cycle_times.sort();
        let cycle_mean = bench_mean(&cycle_times);
        bench_emit(
            &format!("poll_{server_count}_servers_cycle_mean_us"),
            cycle_mean.as_micros(),
        );
        bench_emit(
            &format!("poll_{server_count}_servers_cycle_max_us"),
            cycle_times.last().map_or(0, |d| d.as_micros()),
        );
        bench_emit(
            &format!("poll_{server_count}_servers_per_server_us"),
            cycle_mean.as_micros() / server_count as u128,
        );

        for ((stop, addr), handle) in stops.into_iter().zip(addrs).zip(handles) {
            stop_blocking_mock(&stop, addr, handle);
        }
    }

    // Whole-process footprint for this measurement window. RSS and thread
    // count are inherently process properties; they describe this test
    // process, not the production TUI (see the document's limitations).
    let wall = wall_start.elapsed();
    bench_emit("measurement_wall_ms", wall.as_millis());
    if let Some(rss_kb) = self_status_field("VmRSS:") {
        bench_emit("process_rss_kb", rss_kb);
    }
    if let Some(threads) = self_status_field("Threads:") {
        bench_emit("process_threads", threads);
    }
    if let (Some(cpu0), Some(cpu1)) = (cpu_start, thread_cpu_ticks()) {
        let ticks = cpu1.saturating_sub(cpu0);
        bench_emit("thread_cpu_ticks_total", ticks);
        if !wall.is_zero() {
            bench_emit(
                "thread_cpu_pct_avg",
                format!(
                    "{:.2}",
                    ticks as f64 / (wall.as_secs_f64() * BENCH_CLK_TCK) * 100.0
                ),
            );
        }
    }

    // Host / test conditions, read from inside the run: the sanctioned
    // test-desktop helper only reports host name, toolchain and GPU state, so
    // the CPU model, memory, kernel and CPU count are captured here.
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpuinfo
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .map(|rest| rest.trim_start_matches([':', ' ', '\t']).trim())
        {
            bench_emit("host_cpu_model", model);
        }
    }
    bench_emit(
        "host_logical_cpus",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
    );
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        bench_emit("host_kernel", release.trim());
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))
            .and_then(|line| line.split_whitespace().nth(1))
        {
            bench_emit("host_mem_total_kb", kb);
        }
    }

    // Structural invariants only: the scenarios produced real data and the
    // dashboard is the intended size. No wall-clock threshold here.
    assert_eq!(snapshot.servers.len(), 16, "16-server scenario is built");
    assert_eq!(snapshot.system.processes.len(), 400);
    assert!(!warm_render.is_empty());
}

/// The heavy half of the CFA-35 baseline: the real `/proc`+DRM
/// process-collector cycle and a longer-operation window that samples RSS
/// and thread count over time. `#[ignore]`d on purpose — reading the whole
/// host process table on every gate is too costly for the shared suite.
/// Run it through an approved single-threaded benchmark runner:
///   cargo test --all-targets --locked -- --ignored --test-threads=1 \
///     perf_baseline_soak_and_process_collection_benchmark
#[test]
#[ignore = "heavy: reads the whole host /proc table + DRM fdinfo; run via an approved single-threaded runner, not in the shared gate"]
fn perf_baseline_soak_and_process_collection_benchmark() {
    // Production fast-worker cycle: `refresh_processes_specifics` followed
    // by `collect_process_stats`. Both are timed, together and apart, so the
    // report does not understate the cycle by measuring only the collector.
    let mut system = System::new();
    let mut process_cache = ProcessCache::default();
    let mut drm_state = DrmSamplerState::default();
    let refresh_system = |system: &mut System| -> Duration {
        let start = Instant::now();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_exe(UpdateKind::Always),
        );
        start.elapsed()
    };

    let mut refresh_times = Vec::new();
    let mut collect_times = Vec::new();
    let mut cycle_times = Vec::new();
    let mut process_count = 0usize;
    for _ in 0..3 {
        let refresh = refresh_system(&mut system);
        let collect_start = Instant::now();
        let processes = collect_process_stats(&system, &mut process_cache, &mut drm_state);
        let collect = collect_start.elapsed();
        process_count = processes.len();
        refresh_times.push(refresh);
        collect_times.push(collect);
        cycle_times.push(refresh + collect);
    }
    refresh_times.sort();
    collect_times.sort();
    cycle_times.sort();
    bench_emit("process_count", process_count);
    bench_emit(
        "process_refresh_mean_us",
        bench_mean(&refresh_times).as_micros(),
    );
    bench_emit(
        "process_collect_mean_us",
        bench_mean(&collect_times).as_micros(),
    );
    bench_emit(
        "process_collect_max_us",
        collect_times.last().map_or(0, |d| d.as_micros()),
    );
    bench_emit(
        "process_cycle_mean_us",
        bench_mean(&cycle_times).as_micros(),
    );
    bench_emit(
        "process_cycle_max_us",
        cycle_times.last().map_or(0, |d| d.as_micros()),
    );

    // Component split of the collector (CFA-36): the collector cycle is the
    // sum of the per-process thread-count read and the per-process DRM fd
    // scan. Both phases are re-timed here (over the same process table) so
    // an optimization targets the dominant one. Reported, never asserted.
    let mut thread_read_total = Duration::ZERO;
    let mut thread_read_failures = 0usize;
    for pid in system.processes().keys() {
        let start = Instant::now();
        if read_process_thread_count(pid.as_u32()).is_none() {
            thread_read_failures += 1;
        }
        thread_read_total += start.elapsed();
    }
    bench_emit(
        "process_threads_read_total_us",
        thread_read_total.as_micros(),
    );
    bench_emit("process_threads_read_failures", thread_read_failures);

    let mut drm_scan_total = Duration::ZERO;
    let mut drm_procs_with_gpu = 0usize;
    let mut drm_scan_state = DrmSamplerState::default();
    let drm_now = Instant::now();
    for (pid, process) in system.processes().iter() {
        let identity = ProcessIdentity::new(pid.as_u32(), process.start_time());
        let start = Instant::now();
        let gpus = sample_process_gpus(&RealSys, drm_now, identity, &mut drm_scan_state);
        drm_scan_total += start.elapsed();
        if !gpus.is_empty() {
            drm_procs_with_gpu += 1;
        }
    }
    bench_emit("process_drm_scan_total_us", drm_scan_total.as_micros());
    bench_emit("process_drm_procs_with_gpu", drm_procs_with_gpu);

    // Longer-operation window: render the full dashboard continuously and
    // sample RSS / threads once per second to expose growth over time. This
    // is the closest a unit harness gets to "longer monitoring operation"; a
    // full soak belongs to EPIC 4.1.
    let snapshot = bench_dashboard(16, 400);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
    let mut ui_state = UiState::default();
    let soak_window = Duration::from_secs(10);
    let soak_start = Instant::now();
    let mut soak_frames = 0u64;
    let mut next_sample_s = 1u64;
    while soak_start.elapsed() < soak_window {
        terminal
            .draw(|frame| {
                ui::draw(
                    frame,
                    &snapshot.system,
                    &snapshot.llm,
                    &snapshot.gpu,
                    &snapshot.gpu_map,
                    &snapshot.servers,
                    &snapshot.selected_server,
                    &[],
                    &mut ui_state,
                    "http://10.0.0.0:8080",
                    1000,
                    false,
                )
            })
            .unwrap();
        soak_frames += 1;
        let elapsed_s = soak_start.elapsed().as_secs();
        if elapsed_s >= next_sample_s {
            if let Some(rss_kb) = self_status_field("VmRSS:") {
                bench_emit(&format!("soak_rss_kb_at_{elapsed_s}s"), rss_kb);
            }
            if let Some(threads) = self_status_field("Threads:") {
                bench_emit(&format!("soak_threads_at_{elapsed_s}s"), threads);
            }
            next_sample_s = elapsed_s + 1;
        }
    }
    bench_emit("soak_frames_total", soak_frames);
    bench_emit("soak_window_s", soak_window.as_secs());

    assert!(process_count > 0, "host process table is non-empty");
    assert!(soak_frames > 0, "soak rendered at least one frame");
}
