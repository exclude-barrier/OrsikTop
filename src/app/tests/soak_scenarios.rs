//! Soak scenarios moved from the former inline `mod tests` (smoke/long tests live in `soak.rs`).
use super::{
    assert_no_cross_mixing, bench_emit, join_within, programmable_mock, self_status_field,
    spawn_llm_worker, start_soak_worker, test_spec, wait_for_connected, LlmCommand, LlmSample,
    LocalServerIdentity, MockMode, ProgrammableMock, ServerSpec, UiState, MAX_MONITORED_SERVERS,
    MIN_LLM_POLL_MS,
};

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn soak_worker_survives_malformed_json_and_recovers_without_mixing() {
    // A answers malformed JSON, B is healthy. B's telemetry must stay its
    // own while A is broken, and A must recover to its own model once the
    // endpoint is healthy again — never borrowing B's.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let b = programmable_mock("model-B".to_string(), 8192, 7_000);
    a.set_mode(MockMode::BadJson);

    let key_a = crate::domain::server_key(&a.base());
    let key_b = crate::domain::server_key(&b.base());

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(300));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    // Phase 1: B reports its own model while A is malformed.
    let mut b_ok = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !b_ok {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            let server = sample.server.clone();
            assert!(
                sample.server == key_a || sample.server == key_b,
                "unknown server key published: {server}"
            );
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
            if sample.server == key_b && sample.stats.connected && sample.stats.model == "model-B" {
                b_ok = true;
            }
        }
    }
    assert!(
        b_ok,
        "the healthy server must report its own model while A is malformed"
    );

    // Phase 2: A recovers to its own model.
    a.set_mode(MockMode::Healthy);
    let mut a_ok = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !a_ok {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
            if sample.server == key_a && sample.stats.connected && sample.stats.model == "model-A" {
                a_ok = true;
            }
        }
    }
    assert!(
        a_ok,
        "A must recover to its own model after the malformed period"
    );

    stop.store(true, Ordering::SeqCst);
    let _ = worker.join();
}

#[test]
fn soak_polls_the_maximum_monitored_server_count_independently() {
    // Every one of MAX_MONITORED_SERVERS servers must be polled, each with
    // its own model — the cap must not silently drop or cross-wire a server.
    let mocks: Vec<ProgrammableMock> = (0..MAX_MONITORED_SERVERS)
        .map(|i| programmable_mock(format!("model-{i}"), 4096, 1_000 * (i as u64 + 1)))
        .collect();
    let specs: Vec<ServerSpec> = mocks
        .iter()
        .map(|mock| {
            let key = crate::domain::server_key(&mock.base());
            test_spec(&key, &mock.base())
        })
        .collect();
    let keys: Vec<String> = specs.iter().map(|spec| spec.key.clone()).collect();

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(300));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        specs,
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    // Only a sample that is actually *connected* and carries this server's
    // own model counts as "polled", so a worker that published nothing but
    // error samples for every server cannot pass; each mock has a unique
    // model, and all share the same n_ctx.
    let mut seen: HashSet<String> = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && seen.len() < keys.len() {
        if let Ok(sample) = sample_rx.recv_timeout(Duration::from_millis(500)) {
            let server = sample.server.clone();
            let idx = keys
                .iter()
                .position(|key| *key == sample.server)
                .unwrap_or_else(|| panic!("unknown server key published: {server}"));
            assert_no_cross_mixing(&sample, &keys[idx], &format!("model-{idx}"), 4096);
            if sample.stats.connected && sample.stats.model == format!("model-{idx}") {
                seen.insert(sample.server);
            }
        }
    }
    let seen_len = seen.len();
    let key_count = keys.len();
    assert_eq!(
        seen_len, key_count,
        "all {MAX_MONITORED_SERVERS} servers must be polled, saw {seen_len} of {key_count}"
    );

    stop.store(true, Ordering::SeqCst);
    let _ = worker.join();
}

#[test]
fn soak_clean_shutdown_is_bounded_while_a_server_hangs() {
    // The critical blocking-shutdown case is a *hung* server whose request
    // is in flight for the whole client timeout, not a merely slow one:
    // joining the worker after `stop` must still complete within a bound,
    // because the in-flight request is cut off by `LLM_REQUEST_TIMEOUT_MS`.
    // The mock is held open until `stop`, so `hang_active()` proves a
    // request is genuinely in flight when shutdown begins — no race against
    // the next poll cycle.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    a.set_mode(MockMode::Hang);
    let key_a = crate::domain::server_key(&a.base());

    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(300));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, _sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        vec![test_spec(&key_a, &a.base())],
        Arc::clone(&refresh),
        Arc::clone(&offline),
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );

    // Wait until the mock actually holds a request open.
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.hang_active() == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        a.hang_active() > 0,
        "the mock never held a request open before shutdown"
    );

    stop.store(true, Ordering::SeqCst);
    assert!(
        join_within(worker, Duration::from_secs(5)),
        "the worker did not shut down within 5 s (blocking shutdown)"
    );
}

#[test]
fn soak_reconnects_after_request_timeout() {
    // A server that holds the connection open past the client's request
    // timeout must fail (not merely answer late), and must recover to
    // connected with its own model once it answers again — the real
    // "reconnect after timeout" path. `MockMode::Slow` is below the timeout,
    // so only `Hang` actually triggers a client timeout. `Hang` holds the
    // connection until the mode changes, so the disconnect seen in phase 1
    // can only be the client's own `LLM_REQUEST_TIMEOUT_MS` firing — a client
    // with no or a longer timeout would stay blocked here, not observe a
    // connection drop.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    a.set_mode(MockMode::Hang);
    let key_a = crate::domain::server_key(&a.base());

    let (stop, samples, _cmds, worker) = start_soak_worker(vec![test_spec(&key_a, &a.base())], 300);

    // Phase 1: the hung server must not report a (stale) connected sample.
    let mut saw_timeout = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !saw_timeout {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            if sample.server == key_a && !sample.stats.connected {
                saw_timeout = true;
            }
        }
    }
    assert!(
        saw_timeout,
        "a request held past the timeout must be reported as disconnected"
    );

    // Phase 2: once the server answers, it must recover to its own model.
    a.set_mode(MockMode::Healthy);
    wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
}

#[test]
fn soak_counter_reset_is_reported_as_unavailable_not_a_fantasy_rate() {
    // A restarted llama.cpp restarts its cumulative counters at zero. The
    // monitor must treat that as a reset (live rate unavailable), never as a
    // negative or absurd delta, and re-establish a fresh baseline. The
    // endpoint (and identity) stays the same, so the monitor observes the
    // backwards counters directly.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let key_a = crate::domain::server_key(&a.base());

    let (stop, samples, _cmds, worker) = start_soak_worker(vec![test_spec(&key_a, &a.base())], 300);

    // Establish a live generation rate first.
    let mut saw_rate = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !saw_rate {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            if sample.stats.connected && sample.stats.generation_tps.is_some() {
                saw_rate = true;
            }
        }
    }
    assert!(saw_rate, "a live generation rate must be established first");

    // Restart the counters at zero without changing the endpoint.
    a.restart_counters();
    let mut saw_reset = false;
    let mut saw_fresh = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !(saw_reset && saw_fresh) {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            for (label, value) in [
                ("prompt_tps", sample.stats.prompt_tps),
                ("generation_tps", sample.stats.generation_tps),
            ] {
                if let Some(value) = value {
                    assert!(
                        value.is_finite() && (0.0..1e9).contains(&value),
                        "a counter reset produced a fantasy {label}={value}"
                    );
                }
            }
            if sample.stats.connected && sample.stats.generation_tps.is_none() {
                saw_reset = true;
            }
            // Only a *post-reset* live rate proves a fresh baseline was
            // re-established. A rate fetched before the reset (still in
            // flight when `restart_counters()` ran) must not count, or the
            // test could pass without ever observing the reset recovery.
            if saw_reset && sample.stats.connected && sample.stats.generation_tps.is_some() {
                saw_fresh = true;
            }
        }
    }
    assert!(
        saw_reset,
        "the counter reset must report an unavailable rate"
    );
    assert!(saw_fresh, "a fresh baseline must re-establish a live rate");

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
}

#[test]
fn soak_set_servers_churn_keeps_generations_and_polling_bounded() {
    // Repeated `SetServers` rounds: the generation must never go backwards
    // (a stale sample from a replaced set must not surface after a newer
    // one), the added server must be polled under the new generation, a
    // removed server must stop, and a same-key restart (new identity + a
    // counter reset) must resume with the server's own values.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let b = programmable_mock("model-B".to_string(), 8192, 2_000);
    let c = programmable_mock("model-C".to_string(), 16384, 3_000);
    let key_a = crate::domain::server_key(&a.base());
    let key_b = crate::domain::server_key(&b.base());
    let key_c = crate::domain::server_key(&c.base());

    let (stop, samples, cmds, worker) = start_soak_worker(
        vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
        300,
    );

    let mut last_generation = 0u64;
    // Phase 1: settle the initial A+B set at generation 0.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut established = false;
    while Instant::now() < deadline && !established {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            assert!(
                sample.generation >= last_generation,
                "generation went backwards ({} after {})",
                sample.generation,
                last_generation
            );
            last_generation = sample.generation;
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
            if sample.server == key_b && sample.stats.connected && sample.stats.model == "model-B" {
                established = true;
            }
        }
    }
    assert!(established, "the initial A+B set must be polled");

    // Phase 2: switch to a new set {A (new identity), C} at generation 1.
    // A keeps its key but its identity changed, so its monitor is rebuilt;
    // its counters restart at zero. B is removed, C is added.
    a.restart_counters();
    cmds.send(LlmCommand::SetServers {
        generation: 1,
        servers: vec![
            ServerSpec {
                identity: Some(LocalServerIdentity {
                    pid: 999,
                    start_time: 7,
                }),
                ..test_spec(&key_a, &a.base())
            },
            test_spec(&key_c, &c.base()),
        ],
    })
    .unwrap();

    let mut a_gen1 = false;
    let mut c_gen1 = false;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline && !(a_gen1 && c_gen1) {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            assert!(
                sample.generation >= last_generation,
                "generation went backwards ({} after {})",
                sample.generation,
                last_generation
            );
            last_generation = last_generation.max(sample.generation);
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            assert_no_cross_mixing(&sample, &key_c, "model-C", 16384);
            if sample.generation == 1
                && sample.server == key_a
                && sample.stats.connected
                && sample.stats.model == "model-A"
            {
                a_gen1 = true;
            }
            if sample.generation == 1
                && sample.server == key_c
                && sample.stats.connected
                && sample.stats.model == "model-C"
            {
                c_gen1 = true;
            }
        }
    }
    assert!(
        a_gen1,
        "the restarted server A must resume with its own values under generation 1"
    );
    assert!(
        c_gen1,
        "the newly added server C must be polled under generation 1"
    );

    // Once the switch has settled, the removed server B must not be polled
    // under the new generation.
    thread::sleep(Duration::from_millis(600));
    let mut b_gen1 = 0usize;
    while let Ok(sample) = samples.try_recv() {
        assert!(sample.generation >= last_generation);
        if sample.server == key_b && sample.generation == 1 {
            b_gen1 += 1;
        }
    }
    assert_eq!(
        b_gen1, 0,
        "a server removed from the set must not be polled under the new generation"
    );

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
}

#[test]
fn soak_selection_switch_keeps_each_servers_own_history() {
    // Partial integration coverage: drive the real worker and feed its
    // samples through the production selection seam
    // (`UiState::record_llm_sample` + `select_llm_history`) while the
    // selected server switches A -> B, and check that both servers keep an
    // independently measured sample interval (via the public accessor) after
    // the switch. It does **not** assert the *active* history's content, nor
    // prove no cross-mixing: the interval accessor cannot tell whose samples
    // built a history, and background servers accumulate their interval
    // regardless of selection. Those properties are covered at the unit
    // level by `ui::tests::per_server_histories_are_independent_and_survive_selection`
    // and the `assert_no_cross_mixing` checks in the failure-mode tests here.
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let b = programmable_mock("model-B".to_string(), 8192, 9_000);
    let key_a = crate::domain::server_key(&a.base());
    let key_b = crate::domain::server_key(&b.base());

    let (stop, samples, _cmds, worker) = start_soak_worker(
        vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
        300,
    );

    let mut ui = UiState::default();
    let mut selected = key_a.clone();
    ui.select_llm_history(&selected);

    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline
        && !(ui.server_sample_interval_ms(&key_a).is_some()
            && ui.server_sample_interval_ms(&key_b).is_some())
    {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            ui.record_llm_sample(&sample.server, &selected, &sample.stats);
            // Once A has its own measured interval, switch the selection.
            if selected == key_a && ui.server_sample_interval_ms(&key_a).is_some() {
                selected = key_b.clone();
                ui.select_llm_history(&key_b);
            }
        }
    }
    assert!(
        ui.server_sample_interval_ms(&key_a).is_some(),
        "A must keep its own history across the selection switch"
    );
    assert!(
        ui.server_sample_interval_ms(&key_b).is_some(),
        "B must accumulate its own history after the switch"
    );

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
}

/// The longer half of EPIC 4.1: drives every failure mode across two servers
/// over a sustained window, then asserts resilience (samples keep flowing,
/// no cross-mixing), a real stale-state transition (a failure phase outlasts
/// the grace hold, so the server is published as disconnected rather than
/// holding a last-good forever), and recovery to each server's own values.
/// Resource footprint is deliberately **not measured** here: `/proc/self` is
/// process-global and the shared gate runs tests in parallel, so such a bound
/// is meaningless, and this test asserts no resource values at all; the
/// strict probe lives in the `#[ignore]`d single-threaded test below.
#[test]
fn soak_endurance_all_failure_modes_without_mixing_and_recovers() {
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let b = programmable_mock("model-B".to_string(), 8192, 2_000);

    let key_a = crate::domain::server_key(&a.base());
    let key_b = crate::domain::server_key(&b.base());
    let known = [key_a.clone(), key_b.clone()];

    // A short grace so a failure phase visibly outlasts the held last-good;
    // B flips between modes so the two servers run under mismatched
    // conditions (a mismatch cannot cross-wire them).
    let (stop, samples, _cmds, worker) = start_soak_worker(
        vec![test_spec(&key_a, &a.base()), test_spec(&key_b, &b.base())],
        200,
    );

    let script = [
        (MockMode::Healthy, MockMode::Healthy, 500u64),
        (MockMode::Disconnect, MockMode::BadJson, 1_200),
        (MockMode::Empty, MockMode::MetricsOff, 500),
        (MockMode::Slow, MockMode::Disconnect, 500),
        (MockMode::Healthy, MockMode::Healthy, 1_200),
    ];

    let mut observed = 0usize;
    let mut saw_a_disconnected = false;
    for (a_mode, b_mode, hold_ms) in script {
        a.set_mode(a_mode);
        b.set_mode(b_mode);
        let phase_end = Instant::now() + Duration::from_millis(hold_ms);
        while Instant::now() < phase_end {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(50)) {
                let server = sample.server.clone();
                assert!(
                    known.contains(&sample.server),
                    "unknown server key published: {server}"
                );
                assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
                assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
                if sample.server == key_a && !sample.stats.connected {
                    saw_a_disconnected = true;
                }
                observed += 1;
            }
        }
    }
    assert!(
        observed > 0,
        "the soak must observe samples (no endless/spinning cycle)"
    );
    assert!(
            saw_a_disconnected,
            "a failure phase longer than the grace must publish A as disconnected, not hold last-good forever"
        );

    // Recovery: both servers healthy again report their own values.
    a.set_mode(MockMode::Healthy);
    b.set_mode(MockMode::Healthy);
    let mut a_ok = false;
    let mut b_ok = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !(a_ok && b_ok) {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(300)) {
            assert_no_cross_mixing(&sample, &key_a, "model-A", 4096);
            assert_no_cross_mixing(&sample, &key_b, "model-B", 8192);
            if sample.server == key_a && sample.stats.connected && sample.stats.model == "model-A" {
                a_ok = true;
            }
            if sample.server == key_b && sample.stats.connected && sample.stats.model == "model-B" {
                b_ok = true;
            }
        }
    }
    assert!(
        a_ok && b_ok,
        "both servers must recover to their own values (a={a_ok}, b={b_ok})"
    );

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
}

/// Strict, deterministic leak probe for EPIC 4.1's "keine unkontrollierte
/// Ressourcenentwicklung". `#[ignore]`d on purpose: `/proc/self/status`
/// (`Threads:`, `VmRSS:`) is **process-global**, so in the shared parallel
/// gate the values are dominated by the other concurrently running tests and
/// no bound on them can detect a worker leak (this is why the in-gate
/// endurance test measures no resources at all). Run it single-threaded via
/// an approved runner so `/proc/self` is this test alone and the bounds are
/// meaningful:
///   cargo test --all-targets --locked -- --ignored --test-threads=1 \
///     soak_many_set_servers_rounds_bound_resources_single_threaded
///
/// It churns the worker's monitor set through `MANY` `SetServers` rounds,
/// each a fresh process identity (i.e. a fresh `LlamaMonitor`/HTTP client)
/// plus an add/remove of a second server, then asserts the footprint does not
/// grow with the round count. A per-round leak of a client's threads or
/// buffers blows the bound, so the probe is red if one is introduced.
#[test]
#[ignore = "heavy/leak probe: run single-threaded via an approved runner (see doc comment); process-global /proc is meaningless in the parallel gate"]
fn soak_many_set_servers_rounds_bound_resources_single_threaded() {
    const MANY: u64 = 60;
    let a = programmable_mock("model-A".to_string(), 4096, 1_000);
    let b = programmable_mock("model-B".to_string(), 8192, 2_000);
    let key_a = crate::domain::server_key(&a.base());
    let key_b = crate::domain::server_key(&b.base());

    let (stop, samples, cmds, worker) = start_soak_worker(vec![test_spec(&key_a, &a.base())], 200);

    // Settle with one monitor so the baseline includes the worker's steady
    // thread set; every mock thread is already running.
    wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));
    let threads_baseline = self_status_field("Threads:").unwrap_or(0);
    let rss_baseline = self_status_field("VmRSS:").unwrap_or(0);

    // High churn: a new identity every round forces a fresh monitor (HTTP
    // client) each round; B is added/removed, so monitors are also dropped.
    for round in 1..=MANY {
        let mut servers = vec![ServerSpec {
            identity: Some(LocalServerIdentity {
                pid: 2_000 + round as u32,
                start_time: round,
            }),
            ..test_spec(&key_a, &a.base())
        }];
        if round % 2 == 0 {
            servers.push(test_spec(&key_b, &b.base()));
        }
        cmds.send(LlmCommand::SetServers {
            generation: round,
            servers,
        })
        .unwrap();
        // Pace one poll cycle per round so the command is actually applied
        // and a monitor rebuilds; drain the bounded channel meanwhile.
        let cycle_end = Instant::now() + Duration::from_millis(MIN_LLM_POLL_MS + 20);
        while Instant::now() < cycle_end {
            let _ = samples.recv_timeout(Duration::from_millis(20));
        }
    }

    // The worker must still be polling after the churn (no wedge).
    wait_for_connected(&samples, &key_a, "model-A", Duration::from_secs(10));

    stop.store(true, Ordering::SeqCst);
    assert!(join_within(worker, Duration::from_secs(5)));
    // The worker (and its monitors/clients) is joined, but a just-finished
    // reqwest blocking runtime can take a moment to tear down its worker
    // threads; this sleep lets that settle before counting. The two mock
    // threads are still running — they are joined only when `a`/`b` drop at
    // the end of this function — and were already running when
    // `threads_baseline` was taken, so they cancel out of the comparison.
    thread::sleep(Duration::from_millis(200));

    let threads_after = self_status_field("Threads:").unwrap_or(0);
    let rss_after = self_status_field("VmRSS:").unwrap_or(0);
    bench_emit("soak_churn_rounds", MANY);
    bench_emit("soak_churn_threads_baseline", threads_baseline);
    bench_emit("soak_churn_threads_after", threads_after);
    bench_emit(
        "soak_churn_rss_growth_kb",
        rss_after.saturating_sub(rss_baseline),
    );

    // Single-threaded: after the worker is joined the footprint must return
    // to (roughly) the baseline taken with the worker running; a per-round
    // leak over 60 rounds would be far above this slack.
    assert!(
            threads_after <= threads_baseline + 8,
            "threads grew over {MANY} SetServers rounds ({threads_baseline} -> {threads_after}); a monitor/client leak?"
        );
    let rss_growth = rss_after.saturating_sub(rss_baseline);
    assert!(
        rss_growth < 64 * 1024,
        "RSS grew by {rss_growth} KiB over {MANY} SetServers rounds (uncontrolled growth?)"
    );
}
