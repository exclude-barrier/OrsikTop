//! Server-registry tests: reconcile, identity, GPU mapping, summaries and sample stabilization.
use super::{
    accepts_llm_sample, apply_fast_snapshot, build_server_summaries, changed_identity_keys, ident,
    map_server_identity, new_server_monitor, publish_selected_mapping, rebuild_server_monitors,
    reconcile_server, selected_identity_of, spawn_body_metrics_server, spawn_empty_metrics_server,
    spawn_flaky_metrics_server, spawn_metrics_off_server, stabilize_llm_sample, target,
    update_resolved_server, DashboardSnapshot, FastSnapshot, GpuMapping, LlmSample, LlmStats,
    LocalServerIdentity, ServerChange, ServerSpec, SystemStats, UiState,
};

use crate::domain::ServerBackend;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn reconcile_detects_endpoint_identity_and_origin_independently() {
    let unchanged = ServerChange {
        endpoint: false,
        pid: false,
        auto: false,
    };
    let base = target("http://127.0.0.1:8081", Some(ident(5000, 1)), true);

    assert_eq!(reconcile_server(&base, &base), unchanged);

    // Restart on the same port: endpoint stable, identity changed.
    assert_eq!(
        reconcile_server(
            &base,
            &target("http://127.0.0.1:8081", Some(ident(6000, 1)), true)
        ),
        ServerChange {
            endpoint: false,
            pid: true,
            auto: false,
        }
    );

    // Reused PID (same number, new start time) still counts as a changed
    // identity, so the mapping is invalidated even before the next resync.
    assert_eq!(
        reconcile_server(
            &base,
            &target("http://127.0.0.1:8081", Some(ident(5000, 2)), true)
        ),
        ServerChange {
            endpoint: false,
            pid: true,
            auto: false,
        }
    );

    // Server disappeared: identity cleared and origin flips to manual.
    assert_eq!(
        reconcile_server(&base, &target("http://127.0.0.1:8081", None, false)),
        ServerChange {
            endpoint: false,
            pid: true,
            auto: true,
        }
    );

    // New endpoint entirely.
    assert_eq!(
        reconcile_server(
            &base,
            &target("http://127.0.0.1:9090", Some(ident(5000, 1)), true)
        ),
        ServerChange {
            endpoint: true,
            pid: false,
            auto: false,
        }
    );

    // Origin-only change is still detected (defensive: the origin is
    // tracked separately, not inferred from the URL).
    assert_eq!(
        reconcile_server(
            &base,
            &target("http://127.0.0.1:8081", Some(ident(5000, 1)), false)
        ),
        ServerChange {
            endpoint: false,
            pid: false,
            auto: true,
        }
    );
}

#[test]
fn selected_identity_and_mapping_publication() {
    let (tx, rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
    let specs = vec![
        ServerSpec {
            key: "a".to_string(),
            endpoint: "http://a".to_string(),
            identity: Some(ident(5000, 1)),
        },
        ServerSpec {
            key: "b".to_string(),
            endpoint: "http://b".to_string(),
            identity: None,
        },
    ];
    // The identity comes from the selected spec; a remote/configured server
    // (no local process) yields None and must stay UNKNOWN.
    assert_eq!(selected_identity_of(&specs, "a"), Some(ident(5000, 1)));
    assert_eq!(selected_identity_of(&specs, "b"), None);
    assert_eq!(selected_identity_of(&specs, "missing"), None);

    // Publishing advances the generation and sends the selected identity, so
    // a stale mapping of a previously selected server is invalidated.
    let mut generation = 0u64;
    publish_selected_mapping(selected_identity_of(&specs, "a"), &mut generation, &tx);
    assert_eq!(generation, 1);
    assert_eq!(rx.try_recv().unwrap(), (1, Some(ident(5000, 1))));
    publish_selected_mapping(selected_identity_of(&specs, "b"), &mut generation, &tx);
    assert_eq!(generation, 2);
    assert_eq!(rx.try_recv().unwrap(), (2, None));
}

#[test]
fn update_resolved_server_only_relabels() {
    let mut current = target("http://127.0.0.1:8081", Some(ident(5000, 1)), true);
    update_resolved_server(&mut current, target("http://127.0.0.1:9090", None, false));
    assert_eq!(current.endpoint, "http://127.0.0.1:9090");
    assert!(!current.auto);
    assert_eq!(current.identity, None);
}

#[test]
fn stale_llm_sample_is_rejected_across_generations() {
    // A sample is accepted only under the current generation; the A→B→A
    // case is handled because the second A session has a new generation.
    let a_sample = LlmSample {
        server: "http://a:1/".to_string(),
        generation: 0,
        stats: LlmStats {
            model: "A".to_string(),
            ..Default::default()
        },
    };
    assert!(accepts_llm_sample(0, &a_sample));
    let b_sample = LlmSample {
        server: "http://b:1/".to_string(),
        generation: 1,
        stats: LlmStats {
            model: "B".to_string(),
            ..Default::default()
        },
    };
    assert!(accepts_llm_sample(1, &b_sample));
    // A delayed result from the first A session is rejected under B and
    // under the second A session (generation 2).
    assert!(!accepts_llm_sample(1, &a_sample));
    assert!(!accepts_llm_sample(2, &a_sample));
}

#[test]
fn stale_gpu_mapping_is_not_applied_but_system_data_is() {
    let mapped_b = crate::domain::GpuMapping::Single(mapped("0000:02:00.0"));
    let mut snapshot = DashboardSnapshot {
        gpu_map: mapped_b.clone(),
        ..DashboardSnapshot::default()
    };
    let mut ui_state = UiState::default();

    // A snapshot from the previous mapping generation carries A's mapping
    // plus fresh system/GPU data. The mapping must be ignored; the rest
    // must still be applied.
    let stale = FastSnapshot {
        gpu: crate::domain::GpuStats {
            name: "fresh-gpu".to_string(),
            ..Default::default()
        },
        system: SystemStats {
            cpu_usage: 42.0,
            ..Default::default()
        },
        gpu_map: crate::domain::GpuMapping::Single(mapped("0000:01:00.0")),
        mapping_generation: 1,
    };
    apply_fast_snapshot(&mut snapshot, &mut ui_state, 2, stale);

    assert_eq!(
        snapshot.gpu_map, mapped_b,
        "stale GPU mapping must not overwrite the current one"
    );
    assert_eq!(snapshot.system.cpu_usage, 42.0, "system data still applies");
    assert_eq!(snapshot.gpu.name, "fresh-gpu", "GPU data still applies");
}

#[test]
fn integrated_selection_switch_rejects_delayed_gpu_mapping() {
    use crate::domain::{DeviceId, GpuMapping};

    // Two locally verified servers on distinct GPUs. Everything below runs
    // through the production seams: `selected_identity_of` +
    // `publish_selected_mapping` (selection → generation) and
    // `map_server_identity` + `apply_fast_snapshot` (evidence → TUI), with
    // no GPU hardware and no HTTP.
    let gpus = vec![
        discovered_gpu("0000:01:00.0"),
        discovered_gpu("0000:02:00.0"),
    ];
    let ident_a = ident(5000, 1);
    let ident_b = ident(6000, 1);
    let specs = vec![
        ServerSpec {
            key: "a".to_string(),
            endpoint: "http://127.0.0.1:8081".to_string(),
            identity: Some(ident_a),
        },
        ServerSpec {
            key: "b".to_string(),
            endpoint: "http://127.0.0.1:8082".to_string(),
            identity: Some(ident_b),
        },
    ];
    let evidence_a = vec![DeviceId::new(Some("0000:01:00.0".into()), None)];
    let evidence_b = vec![DeviceId::new(Some("0000:02:00.0".into()), None)];

    let (tx, _rx) = mpsc::channel::<(u64, Option<LocalServerIdentity>)>();
    let mut mapping_generation = 0u64;
    let mut selected = "a".to_string();
    let mut snapshot = DashboardSnapshot::default();
    let mut ui_state = UiState::default();
    let publish = |selected: &str, generation: &mut u64, snapshot: &mut DashboardSnapshot| {
        publish_selected_mapping(selected_identity_of(&specs, selected), generation, &tx);
        // The app clears the shown mapping synchronously on any selection
        // change so no stale device label survives the switch.
        snapshot.gpu_map = GpuMapping::None;
    };

    // --- Select A → GPU 1. ---
    publish(&selected, &mut mapping_generation, &mut snapshot);
    let generation_a = mapping_generation;
    let map_a = map_server_identity(ident_a, Some(1), evidence_a.clone(), Vec::new(), &gpus);
    assert!(matches!(map_a, GpuMapping::Single(_)), "A maps to one GPU");
    apply_fast_snapshot(
        &mut snapshot,
        &mut ui_state,
        mapping_generation,
        FastSnapshot {
            gpu_map: map_a.clone(),
            mapping_generation: generation_a,
            ..FastSnapshot::default()
        },
    );
    assert_eq!(snapshot.gpu_map, map_a, "GPU 1 displayed for A");

    // --- Select B → A's mapping cleared immediately. ---
    selected = "b".to_string();
    publish(&selected, &mut mapping_generation, &mut snapshot);
    assert!(
        snapshot.gpu_map.is_empty(),
        "switching servers must clear the previous attribution immediately"
    );

    // A delayed snapshot produced for A arrives under B → rejected.
    apply_fast_snapshot(
        &mut snapshot,
        &mut ui_state,
        mapping_generation,
        FastSnapshot {
            gpu_map: map_a.clone(),
            mapping_generation: generation_a,
            ..FastSnapshot::default()
        },
    );
    assert!(
        snapshot.gpu_map.is_empty(),
        "delayed A snapshot must not be applied under B"
    );

    // B's valid snapshot → GPU 2.
    let generation_b = mapping_generation;
    let map_b = map_server_identity(ident_b, Some(1), evidence_b.clone(), Vec::new(), &gpus);
    assert!(matches!(map_b, GpuMapping::Single(_)), "B maps to one GPU");
    apply_fast_snapshot(
        &mut snapshot,
        &mut ui_state,
        mapping_generation,
        FastSnapshot {
            gpu_map: map_b.clone(),
            mapping_generation: generation_b,
            ..FastSnapshot::default()
        },
    );
    assert_eq!(snapshot.gpu_map, map_b, "GPU 2 displayed for B");
    assert_ne!(map_a, map_b, "A and B are attributed to distinct GPUs");

    // --- Switch back to A. ---
    selected = "a".to_string();
    publish(&selected, &mut mapping_generation, &mut snapshot);
    assert!(
        snapshot.gpu_map.is_empty(),
        "B mapping cleared on return to A"
    );
    apply_fast_snapshot(
        &mut snapshot,
        &mut ui_state,
        mapping_generation,
        FastSnapshot {
            gpu_map: map_b.clone(),
            mapping_generation: generation_b,
            ..FastSnapshot::default()
        },
    );
    assert!(
        snapshot.gpu_map.is_empty(),
        "delayed B snapshot must not be applied under the second A selection"
    );
    apply_fast_snapshot(
        &mut snapshot,
        &mut ui_state,
        mapping_generation,
        FastSnapshot {
            gpu_map: map_a.clone(),
            mapping_generation,
            ..FastSnapshot::default()
        },
    );
    assert_eq!(snapshot.gpu_map, map_a, "fresh A snapshot re-shows GPU 1");
}

#[test]
fn reused_pid_with_a_new_start_time_gets_no_gpu_mapping() {
    let gpus = vec![discovered_gpu("0000:01:00.0")];
    let evidence = vec![crate::domain::DeviceId::new(
        Some("0000:01:00.0".into()),
        None,
    )];
    let old = ident(4242, 1000);

    // Same PID number, different start time: a different process now owns
    // the PID. GPU evidence is present, but it must not be attributed to
    // the earlier server — the mapping is invalid immediately, not only
    // after the next resync.
    let stale = map_server_identity(old, Some(2000), evidence.clone(), Vec::new(), &gpus);
    assert!(
        !matches!(stale, GpuMapping::Single(_) | GpuMapping::Multi(_)),
        "reused PID must not map to the earlier server: {stale:?}"
    );
    assert!(stale.is_empty(), "expected no attribution, got {stale:?}");

    // An unreadable start time is also untrusted.
    let unreadable = map_server_identity(old, None, evidence.clone(), Vec::new(), &gpus);
    assert!(
        unreadable.is_empty(),
        "expected no attribution: {unreadable:?}"
    );

    // When discovery reports the genuine new llama.cpp process (same PID,
    // new start time), the mapping is re-enabled for it.
    let new = ident(4242, 2000);
    let fresh = map_server_identity(new, Some(2000), evidence, Vec::new(), &gpus);
    assert!(
        matches!(fresh, GpuMapping::Single(_)),
        "a genuine new server must map again: {fresh:?}"
    );
}

#[test]
fn transient_llm_failure_keeps_last_sample_and_marks_reconnecting() {
    let now = Instant::now();
    let mut last_good = None;
    let good = LlmStats {
        connected: true,
        prompt_tps: Some(123.0),
        ..LlmStats::default()
    };
    let fresh = stabilize_llm_sample(good, &mut last_good, now, Duration::from_millis(2500));
    assert!(fresh.connected);
    assert!(!fresh.reconnecting);

    let failed = LlmStats {
        error: "cannot reach llama.cpp: timeout".to_string(),
        ..LlmStats::default()
    };
    let held = stabilize_llm_sample(
        failed,
        &mut last_good,
        now + Duration::from_millis(500),
        Duration::from_millis(2500),
    );
    assert!(held.connected);
    assert!(held.reconnecting);
    assert_eq!(held.prompt_tps, Some(123.0));
}

#[test]
fn held_llm_sample_expires_after_the_offline_grace() {
    // The held sample is bounded: once the grace window has passed, the
    // failed sample is reported as offline and no held value survives.
    let now = Instant::now();
    let grace = Duration::from_millis(2500);
    let mut last_good = None;
    let good = LlmStats {
        connected: true,
        generation_tps: Some(55.0),
        context_used: Some(1000),
        ..LlmStats::default()
    };
    stabilize_llm_sample(good, &mut last_good, now, grace);
    let failed = || LlmStats {
        error: "cannot reach llama.cpp: timeout".to_string(),
        ..LlmStats::default()
    };
    let held = stabilize_llm_sample(failed(), &mut last_good, now + grace, grace);
    assert!(held.connected && held.reconnecting);
    assert_eq!(held.generation_tps, Some(55.0));

    let expired = stabilize_llm_sample(
        failed(),
        &mut last_good,
        now + grace + Duration::from_millis(1),
        grace,
    );
    assert!(!expired.connected && !expired.reconnecting);
    assert_eq!(expired.generation_tps, None);
    assert_eq!(expired.context_used, None);
    assert_eq!(crate::llama::activity_phase(&expired), None);
}

#[test]
fn build_server_summaries_reflects_per_server_state() {
    let order = vec!["http://a:1/".to_string(), "http://b:2/".to_string()];
    let mut endpoints = BTreeMap::new();
    endpoints.insert("http://a:1/".to_string(), "http://a:1".to_string());
    endpoints.insert("http://b:2/".to_string(), "http://b:2".to_string());
    let mut registry = BTreeMap::new();
    registry.insert(
        "http://a:1/".to_string(),
        LlmStats {
            connected: true,
            model: "model-A".to_string(),
            slots_available: true,
            slot_count: 2,
            busy_slots: 1,
            context_used: Some(10),
            context_size: 100,
            ..Default::default()
        },
    );

    let summaries = build_server_summaries(
        &order,
        &endpoints,
        &registry,
        &UiState::default(),
        Instant::now(),
    );
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].label, "model-A");
    assert!(summaries[0].connected);
    assert!(summaries[0].slots_available);
    assert_eq!(summaries[0].slot_count, 2);
    assert_eq!(summaries[0].busy_slots, 1);
    assert_eq!(summaries[0].context_used, Some(10));
    // Server B has no telemetry yet: offline with no fabricated zeros.
    assert!(!summaries[1].connected);
    assert!(!summaries[1].slots_available);
    assert_eq!(summaries[1].slot_count, 0);
    assert_eq!(summaries[1].context_used, None);
}

#[test]
fn build_server_summaries_distinguishes_unavailable_slots_from_real_zero() {
    // The reported M4 bug: a connected server whose /slots endpoint is down
    // (but /metrics answers) must not collapse to "0/0" slots. Availability
    // is propagated explicitly and never confused with a real empty array.
    let order = vec![
        "http://metrics-only:1/".to_string(),
        "http://empty:2/".to_string(),
        "http://healthy:3/".to_string(),
        "http://dead:4/".to_string(),
    ];
    let mut endpoints = BTreeMap::new();
    for key in &order {
        endpoints.insert(key.clone(), key.trim_end_matches('/').to_string());
    }
    let mut registry = BTreeMap::new();
    // /metrics up, /slots down: connected, but the slot count is unknown.
    registry.insert(
        "http://metrics-only:1/".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: false,
            props_slot_count: 3,
            ..Default::default()
        },
    );
    // /slots answered a valid, explicitly empty array: this is a real zero.
    registry.insert(
        "http://empty:2/".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: true,
            slot_count: 0,
            busy_slots: 0,
            ..Default::default()
        },
    );
    registry.insert(
        "http://healthy:3/".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: true,
            slot_count: 4,
            busy_slots: 2,
            ..Default::default()
        },
    );
    // No telemetry at all (unreachable): also unavailable, never zero.
    registry.insert(
        "http://dead:4/".to_string(),
        LlmStats {
            connected: false,
            error: "connection refused".to_string(),
            ..Default::default()
        },
    );

    let summaries = build_server_summaries(
        &order,
        &endpoints,
        &registry,
        &UiState::default(),
        Instant::now(),
    );
    assert!(!summaries[0].slots_available, "metrics-only slots unknown");
    assert!(summaries[0].connected, "metrics-only server is connected");
    assert_eq!(
        summaries[0].props_slot_count, 3,
        "a known /props total is preserved even while /slots is down"
    );
    assert!(
        summaries[1].slots_available,
        "an explicit empty /slots array is a real zero"
    );
    assert_eq!(summaries[1].slot_count, 0);
    assert!(summaries[2].slots_available);
    assert_eq!((summaries[2].busy_slots, summaries[2].slot_count), (2, 4));
    assert!(!summaries[3].connected);
    assert!(!summaries[3].slots_available);
    // No cross-server contamination: each summary reflects only its own key.
    assert_ne!(summaries[2].slot_count, summaries[1].slot_count);
}

#[test]
fn build_server_summaries_keep_unevidenced_activity_unknown_and_match_the_panel() {
    use crate::domain::ActivityPhase;

    // Connected with /metrics only: no /slots, no request gauge and no
    // server-reported phase. The overview must say UNKNOWN (not IDLE), and
    // a held reconnecting sample must say RECONNECTING — in both cases the
    // same phase the selected-server panel derives (CFA-88).
    let order = vec!["bare".to_string(), "held".to_string(), "off".to_string()];
    let mut endpoints = BTreeMap::new();
    for key in &order {
        endpoints.insert(key.clone(), format!("http://{key}:8080"));
    }
    let bare = LlmStats {
        connected: true,
        metrics_available: true,
        ..Default::default()
    };
    let held = LlmStats {
        connected: true,
        reconnecting: true,
        metrics_available: true,
        slots_available: true,
        slot_count: 1,
        ..Default::default()
    };
    let off = LlmStats::default();
    let mut registry = BTreeMap::new();
    registry.insert("bare".to_string(), bare.clone());
    registry.insert("held".to_string(), held.clone());
    registry.insert("off".to_string(), off.clone());

    let summaries = build_server_summaries(
        &order,
        &endpoints,
        &registry,
        &UiState::default(),
        Instant::now(),
    );
    assert_eq!(summaries[0].phase, Some(ActivityPhase::Unknown));
    assert_eq!(summaries[1].phase, Some(ActivityPhase::Reconnecting));
    assert!(summaries[1].reconnecting && !summaries[1].connected);
    assert_eq!(summaries[2].phase, None);
    for (summary, stats) in summaries.iter().zip([&bare, &held, &off]) {
        assert_eq!(summary.phase, crate::llama::activity_phase(stats));
        let panel = crate::ui::llm_phase_label(stats);
        let overview = summary.phase.map_or("OFFLINE", ActivityPhase::label);
        assert_eq!(panel, overview, "panel and selector disagree");
    }
}

#[test]
fn build_server_summaries_derives_phase_age_and_backend() {
    use crate::domain::ActivityPhase;
    use crate::strata::StrataPhase;

    let order = vec![
        "gen".to_string(),
        "quiet".to_string(),
        "strata".to_string(),
        "offline_reg".to_string(),
        "off".to_string(),
    ];
    let mut endpoints = BTreeMap::new();
    for key in &order {
        endpoints.insert(key.clone(), format!("http://{key}:8080"));
    }
    let mut registry = BTreeMap::new();
    registry.insert(
        "gen".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            generation_tps: Some(20.0),
            backend: ServerBackend::LlamaCpp,
            ..Default::default()
        },
    );
    // IDLE needs evidence: here a valid /slots array with no busy slot.
    registry.insert(
        "quiet".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            slots_available: true,
            slot_count: 1,
            backend: ServerBackend::LlamaCpp,
            ..Default::default()
        },
    );
    registry.insert(
        "strata".to_string(),
        LlmStats {
            connected: true,
            metrics_available: true,
            strata_phase: Some(StrataPhase::Reading),
            backend: ServerBackend::Strata,
            ..Default::default()
        },
    );
    // A registry entry that is currently not answering: its phase must be
    // unknown (`None`), never a guessed `IDLE`.
    registry.insert(
        "offline_reg".to_string(),
        LlmStats {
            connected: false,
            ..Default::default()
        },
    );
    // "off" has no registry entry at all: never sampled.

    let mut ui_state = UiState::default();
    for key in ["gen", "quiet", "strata"] {
        ui_state.record_llm_sample(
            key,
            "gen",
            &LlmStats {
                connected: true,
                metrics_available: true,
                ..Default::default()
            },
        );
    }
    let now = Instant::now();
    let summaries = build_server_summaries(&order, &endpoints, &registry, &ui_state, now);

    assert_eq!(summaries[0].phase, Some(ActivityPhase::Generating));
    assert_eq!(summaries[1].phase, Some(ActivityPhase::Idle));
    assert_eq!(summaries[2].phase, Some(ActivityPhase::Prefill));
    assert_eq!(
        summaries[3].phase, None,
        "a not-answering registry entry stays unknown, not IDLE"
    );
    assert_eq!(
        summaries[4].phase, None,
        "never sampled stays unknown, not IDLE"
    );

    assert_eq!(summaries[0].backend, ServerBackend::LlamaCpp);
    assert_eq!(summaries[2].backend, ServerBackend::Strata);
    assert_eq!(summaries[4].backend, ServerBackend::Unknown);

    assert!(summaries[0].age_ms.is_some(), "sampled server has an age");
    assert!(
        summaries[3].age_ms.is_none(),
        "a not-answering server has no fresh age"
    );
    assert!(
        summaries[4].age_ms.is_none(),
        "a server that never answered has no age, never a fake 0"
    );
}

#[test]
fn rebuild_resets_monitor_on_a_same_endpoint_restart() {
    let spec = ServerSpec {
        key: "http://h:1/".to_string(),
        endpoint: "http://h:1".to_string(),
        identity: Some(LocalServerIdentity {
            pid: 100,
            start_time: 5,
        }),
    };
    let mut monitor = new_server_monitor(spec.clone());
    // Simulate a warm cache (a previous good sample).
    monitor.last_good = Some((LlmStats::default(), Instant::now()));

    // Same identity: the monitor and its per-server caches are preserved.
    let kept = rebuild_server_monitors(vec![monitor], vec![spec.clone()]);
    assert_eq!(kept.len(), 1);
    assert!(
        kept[0].last_good.is_some(),
        "an unchanged server keeps its state"
    );

    // Same endpoint, new process (new start time): the monitor is rebuilt,
    // dropping the stale caches instead of keeping them until the next
    // /props refresh.
    let restarted = ServerSpec {
        identity: Some(LocalServerIdentity {
            pid: 100,
            start_time: 6,
        }),
        ..spec
    };
    let reset = rebuild_server_monitors(kept, vec![restarted]);
    assert_eq!(reset.len(), 1);
    assert!(
        reset[0].last_good.is_none(),
        "a same-endpoint restart must reset the per-server caches"
    );
}

#[test]
fn changed_identity_keys_detects_only_restarted_servers() {
    let a = ServerSpec {
        key: "a".to_string(),
        endpoint: "http://a".to_string(),
        identity: Some(LocalServerIdentity {
            pid: 1,
            start_time: 1,
        }),
    };
    let b = ServerSpec {
        key: "b".to_string(),
        endpoint: "http://b".to_string(),
        identity: Some(LocalServerIdentity {
            pid: 2,
            start_time: 1,
        }),
    };
    // Only A restarted (same key, new start time).
    let a2 = ServerSpec {
        identity: Some(LocalServerIdentity {
            pid: 1,
            start_time: 2,
        }),
        ..a.clone()
    };
    assert_eq!(
        changed_identity_keys(&[a.clone(), b.clone()], &[a2, b.clone()]),
        vec!["a".to_string()]
    );
    // A new or removed key is not a restart.
    assert!(changed_identity_keys(std::slice::from_ref(&a), std::slice::from_ref(&b)).is_empty());
    assert!(changed_identity_keys(&[], std::slice::from_ref(&a)).is_empty());
}

#[test]
fn partial_metrics_payload_keeps_absent_metrics_unavailable_not_zero() {
    // Reachable /metrics with only a prompt counter and an explicit zero
    // request counter; every other metric must stay unavailable, not 0.
    let body = include_str!("../../../tests/fixtures/metrics_partial.prom");
    let (addr, stop, handle) = spawn_body_metrics_server(body);
    let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let stats = monitor.sample();
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();

    assert!(
        stats.metrics_available,
        "a partial payload is still available"
    );
    // Present metric -> real value; present zero -> real zero.
    assert_eq!(stats.prompt_total, Some(5000.0));
    assert_eq!(stats.active_requests, Some(0.0));
    // Absent metrics -> unavailable, never a fabricated zero.
    assert_eq!(stats.generated_total, None);
    assert_eq!(stats.prompt_seconds_total, None);
    assert_eq!(stats.prompt_avg_tps, None);
    assert_eq!(stats.generation_avg_tps, None);
    assert_eq!(stats.deferred_requests, None);
    assert_eq!(stats.context_high_watermark, None);
    assert_eq!(stats.spec_drafts_total, None);
    // No slot pairing and no aggregate counters -> live throughput —.
    assert_eq!((stats.prompt_tps, stats.generation_tps), (None, None));
}

#[test]
fn metrics_disabled_still_exposes_slot_state() {
    // Regression: a 501 on /metrics must not discard a healthy /slots
    // response. The server is reachable; the slot/context state stays
    // available while the metric-derived values are marked unavailable.
    let (addr, stop, handle) = spawn_metrics_off_server();
    let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let stats = monitor.sample();
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();

    assert!(stats.connected, "a reachable /slots server is connected");
    assert!(
        !stats.metrics_available,
        "a 501 /metrics must mark metrics unavailable"
    );
    assert!(
        stats.slots_available,
        "/slots succeeded and must stay visible"
    );
    assert_eq!(stats.slot_count, 1);
    assert_eq!(stats.context_size, 4096);
    assert_eq!(stats.slot_overview.len(), 1);
    assert_eq!(stats.slot_overview[0].id, 0);
    assert!(
        stats.error.contains("--metrics"),
        "the metrics-disabled reason must still be surfaced: {:?}",
        stats.error
    );
}

#[test]
fn empty_metrics_body_is_unavailable_not_a_fake_zero() {
    // A 2xx /metrics with no parseable sample must be treated as
    // unavailable, not as a real set of zeroes.
    let (addr, stop, handle) = spawn_empty_metrics_server();
    let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let stats = monitor.sample();
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();

    assert!(stats.connected, "/slots answered");
    assert!(
        !stats.metrics_available,
        "an empty /metrics body carries no metric data"
    );
    assert!(stats.slots_available);
    assert_eq!(stats.prompt_total, None);
    assert!(
        stats.error.contains("no metrics"),
        "the reason must be stated, got: {:?}",
        stats.error
    );
}

#[test]
fn metrics_counter_baseline_resets_across_an_outage() {
    // Regression: a metrics outage must not leave a stale counter baseline
    // that turns the first post-outage sample into an average over the
    // whole outage. /slots is 501 throughout so the metric path decides.
    let (addr, stop, handle) = spawn_flaky_metrics_server(2);
    let mut monitor = crate::llama::LlamaMonitor::new(&format!("http://{addr}")).unwrap();

    let first = monitor.sample();
    assert!(first.metrics_available);
    // First sample establishes the baseline: no fabricated rate yet.
    assert_eq!((first.prompt_tps, first.generation_tps), (None, None));

    let outage = monitor.sample();
    assert!(!outage.metrics_available, "second /metrics request is 501");
    assert!(
        !outage.connected,
        "with /metrics and /slots both down the server is unreachable"
    );

    thread::sleep(Duration::from_millis(5));

    let recovered = monitor.sample();
    assert!(recovered.metrics_available);
    assert_eq!(
        (recovered.prompt_tps, recovered.generation_tps),
        (None, None),
        "the first sample after an outage must not average the delta over the outage"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

fn mapped(bdf: &str) -> crate::domain::MappedGpu {
    crate::domain::MappedGpu {
        device: crate::domain::DeviceId::new(Some(bdf.to_string()), None),
        name: "card0".to_string(),
        vendor: crate::domain::GpuVendor::Amd,
        evidence: crate::domain::GpuEvidence::RenderNodeFd,
    }
}

fn discovered_gpu(bdf: &str) -> crate::discovery::DiscoveredGpu {
    crate::discovery::DiscoveredGpu {
        card: "card0".to_string(),
        device_id: crate::domain::DeviceId::new(Some(bdf.to_string()), None),
        vendor: crate::domain::GpuVendor::Amd,
        pci_vendor_id: 0x1002,
        pci_device_id: 0x74a0,
        pci_class_code: 0x030000,
        driver: "amdgpu".to_string(),
        render_nodes: vec!["renderD128".to_string()],
        outputs: Vec::new(),
    }
}
