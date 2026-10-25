//! `LlamaMonitor` state-machine tests against a mock HTTP backend.
use super::super::*;
use serde_json::json;

#[test]
fn props_failure_retries_next_sample() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    // A failed /props fetch must not extend the 30 s refresh window, so
    // the next sample retries while the cached props keep being shown.
    assert!(monitor.props_needs_refresh());
    monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
    assert!(monitor.props_dirty);
    assert!(monitor.props_needs_refresh());

    // A successful fetch clears the dirty flag and populates the cache.
    let props = json!({
        "model_name": "/models/Qwen3-4B-Q4_K_M.gguf",
        "total_slots": 2,
        "default_generation_settings": { "n_ctx": 4096 }
    });
    monitor.apply_props_result(JsonOutcome::Ok(props));
    assert!(!monitor.props_dirty);
    assert_eq!(monitor.props.model, "Qwen3-4B-Q4_K_M");
    assert_eq!(monitor.props.context_size, 4096);
    assert_eq!(monitor.props.total_slots, 2);
}

#[test]
fn local_mtp_is_shown_when_the_server_only_reports_types_none() {
    // Live shape on test-desktop: /props carries only
    // `speculative.types: "none"`, the CLI is launched with draft-mtp/8.
    // The server did not name a type, so the CLI fills it in.
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "default_generation_settings": {"params": {"speculative.types": "none"}}
    })));
    let mut stats = LlmStats {
        spec_enabled: monitor.props.spec_enabled,
        spec_is_mtp: monitor.props.spec_is_mtp,
        spec_n_max: monitor.props.spec_n_max,
        ..Default::default()
    };
    apply_local_spec_fallback(
        &monitor.props,
        &mut stats,
        LocalSpeculativeConfig {
            enabled: true,
            is_mtp: true,
            n_max: Some(8),
        },
    );
    assert!(stats.spec_enabled);
    assert!(stats.spec_is_mtp, "CLI draft-mtp must surface");
    assert_eq!(stats.spec_n_max, Some(8));
}

#[test]
fn named_mtp_type_implies_enabled_from_the_server_alone() {
    // A server that names draft-mtp but omits the `speculative` boolean
    // still means speculative decoding is on.
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative.types": "draft-mtp"
    })));
    assert!(monitor.props.spec_enabled);
    assert!(monitor.props.spec_is_mtp);
}

#[test]
fn nested_type_none_takes_precedence_over_root_mtp() {
    // Documented precedence: the nested default_generation_settings wins
    // per field, so a nested `types: "none"` overrides a root draft-mtp.
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative.types": "draft-mtp",
        "default_generation_settings": {"params": {"speculative.types": "none"}}
    })));
    assert!(!monitor.props.spec_is_mtp);
    assert!(!monitor.props.spec_named_type);
}

#[test]
fn standalone_n_max_zero_suppresses_local_mtp() {
    // An explicit server `speculative.n_max: 0` (no `speculative` boolean)
    // disables speculation: the CLI MTP must not override it.
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "params": {"speculative.n_max": 0}
    })));
    let mut stats = LlmStats {
        spec_enabled: monitor.props.spec_enabled,
        spec_is_mtp: monitor.props.spec_is_mtp,
        spec_n_max: monitor.props.spec_n_max,
        ..Default::default()
    };
    apply_local_spec_fallback(
        &monitor.props,
        &mut stats,
        LocalSpeculativeConfig {
            enabled: true,
            is_mtp: true,
            n_max: Some(8),
        },
    );
    assert!(!stats.spec_enabled);
    assert!(!stats.spec_is_mtp);
    assert_eq!(stats.spec_n_max, None);
}

#[test]
fn local_mtp_does_not_override_an_explicit_server_disable() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": false,
        "speculative.types": ""
    })));
    let mut stats = LlmStats {
        spec_enabled: monitor.props.spec_enabled,
        spec_is_mtp: monitor.props.spec_is_mtp,
        spec_n_max: monitor.props.spec_n_max,
        ..Default::default()
    };
    apply_local_spec_fallback(
        &monitor.props,
        &mut stats,
        LocalSpeculativeConfig {
            enabled: true,
            is_mtp: true,
            n_max: Some(8),
        },
    );
    assert!(!stats.spec_enabled);
    assert!(!stats.spec_is_mtp);
    assert_eq!(stats.spec_n_max, None);
}

#[test]
fn local_mtp_does_not_override_a_server_named_non_mtp_type() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "speculative.types": "ngram"
    })));
    let mut stats = LlmStats {
        spec_enabled: monitor.props.spec_enabled,
        spec_is_mtp: monitor.props.spec_is_mtp,
        spec_n_max: monitor.props.spec_n_max,
        ..Default::default()
    };
    apply_local_spec_fallback(
        &monitor.props,
        &mut stats,
        LocalSpeculativeConfig {
            enabled: true,
            is_mtp: true,
            n_max: Some(8),
        },
    );
    assert!(stats.spec_enabled);
    assert!(!stats.spec_is_mtp, "server named a non-MTP type");
}

#[test]
fn successful_props_response_replaces_spec_state_instead_of_accumulating() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "model_name": "model-A",
        "speculative": true,
        "speculative.n_max": 3,
        "speculative.types": "draft-mtp"
    })));
    assert!(monitor.props.spec_enabled);
    assert!(monitor.props.spec_is_mtp);
    assert_eq!(monitor.props.spec_n_max, Some(3));

    // Explicit disable on the same endpoint (new model / restarted server):
    // the previous active state must be removed, not OR-ed into.
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "model_name": "model-B",
        "speculative": false,
        "speculative.n_max": 0,
        "speculative.types": ""
    })));
    assert!(!monitor.props.spec_enabled, "stale enabled flag");
    assert!(!monitor.props.spec_is_mtp, "stale mtp flag");
    assert_eq!(monitor.props.spec_n_max, None, "stale n_max");
    assert_eq!(monitor.props.model, "model-B");
}

#[test]
fn successful_props_response_adopts_a_changed_n_max() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "params": {"speculative.n_max": 3}
    })));
    assert_eq!(monitor.props.spec_n_max, Some(3));
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "params": {"speculative.n_max": 8}
    })));
    assert_eq!(monitor.props.spec_n_max, Some(8));
}

#[test]
fn props_without_spec_info_keeps_the_cached_state() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "speculative.n_max": 4,
        "speculative.types": "draft-mtp"
    })));
    // An older response that says nothing about spec must not erase it...
    monitor.apply_props_result(JsonOutcome::Ok(json!({"model_name": "model-C"})));
    assert!(monitor.props.spec_enabled);
    assert!(monitor.props.spec_is_mtp);
    assert_eq!(monitor.props.spec_n_max, Some(4));

    // ...and neither must a failed refresh.
    monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
    assert!(monitor.props.spec_enabled);
    assert_eq!(monitor.props.spec_n_max, Some(4));
}

#[test]
fn default_generation_settings_takes_precedence_over_root() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "speculative.n_max": 3,
        "default_generation_settings": {
            "speculative": false,
            "speculative.n_max": 0
        }
    })));
    assert!(!monitor.props.spec_enabled, "nested disable must win");
    assert_eq!(monitor.props.spec_n_max, None);

    // Root fields fill what the nested variant does not mention.
    monitor.apply_props_result(JsonOutcome::Ok(json!({
        "speculative": true,
        "speculative.n_max": 6,
        "default_generation_settings": {"n_ctx": 4096}
    })));
    assert!(monitor.props.spec_enabled);
    assert_eq!(monitor.props.spec_n_max, Some(6));
}

#[test]
fn local_spec_config_is_cached_within_the_refresh_window() {
    // A non-loopback endpoint never triggers the /proc scan: the result
    // is None and the window starts on the first call. A second call
    // within the window returns the cached None without rescanning
    // (the timestamp is unchanged, proving no new scan ran).
    let mut monitor = LlamaMonitor::new("http://10.0.0.7:9090").unwrap();
    assert!(monitor.local_spec_config().is_none());
    let first = monitor.local_spec_scanned_at.expect("scan timestamp set");
    assert!(monitor.local_spec_config().is_none());
    assert_eq!(
        monitor.local_spec_scanned_at,
        Some(first),
        "cached value, no rescan"
    );
}

#[test]
fn slots_outcome_maps_to_error_or_counters() {
    let monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    // A failed /slots fetch degrades gracefully: it sets slots_error and
    // does not touch the previous-slot counters or mark the slot set
    // unavailable.
    let mut stats = LlmStats::default();
    let slots = monitor.apply_slots_outcome(&mut stats, JsonOutcome::Http(StatusCode::NOT_FOUND));
    assert!(slots.is_none());
    assert!(!stats.slots_available);
    assert_eq!(stats.slots_error, "/slots returned HTTP 404 Not Found");

    let mut stats = LlmStats::default();
    let slots = monitor.apply_slots_outcome(
        &mut stats,
        JsonOutcome::Unreachable("connection refused".into()),
    );
    assert!(slots.is_none());
    assert!(!stats.slots_available);
    assert_eq!(stats.slots_error, "cannot reach /slots: connection refused");

    // A valid /slots payload populates the counters and clears any error.
    let value = json!([{ "id": 0, "task_id": 1, "state": "busy" }]);
    let mut stats = LlmStats::default();
    let slots = monitor.apply_slots_outcome(&mut stats, JsonOutcome::Ok(value));
    assert!(slots.is_some());
    assert!(stats.slots_available);
    assert_eq!(stats.slot_count, 1);
    assert!(stats.slots_error.is_empty());
}

#[test]
fn sample_reads_strata_json_metrics_end_to_end() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let metrics_calls = Arc::new(AtomicU32::new(0));
    let calls = Arc::clone(&metrics_calls);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    let body = if path.contains("/props") {
                        r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_alias":"strata-model"}"#.to_string()
                    } else if path.contains("/slots") {
                        r#"[{"id":0,"n_ctx":4096,"is_processing":false}]"#.to_string()
                    } else if path.contains("/metrics") {
                        let n = u64::from(calls.fetch_add(1, Ordering::SeqCst) + 1);
                        // First sample reports an active decode phase; the
                        // second is idle. The phase gates whether a live
                        // decode rate is surfaced at all.
                        let (state, tok_s) = if n == 1 {
                            ("generating", "42.0")
                        } else {
                            ("idle", "null")
                        };
                        format!(
                            r#"{{"engine":{{"model":"strata"}},"live":{{"state":"{state}","queued":0,"tok_s":{tok_s}}},"requests_kept":1,"totals":{{"since":1.0,"prompt_tokens":{},"output_tokens":{},"prompt_ms":1000.0,"decode_ms":500.0,"drafts_offered":40,"drafts_accepted":30}}}}"#,
                            1000 * n,
                            250 * n
                        )
                    } else {
                        "{}".to_string()
                    };
                    let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let first = monitor.sample();
    assert!(first.connected, "strata server must be connected");
    assert!(
        first.metrics_available,
        "strata JSON /metrics must be available, error={:?}",
        first.error
    );
    assert_eq!(first.error, "", "no metrics-off error expected");
    assert_eq!(first.model, "strata-model");
    assert_eq!(
        first.backend,
        crate::domain::ServerBackend::Strata,
        "a recognised Strata body must be identified as Strata"
    );
    assert_eq!(first.prompt_total, Some(1000.0));
    assert_eq!(first.generated_total, Some(250.0));
    // Lifetime averages from totals + ms-derived seconds.
    assert_eq!(first.prompt_avg_tps, Some(1000.0));
    assert_eq!(first.generation_avg_tps, Some(500.0));
    // The live decode rate is Strata's authoritative windowed value, not a
    // counter delta; Strata has no instantaneous prefill rate.
    assert_eq!(first.generation_tps, Some(42.0));
    assert_eq!(first.prompt_tps, None);
    assert!(first.slots_available);
    assert_eq!(first.slot_count, 1);

    // Idle: no stale decode speed is carried over.
    let second = monitor.sample();
    assert!(second.metrics_available);
    assert_eq!(second.prompt_total, Some(2000.0));
    assert_eq!(second.generation_tps, None);
    assert_eq!(second.prompt_tps, None);

    stop.store(true, Ordering::Relaxed);
}

#[test]
fn a_wire_format_switch_resets_the_counter_baseline() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let calls = Arc::new(AtomicU32::new(0));
    let calls_flag = Arc::clone(&calls);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    // /slots uses the exact Strata shape (no counters), so
                    // the slot path must abstain and the counter baseline
                    // (not a fabricated slot zero) is the only live-rate
                    // source under test.
                    let response = if path.contains("/props") {
                        let body = r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_alias":"m"}"#;
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                    } else if path.contains("/slots") {
                        let body = r#"[{"id":0,"n_ctx":4096,"is_processing":false}]"#;
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                    } else if path.contains("/metrics") {
                        let n = calls_flag.fetch_add(1, Ordering::SeqCst);
                        let body = if n == 0 {
                            // First: a Strata sample with tiny counters.
                            r#"{"engine":{},"live":{"state":"idle"},"requests_kept":1,"totals":{"since":1.0,"prompt_tokens":5,"output_tokens":5,"prompt_ms":1000.0,"decode_ms":1000.0}}"#.to_string()
                        } else {
                            // Then: llama.cpp Prometheus with much larger counters.
                            "# fixture\nllamacpp:prompt_tokens_total 100000\nllamacpp:tokens_predicted_total 100000\nllamacpp:prompt_seconds_total 10\nllamacpp:tokens_predicted_seconds_total 10\n".to_string()
                        };
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                            .to_string()
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let strata = monitor.sample();
    assert!(strata.metrics_available);
    assert!(strata.slots_available);
    assert_eq!(strata.prompt_total, Some(5.0));
    assert_eq!(strata.prompt_tps, None);
    assert_eq!(strata.generation_tps, None);

    // Switching to Prometheus must not diff the llama counter against the
    // Strata baseline (which would read as a huge spike). The counter-less
    // /slots abstains, so a `None` proves the counter baseline was reset; a
    // leaked baseline would produce `Some(≈100000/seconds)`.
    let prometheus = monitor.sample();
    assert!(prometheus.metrics_available);
    assert_eq!(prometheus.prompt_total, Some(100000.0));
    assert_eq!(prometheus.generation_avg_tps, Some(10000.0));
    assert_eq!(
        prometheus.prompt_tps, None,
        "baseline must reset across a wire-format switch"
    );
    assert_eq!(
        prometheus.generation_tps, None,
        "baseline must reset across a wire-format switch"
    );

    stop.store(true, Ordering::Relaxed);
}

#[test]
fn sample_keeps_strata_context_occupancy_unknown_but_tracks_progress() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    // /props has no n_ctx and /slots is Strata 0.1.40-shaped
                    // (no counters, no occupancy); /metrics carries the
                    // engine capacity and live active-request progress.
                    let body = if path.contains("/props") {
                        r#"{"total_slots":1,"model_alias":"strata-model"}"#.to_string()
                    } else if path.contains("/slots") {
                        r#"[{"id":0,"is_processing":true}]"#.to_string()
                    } else if path.contains("/metrics") {
                        r#"{"engine":{"max_context":196608},"live":{"state":"generating","prompt_tokens":500,"generated":20,"tok_s":42.0},"requests_kept":1,"totals":{"since":1.0,"prompt_tokens":5,"output_tokens":5,"prompt_ms":1000.0,"decode_ms":1000.0}}"#.to_string()
                    } else {
                        "{}".to_string()
                    };
                    let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
    let stats = monitor.sample();
    assert!(stats.connected && stats.metrics_available);
    // Capacity comes from the Strata engine when /props omitted n_ctx.
    assert_eq!(stats.context_size, 196608);
    // Occupancy is not fabricated from the active request length.
    assert_eq!(stats.context_used, None);
    // Active request progress is surfaced from the already-fetched /metrics.
    assert_eq!(stats.request_prompt_tokens, Some(500));
    assert_eq!(stats.request_generated_tokens, Some(20));
    assert_eq!(stats.generation_tps, Some(42.0));

    stop.store(true, Ordering::Relaxed);
}

#[test]
fn metric_counter_absence_does_not_poison_the_baseline() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
    let mut stats = LlmStats {
        prompt_total: Some(1000.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);

    // Partial payload: the prompt counter is absent this sample.
    let mut stats = LlmStats {
        prompt_total: None,
        ..Default::default()
    };
    let live = monitor.update_metric_counters(&mut stats);
    assert_eq!(live.0, None, "absent counter -> unavailable rate");

    // It reappears: the absent sample must not be stored as a `0` baseline,
    // so this establishes a fresh baseline instead of a huge spike.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut stats = LlmStats {
        prompt_total: Some(1100.0),
        ..Default::default()
    };
    let live = monitor.update_metric_counters(&mut stats);
    assert_eq!(live.0, None, "reappearance must re-baseline, never spike");
}

#[test]
fn sample_uses_strata_live_prompt_not_new_slots_occupancy_for_request() {
    // Newer Strata: /slots carries only the occupancy (`n_prompt_tokens`),
    // /metrics `live` carries the active request's prompt length. REQUEST
    // prompt must come from the request field, CTX from the occupancy —
    // the two must never collapse into the same figure.
    let backend = spawn_backend(|path, _n| {
        let body = if path.contains("/props") {
            r#"{"total_slots":1,"model_alias":"strata-model"}"#.to_string()
        } else if path.contains("/slots") {
            include_str!("../../../tests/fixtures/slots_strata_new.json").to_string()
        } else if path.contains("/metrics") {
            r#"{"engine":{"max_context":196608},"live":{"state":"reading","prompt_tokens":2048,"generated":0},"requests_kept":1,"totals":{"since":1.0,"prompt_tokens":5,"output_tokens":5,"prompt_ms":1000.0,"decode_ms":1000.0}}"#.to_string()
        } else {
            "{}".to_string()
        };
        (200, "application/json", body)
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
    let stats = monitor.sample();
    assert!(stats.connected && stats.metrics_available);
    assert_eq!(stats.context_used, Some(113274));
    assert_eq!(stats.request_prompt_tokens, Some(2048));
    assert_eq!(stats.request_generated_tokens, Some(0));
}

#[test]
fn spec_acceptance_computes_from_deltas_and_holds_after_quiet() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    let mut stats = LlmStats {
        spec_draft_tokens: Some(200.0),
        spec_accepted_tokens: Some(140.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);

    // Second sample: 100 new draft tokens, 60 accepted -> 60%.
    let mut stats = LlmStats {
        spec_draft_tokens: Some(300.0),
        spec_accepted_tokens: Some(200.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, Some(60.0));

    // Third sample with no new draft tokens: the last value is held for
    // SPEC_ACCEPTANCE_HOLD instead of dropping to None.
    let mut stats = LlmStats {
        spec_draft_tokens: Some(300.0),
        spec_accepted_tokens: Some(200.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, Some(60.0));
}

#[test]
fn spec_acceptance_expires_after_the_hold_window() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    let mut stats = LlmStats {
        spec_draft_tokens: Some(100.0),
        spec_accepted_tokens: Some(50.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    let mut stats = LlmStats {
        spec_draft_tokens: Some(200.0),
        spec_accepted_tokens: Some(100.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, Some(50.0));

    let mut stats = LlmStats {
        spec_draft_tokens: Some(200.0),
        spec_accepted_tokens: Some(100.0),
        ..Default::default()
    };
    let expired = Instant::now() - (SPEC_ACCEPTANCE_HOLD + Duration::from_secs(1));
    monitor.last_spec_acceptance_at = Some(expired);
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, None);
}

#[test]
fn spec_acceptance_clamps_to_hundred_percent() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    let mut stats = LlmStats {
        spec_draft_tokens: Some(100.0),
        spec_accepted_tokens: Some(100.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);

    // Accepted delta larger than the draft delta must clamp, not exceed.
    let mut stats = LlmStats {
        spec_draft_tokens: Some(200.0),
        spec_accepted_tokens: Some(250.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, Some(100.0));
}

#[test]
fn spec_counter_reset_does_not_create_fake_acceptance() {
    let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

    let mut stats = LlmStats {
        spec_draft_tokens: Some(1000.0),
        spec_accepted_tokens: Some(900.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);

    // Server restart: counters reset below the previous values. No draft
    // progress and no fresh hold -> no acceptance value, no NaN.
    let mut stats = LlmStats {
        spec_draft_tokens: Some(10.0),
        spec_accepted_tokens: Some(5.0),
        ..Default::default()
    };
    monitor.update_metric_counters(&mut stats);
    assert_eq!(stats.spec_acceptance_pct, None);
}

struct MockBackend {
    base: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MockBackend {
    fn base(&self) -> &str {
        &self.base
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn spawn_backend<F>(respond: F) -> MockBackend
where
    F: Fn(&str, usize) -> (u16, &'static str, String) + Send + 'static,
{
    use std::collections::HashMap;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock backend");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let calls: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stop_flag.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    let index = {
                        let mut calls = calls.lock().expect("mock call counter");
                        let entry = calls.entry(path.clone()).or_insert(0);
                        let index = *entry;
                        *entry += 1;
                        index
                    };
                    let (status, content_type, body) = respond(&path, index);
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        501 => "Not Implemented",
                        _ => "Error",
                    };
                    let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    MockBackend {
        base: format!("http://{addr}"),
        stop,
    }
}

#[test]
fn missing_slots_and_activity_fields_leave_the_phase_unknown() {
    // CFA-88: /slots 404 and a /metrics body without
    // `requests_processing`: nothing proves the server is idle, so the
    // phase is UNKNOWN. Once the gauge reports a real 0 it is IDLE.
    use crate::domain::ActivityPhase;
    let backend = spawn_backend(|path, n| {
        if path.contains("/slots") {
            return (404, "text/plain", "not found".to_string());
        }
        if path.contains("/metrics") {
            let mut body = "llamacpp:prompt_tokens_total 10\n".to_string();
            if n > 0 {
                body.push_str("llamacpp:requests_processing 0\n");
            }
            return (200, "text/plain", body);
        }
        (200, "application/json", r#"{"total_slots":1}"#.to_string())
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
    let first = monitor.sample();
    assert!(first.connected && first.metrics_available && !first.slots_available);
    assert_eq!(activity_phase(&first), Some(ActivityPhase::Unknown));
    let second = monitor.sample();
    assert_eq!(second.active_requests, Some(0.0));
    assert_eq!(activity_phase(&second), Some(ActivityPhase::Idle));
}

#[test]
fn a_missing_props_endpoint_leaves_the_model_unknown_but_keeps_telemetry() {
    // /props 404 while /metrics and /slots answer: the server is still
    // reachable, so metric and slot telemetry must stay available and the
    // model stays unknown rather than guessed.
    let backend = spawn_backend(|path, _n| {
        if path.contains("/props") {
            (
                404,
                "application/json",
                "{\"error\":\"not found\"}".to_string(),
            )
        } else if path.contains("/metrics") {
            (
                200,
                "text/plain",
                include_str!("../../../tests/fixtures/metrics_current.prom").to_string(),
            )
        } else if path.contains("/slots") {
            (
                200,
                "application/json",
                include_str!("../../../tests/fixtures/slots_idle.json").to_string(),
            )
        } else {
            (404, "text/plain", String::new())
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
    let stats = monitor.sample();

    assert!(stats.connected);
    assert!(stats.metrics_available);
    assert!(stats.slots_available);
    assert_eq!(stats.prompt_total, Some(12000.0));
    assert_eq!(stats.model, UNKNOWN_MODEL_LABEL, "no props -> no model");
    // Capacity still comes from the /slots payload, not from a guess.
    assert_eq!(stats.context_size, 196608);
    assert_eq!(stats.slot_count, 1);
}

#[test]
fn a_missing_slots_endpoint_keeps_metrics_visible() {
    // /slots 404 while /metrics answers: metric-derived values stay real,
    // the slot-derived values stay unavailable (never a fabricated zero).
    let backend = spawn_backend(|path, _n| {
        if path.contains("/props") {
            (
                200,
                "application/json",
                include_str!("../../../tests/fixtures/props.json").to_string(),
            )
        } else if path.contains("/metrics") {
            (
                200,
                "text/plain",
                include_str!("../../../tests/fixtures/metrics_current.prom").to_string(),
            )
        } else {
            (404, "application/json", "[]".to_string())
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
    let stats = monitor.sample();

    assert!(stats.connected);
    assert!(stats.metrics_available);
    assert!(!stats.slots_available);
    assert_eq!(stats.prompt_total, Some(12000.0));
    assert!(
        stats.slots_error.contains("404"),
        "the /slots failure must be surfaced: {:?}",
        stats.slots_error
    );
    assert_eq!(stats.model, "Qwen3.8-27B-UD-Q4_K_M");
    // Context size is seeded by /props; occupancy stays unknown.
    assert_eq!(stats.context_size, 196608);
    assert_eq!(stats.context_used, None);
    assert_eq!(stats.slot_count, 0);
}

#[test]
fn unrecognized_metrics_json_goes_unavailable_and_recovers_without_a_spike() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Sequence: valid llama.cpp -> unrecognised JSON -> valid llama.cpp
    // with a larger counter. The invalid sample must clear availability and
    // drop the counter baseline so recovery cannot report a delta averaged
    // across the gap.
    let metrics_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&metrics_calls);
    let backend = spawn_backend(move |path, _n| {
        if path.contains("/metrics") {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            if index == 1 {
                (
                    200,
                    "application/json",
                    "{\"unexpected\": true}".to_string(),
                )
            } else {
                (
                    200,
                    "text/plain",
                    format!(
                        "llamacpp:prompt_tokens_total {}\n",
                        12000 + 50 * index as u64
                    ),
                )
            }
        } else if path.contains("/slots") {
            (
                200,
                "application/json",
                "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
            )
        } else {
            (
                200,
                "application/json",
                "{\"model_name\":\"m\",\"total_slots\":1}".to_string(),
            )
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

    let first = monitor.sample();
    assert!(first.metrics_available);
    assert_eq!(first.prompt_total, Some(12000.0));
    assert_eq!(
        first.prompt_tps, None,
        "first sample only sets the baseline"
    );

    thread::sleep(Duration::from_millis(2));

    let invalid = monitor.sample();
    assert!(!invalid.metrics_available);
    assert_eq!(invalid.prompt_total, None, "no fabricated zero");
    assert_eq!(invalid.prompt_tps, None);
    assert_eq!(invalid.backend, crate::domain::ServerBackend::Unknown);
    assert!(
        invalid.error.contains("unrecognized JSON"),
        "the reason must be format-specific: {:?}",
        invalid.error
    );

    thread::sleep(Duration::from_millis(2));

    let recovered = monitor.sample();
    assert!(recovered.metrics_available);
    assert_eq!(recovered.prompt_total, Some(12100.0));
    assert_eq!(
        recovered.prompt_tps, None,
        "the baseline was dropped during the invalid sample; recovery re-baselines"
    );
}

#[test]
fn a_prometheus_counter_reset_after_a_restart_is_unavailable_then_recovers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // A restarted llama.cpp restarts every cumulative counter at zero. That
    // is a reset, not a negative rate: the live value must be unavailable,
    // and the renewed climb establishes a fresh baseline.
    let metrics_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&metrics_calls);
    let backend = spawn_backend(move |path, _n| {
        if path.contains("/metrics") {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let body = match index {
                    0 => "llamacpp:prompt_tokens_total 12000\nllamacpp:prompt_seconds_total 12\nllamacpp:tokens_predicted_total 800\nllamacpp:tokens_predicted_seconds_total 20\n".to_string(),
                    1 => "llamacpp:prompt_tokens_total 0\nllamacpp:tokens_predicted_total 0\n".to_string(),
                    _ => "llamacpp:prompt_tokens_total 200\nllamacpp:tokens_predicted_total 50\n".to_string(),
                };
            (200, "text/plain", body)
        } else if path.contains("/slots") {
            // Counter-less /slots: the aggregate /metrics delta is the only
            // live-rate source under test.
            (
                200,
                "application/json",
                "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
            )
        } else {
            (
                200,
                "application/json",
                "{\"model_name\":\"m\",\"total_slots\":1}".to_string(),
            )
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

    let first = monitor.sample();
    assert!(first.metrics_available);
    assert_eq!(first.prompt_total, Some(12000.0));
    // Lifetime averages are unaffected by the reset semantics.
    assert_eq!(first.prompt_avg_tps, Some(1000.0));
    assert_eq!(first.generation_avg_tps, Some(40.0));
    assert_eq!(first.prompt_tps, None);

    thread::sleep(Duration::from_millis(2));

    let restarted = monitor.sample();
    assert_eq!(restarted.prompt_total, Some(0.0));
    assert_eq!(restarted.prompt_tps, None, "a counter reset is not a rate");
    assert_eq!(restarted.generation_tps, None);

    thread::sleep(Duration::from_millis(2));

    let recovered = monitor.sample();
    assert_eq!(recovered.prompt_total, Some(200.0));
    assert!(
        recovered.prompt_tps.is_some_and(|rate| rate > 0.0),
        "a counter climbing from the new baseline is a real rate: {:?}",
        recovered.prompt_tps
    );
}

#[test]
fn legacy_llama_metric_names_are_accepted_as_fallbacks() {
    let backend = spawn_backend(|path, _n| {
        if path.contains("/metrics") {
            (
                200,
                "text/plain",
                include_str!("../../../tests/fixtures/metrics_legacy_names.prom").to_string(),
            )
        } else if path.contains("/slots") {
            (
                200,
                "application/json",
                "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
            )
        } else {
            (
                200,
                "application/json",
                "{\"model_name\":\"legacy\",\"total_slots\":1}".to_string(),
            )
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
    let stats = monitor.sample();

    assert!(stats.metrics_available);
    assert_eq!(
        stats.prompt_total,
        Some(1000.0),
        "llamacpp:tokens_evaluated_total is a legacy prompt name"
    );
    assert_eq!(
        stats.generated_total,
        Some(250.0),
        "predicted_tokens_total is a legacy generation name"
    );
    // The legacy gauges are only a fallback average source when no seconds
    // counter is present.
    assert_eq!(stats.prompt_avg_tps, Some(0.0));
    assert_eq!(stats.generation_avg_tps, Some(0.0));
}

#[test]
fn an_invalid_props_refresh_keeps_the_cached_model() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let props_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&props_calls);
    let backend = spawn_backend(move |path, _n| {
        if path.contains("/metrics") {
            (
                200,
                "text/plain",
                "llamacpp:prompt_tokens_total 10\n".to_string(),
            )
        } else if path.contains("/slots") {
            (200, "application/json", "[]".to_string())
        } else {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                (
                    200,
                    "application/json",
                    "{\"model_name\":\"model-A\",\"default_generation_settings\":{\"n_ctx\":4096}}"
                        .to_string(),
                )
            } else {
                (200, "application/json", "this is not json".to_string())
            }
        }
    });
    let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

    let first = monitor.sample();
    assert_eq!(first.model, "model-A");
    assert_eq!(first.context_size, 4096);

    // Force a /props refresh; the server now returns a malformed body.
    monitor.props_dirty = true;
    thread::sleep(Duration::from_millis(2));
    let second = monitor.sample();

    assert_eq!(
        second.model, "model-A",
        "a malformed refresh must not erase the cache"
    );
    assert_eq!(second.context_size, 4096);
    // The refresh stays pending so the next sample retries.
    assert!(monitor.props_dirty);
}
