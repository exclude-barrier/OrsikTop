//! Parser, counter and slot-math tests for the llama telemetry layer.
use super::super::*;
use serde_json::json;

#[test]
fn model_display_name_strips_control_characters_and_bounds_length() {
    assert_eq!(model_display_name("/models/qwen2.5-7b.gguf"), "qwen2.5-7b");
    // ANSI/CR/LF/tab are neutralized (control run collapses to a space).
    let cleaned = model_display_name("evil\x1b[31m\nname\t.gguf");
    assert_eq!(cleaned, "evil [31m name");
    assert!(!cleaned.contains('\n') && !cleaned.contains('\x1b'));
    // Excessively long names are bounded with an ellipsis.
    let long = "x".repeat(500);
    let bounded = model_display_name(&long);
    assert_eq!(bounded.chars().count(), MAX_MODEL_DISPLAY_CHARS + 1);
}

#[test]
fn endpoint_url_appends_to_the_path_and_keeps_the_query() {
    assert_eq!(
        endpoint_url("http://127.0.0.1:8080", "metrics"),
        "http://127.0.0.1:8080/metrics"
    );
    assert_eq!(
        endpoint_url("http://127.0.0.1:8080/", "slots"),
        "http://127.0.0.1:8080/slots"
    );
    // A configured base path is preserved.
    assert_eq!(
        endpoint_url("http://h:8080/v1", "props"),
        "http://h:8080/v1/props"
    );
    // A query token stays a query; it must not absorb the segment.
    assert_eq!(
        endpoint_url("http://h:8080/v1?token=abc", "metrics"),
        "http://h:8080/v1/metrics?token=abc"
    );
}

#[test]
fn a_panicking_fetch_task_is_turned_into_an_outcome() {
    // Regression: join().unwrap() used to propagate a fetch panic and kill
    // the LLM worker permanently. The result must become the fallback
    // error outcome instead, so sampling continues.
    let outcome: MetricsOutcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| -> MetricsOutcome { panic!("simulated fetch panic") });
        join_or(handle, MetricsOutcome::Unreachable("fallback".to_string()))
    });
    assert!(matches!(outcome, MetricsOutcome::Unreachable(message) if message == "fallback"));
}

#[test]
fn speculative_type_list_distinguishes_none_from_a_named_type() {
    let none = speculative_config(&json!({"speculative.types": "none"}));
    assert!(none.types_known);
    assert!(!none.named_type, "\"none\" names no type");
    assert!(!none.is_mtp);

    let mtp = speculative_config(&json!({"speculative.types": "draft-mtp"}));
    assert!(mtp.named_type);
    assert!(mtp.is_mtp);

    let other = speculative_config(&json!({"speculative.types": "ngram"}));
    assert!(other.named_type);
    assert!(!other.is_mtp);

    let absent = speculative_config(&json!({"n_ctx": 4096}));
    assert!(!absent.types_known);
    assert!(!absent.named_type);
}

#[test]
fn parse_metrics_body_dispatches_by_shape() {
    // Prometheus text stays on the Prometheus path.
    let parsed = parse_metrics_body("# fixture\nfixture_queued 0\n");
    assert_eq!(parsed.format, Some(MetricsFormat::Prometheus));
    assert!(!parsed.json);
    assert_eq!(parsed.samples.len(), 1);

    // A Strata JSON object is recognised and normalised (ms → s).
    let strata = r#"{"engine":{"model":"m"},"totals":{"prompt_tokens":5,"output_tokens":2,"prompt_ms":2000.0,"decode_ms":500.0},"requests_kept":1}"#;
    let parsed = parse_metrics_body(strata);
    assert_eq!(parsed.format, Some(MetricsFormat::Strata));
    assert!(parsed.json);
    assert!(parsed
        .samples
        .iter()
        .any(|s| s.name == "llamacpp:prompt_tokens_total" && s.value == 5.0));
    assert!(parsed
        .samples
        .iter()
        .any(|s| s.name == "llamacpp:prompt_seconds_total" && s.value == 2.0));
    assert!(parsed
        .samples
        .iter()
        .any(|s| s.name == "llamacpp:tokens_predicted_seconds_total" && s.value == 0.5));
}

#[test]
fn classify_backend_requires_positive_evidence() {
    use crate::domain::ServerBackend;

    // A recognised Strata JSON body without any `live.state` is still
    // positively Strata — it must never be reported as llama.cpp.
    let strata = r#"{"engine":{"model":"m"},"totals":{"prompt_tokens":5,"output_tokens":2,"prompt_ms":2000.0,"decode_ms":500.0},"requests_kept":1}"#;
    let parsed = parse_metrics_body(strata);
    assert_eq!(parsed.format, Some(MetricsFormat::Strata));
    assert_eq!(
        classify_backend(parsed.format, &parsed.samples),
        ServerBackend::Strata
    );

    // A Prometheus exposition counts as llama.cpp only when it actually
    // carries llama.cpp metric names.
    let llama = parse_metrics_body("# fixture\nllamacpp:prompt_tokens_total 5\n");
    assert_eq!(
        classify_backend(llama.format, &llama.samples),
        ServerBackend::LlamaCpp
    );
    // Some other Prometheus endpoint is not claimed as llama.cpp.
    let other = parse_metrics_body("# fixture\nmysql_up 1\n");
    assert_eq!(other.format, Some(MetricsFormat::Prometheus));
    assert_eq!(
        classify_backend(other.format, &other.samples),
        ServerBackend::Unknown
    );

    // Unrecognised JSON is neither Strata nor llama.cpp.
    let json = parse_metrics_body("{\"foo\": 1}");
    assert_eq!(
        classify_backend(json.format, &json.samples),
        ServerBackend::Unknown
    );
}

#[test]
fn pretty_printed_json_never_reaches_the_prometheus_parser() {
    // The line-oriented Prometheus parser would read `"prompt_tokens": 5`
    // as a bogus sample. Dispatch by shape prevents that.
    let pretty =
            "{\n  \"engine\": {},\n  \"totals\": {\n    \"prompt_tokens\": 5\n  },\n  \"live\": {}\n}\n";
    let parsed = parse_metrics_body(pretty);
    assert_eq!(parsed.format, Some(MetricsFormat::Strata));
    assert_eq!(parsed.samples.len(), 1);
    assert_eq!(parsed.samples[0].name, "llamacpp:prompt_tokens_total");
    assert_eq!(parsed.samples[0].value, 5.0);

    // JSON that is not a Strata telemetry object is JSON, not metrics.
    let other = "{\n  \"foo\": 1\n}\n";
    let parsed = parse_metrics_body(other);
    assert_eq!(parsed.format, None);
    assert!(parsed.json);
    assert!(parsed.samples.is_empty());
}

#[test]
fn parser_mutation_fuzz_smoke_does_not_panic() {
    // Deterministic mutation fuzzing over the checked-in parser seeds.
    // A libFuzzer target needs a library crate; this keeps parser
    // robustness in the normal gate without adding a dependency.
    const SEEDS: [&str; 4] = [
        include_str!("../../../tests/fixtures/metrics_current.prom"),
        include_str!("../../../tests/fixtures/metrics_speculative.prom"),
        include_str!("../../../tests/fixtures/metrics_partial.prom"),
        include_str!("../../../tests/fixtures/slots_active.json"),
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..3_000 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let seed = SEEDS[(state as usize) % SEEDS.len()];
        let mut bytes = seed.as_bytes().to_vec();
        for _ in 0..8 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = (state as usize) % bytes.len();
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            bytes[index] = (state >> 32) as u8;
        }
        let text = String::from_utf8_lossy(&bytes);
        let _ = parse_metrics_body(&text);
        let _ = parse_prometheus(&text);
    }
}

#[test]
fn parses_current_llama_metrics_fixture() {
    let metrics = parse_prometheus(include_str!("../../../tests/fixtures/metrics_current.prom"));

    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_total"]),
        Some(12000.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_cached_total"]),
        Some(26000.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:prompt_seconds_total"]),
        Some(12.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:tokens_predicted_seconds_total"]),
        Some(20.0)
    );
    assert_eq!(
        average_over_seconds(
            pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_total"]),
            pick_metric_opt(&metrics, &["llamacpp:prompt_seconds_total"]),
        ),
        Some(1000.0)
    );
}

#[test]
fn speculative_fixture_preserves_position_labels() {
    let metrics = parse_prometheus(include_str!(
        "../../../tests/fixtures/metrics_speculative.prom"
    ));
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]),
        Some(200.0)
    );
    assert_eq!(
        pick_metric_opt(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"]
        ),
        Some(140.0)
    );
    assert_eq!(
        metrics
            .iter()
            .filter(|sample| {
                sample.name == "llamacpp:spec_decode_num_accepted_tokens_per_pos_total"
                    && sample.labels.is_some()
            })
            .count(),
        4
    );
}

#[test]
fn parser_ignores_non_finite_values_and_does_not_collapse_labels() {
    let metrics = parse_prometheus(
        "llamacpp:test{slot=\"0\"} 20\nllamacpp:test{slot=\"1\"} 30\nnot_finite NaN\n",
    );
    assert_eq!(
        metrics
            .iter()
            .filter(|metric| metric.name == "llamacpp:test")
            .count(),
        2
    );
    // A labeled series must not be collapsed into an unlabeled aggregate.
    assert_eq!(pick_metric_opt(&metrics, &["llamacpp:test"]), None);
    assert_eq!(pick_metric_opt(&metrics, &["missing"]), None);
    assert!(!metrics.iter().any(|metric| metric.name == "not_finite"));
}

#[test]
fn derives_average_throughput_from_time_counters() {
    assert_eq!(average_over_seconds(Some(1200.0), Some(2.0)), Some(600.0));
    assert_eq!(average_over_seconds(Some(1200.0), Some(0.0)), None);
    // An absent numerator or denominator is unavailable, not zero.
    assert_eq!(average_over_seconds(None, Some(2.0)), None);
    assert_eq!(average_over_seconds(Some(1200.0), None), None);
}

#[test]
fn counter_delta_handles_counter_reset() {
    assert_eq!(counter_delta_opt(150.0, 100.0), Some(50.0));
    // A counter decrease (reset) is unavailable, not a real 0.
    assert_eq!(counter_delta_opt(10.0, 100.0), None);
    // Unchanged counters are a legitimate 0/s rate.
    assert_eq!(counter_rate(Some(0.0), Some(0.0), 1.0), Some(0.0));
    // An absent current or previous value is unavailable, not 0.
    assert_eq!(counter_rate(None, Some(5.0), 1.0), None);
    assert_eq!(counter_rate(Some(5.0), None, 1.0), None);
}

#[test]
fn derives_live_context_from_slots_fixture() {
    let mut stats = LlmStats {
        context_size: 4096,
        ..Default::default()
    };
    let slots: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/slots_active.json")).unwrap();

    let counters = apply_slots_json(&mut stats, &slots).unwrap();

    assert!(stats.slots_available);
    assert_eq!(stats.slot_count, 2);
    assert_eq!(stats.busy_slots, 1);
    assert_eq!(stats.context_size, 196608);
    assert_eq!(stats.context_used, Some(38779));
    assert_eq!(stats.context_slot_id, Some(0));
    assert_eq!(counters[0].prompt_processed, Some(12000));
    assert_eq!(counters[0].decoded, Some(779));
    assert_eq!(stats.request_prompt_tokens, Some(12000));
    assert_eq!(stats.request_generated_tokens, Some(779));
}

#[test]
fn derives_live_throughput_from_matching_slot_task() {
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(42),
        prompt_processed: Some(1000),
        decoded: Some(100),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(42),
        prompt_processed: Some(1250),
        decoded: Some(130),
    }];

    let (prompt_tps, generation_tps) = slot_delta_tps(&previous, &current, 0.5);
    assert_eq!(prompt_tps, Some(500.0));
    assert_eq!(generation_tps, Some(60.0));
}

#[test]
fn new_slot_task_does_not_create_false_live_spike() {
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(42),
        prompt_processed: Some(1000),
        decoded: Some(500),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(43),
        prompt_processed: Some(100),
        decoded: Some(5),
    }];

    // A task switch makes the pair unverifiable: abstain (the caller falls
    // back to the aggregated counters) rather than emit a fake spike.
    assert_eq!(slot_delta_tps(&previous, &current, 0.5), (None, None));
}

#[test]
fn idle_slots_fixture_reports_no_busy_slots() {
    let mut stats = LlmStats::default();
    let slots: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/slots_idle.json")).unwrap();
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.slot_count, 1);
    assert_eq!(stats.busy_slots, 0);
    // No slot is processing, so there is no current request to report.
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(stats.request_generated_tokens, None);
    // `n_prompt_tokens_processed: 0` + `n_decoded: 0` are present, so a
    // real 0 is reported here (not unknown).
    assert_eq!(stats.context_used, Some(0));
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn props_fixture_exposes_context_slots_and_model() {
    let props: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/props.json")).unwrap();
    assert_eq!(
        json_u64_path(&props, &["default_generation_settings", "n_ctx"]),
        Some(196608)
    );
    assert_eq!(props.get("total_slots").and_then(Value::as_u64), Some(2));
    assert_eq!(
        json_string(&props, &["model_path"])
            .map(|value| model_display_name(&value))
            .as_deref(),
        Some("Qwen3.8-27B-UD-Q4_K_M")
    );
}

#[test]
fn context_occupancy_is_not_derived_from_progress_counters() {
    // A slot that reports only progress counters (no `n_prompt_tokens`) has
    // no trustworthy occupancy: CTX must stay unknown, never become the
    // processed + decoded progress sum (that is request progress, not KV).
    let mut stats = LlmStats::default();
    let slots = json!([{
        "n_ctx": 8192,
        "is_processing": true,
        "n_prompt_tokens_processed": 4000,
        "next_token": [{"n_decoded": 250}]
    }]);

    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, None, "progress is not occupancy");
    assert_eq!(stats.context_size, 8192);
    // The same counters still feed the REQUEST progress row and throughput.
    assert_eq!(stats.request_prompt_tokens, Some(4000));
    assert_eq!(stats.request_generated_tokens, Some(250));
    // The slot reports no `id`; the display identity stays unknown.
    assert_eq!(stats.context_slot_id, None);
}

#[test]
fn context_pair_stays_with_most_used_busy_slot_when_capacities_differ() {
    // The pair must come from one slot: the busy slot's own used AND the
    // busy slot's own n_ctx — never max(used) from one slot mixed with
    // max(n_ctx) from another.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "id_task": 1, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 5000 },
        { "id": 1, "id_task": 2, "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.busy_slots, 1);
    assert_eq!(stats.context_used, Some(5000));
    assert_eq!(stats.context_size, 100000);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn context_pair_falls_back_to_most_used_slot_when_all_idle() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 5000 },
        { "id": 1, "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.busy_slots, 0);
    assert_eq!(stats.context_used, Some(9000));
    assert_eq!(stats.context_size, 200000);
    assert_eq!(stats.context_slot_id, Some(1));
}

#[test]
fn both_busy_slots_select_the_most_used_pair() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 2000 },
        { "id": 1, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 4000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.busy_slots, 2);
    assert_eq!(stats.context_used, Some(4000));
    assert_eq!(stats.context_size, 150000);
    // Occupancy only (no `n_prompt_tokens_processed`): the busy slots'
    // request prompt progress is unknown, not the summed CTX figures.
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(stats.context_slot_id, Some(1));
}

#[test]
fn idle_slot_near_capacity_reports_its_own_pair() {
    // Real observed case: n_ctx_slot = 70144, n_tokens = 70143, and a
    // request of 70210 tokens was rejected as exceeding the slot context.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 70144, "is_processing": false, "n_prompt_tokens": 70143 },
        { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 100 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(70143));
    assert_eq!(stats.context_size, 70144);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn slot_reuse_tracks_new_task_slot_over_stale_retained_context() {
    // Slot reuse: a busy slot runs a new small-prompt task while a second
    // slot still holds its previous task's retained context. The pair
    // follows the active slot, not the stale occupancy.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "id_task": 99, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 1022 },
        { "id": 1, "id_task": 42, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 37943 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(1022));
    assert_eq!(stats.context_size, 115200);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn context_slot_id_is_the_slot_api_id_not_the_array_position() {
    // Non-contiguous /slots ids: the reported identity must be the
    // slot's own `id` field, never its position in the array.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 3, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 9000 },
        { "id": 7, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 2000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(2000));
    assert_eq!(stats.context_size, 150000);
    assert_eq!(stats.context_slot_id, Some(7));
}

#[test]
fn missing_slot_id_leaves_context_slot_id_unknown() {
    // A /slots response without per-slot `id` fields (older llama.cpp)
    // must not fabricate an identity for the displayed pair.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 5000 },
        { "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(5000));
    assert_eq!(stats.context_size, 100000);
    assert_eq!(stats.context_slot_id, None);
}

#[test]
fn single_slot_reports_its_id_for_the_pair() {
    let mut stats = LlmStats::default();
    let slots =
        json!([{ "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 28851 }]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.slot_count, 1);
    assert_eq!(stats.context_used, Some(28851));
    assert_eq!(stats.context_size, 115200);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn empty_slots_array_keeps_props_context_size() {
    let mut stats = LlmStats {
        context_size: 4096,
        ..Default::default()
    };
    apply_slots_json(&mut stats, &json!([])).unwrap();
    assert_eq!(stats.slot_count, 0);
    assert_eq!(stats.context_used, None);
    assert_eq!(stats.context_size, 4096);
    assert_eq!(stats.context_slot_id, None);
}

#[test]
fn slot_without_usage_reports_unknown_context_used_not_zero() {
    let mut stats = LlmStats {
        context_size: 4096,
        ..Default::default()
    };
    let slots = json!([{ "id": 0, "n_ctx": 8192, "is_processing": true }]);
    apply_slots_json(&mut stats, &slots).unwrap();
    // No usable occupancy: the pair keeps the real capacity but the used
    // count is unknown (-), never a fabricated 0.
    assert_eq!(stats.context_used, None);
    assert_eq!(stats.context_size, 8192);
    assert_eq!(stats.context_slot_id, Some(0));
    assert_eq!(stats.slot_overview[0].context_used, None);
    assert_eq!(stats.slot_overview[0].context_size, Some(8192));
}

#[test]
fn slot_missing_n_ctx_keeps_props_context_size() {
    let mut stats = LlmStats {
        context_size: 4096,
        ..Default::default()
    };
    let slots = json!([{ "id": 0, "n_prompt_tokens": 512, "is_processing": true }]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(512));
    assert_eq!(stats.context_size, 4096);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn slot_overview_reports_each_slot_with_own_id_state_and_usage() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 58745 },
        { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 34200 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(
        stats.slot_overview,
        vec![
            LlmSlotInfo {
                id: 0,
                busy: true,
                context_used: Some(58745),
                context_size: Some(115200),
            },
            LlmSlotInfo {
                id: 1,
                busy: false,
                context_used: Some(34200),
                context_size: Some(115200),
            }
        ]
    );
    // The main CTX pair must not be the sum of the slots.
    assert_eq!(stats.context_used, Some(58745));
    assert_eq!(stats.context_size, 115200);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn slot_overview_marks_busy_state_of_all_slots() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 2000 },
        { "id": 1, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 4000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert!(stats.slot_overview.iter().all(|slot| slot.busy));

    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 2000 },
        { "id": 1, "n_ctx": 150000, "is_processing": false, "n_prompt_tokens": 4000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert!(stats.slot_overview.iter().all(|slot| !slot.busy));
}

#[test]
fn slot_overview_orders_by_slot_id_not_array_position() {
    // Scrambled array order + non-contiguous IDs: the overview must be
    // deterministic and follow the slot's own `id`, never its position.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 7, "n_ctx": 200000, "is_processing": true, "n_prompt_tokens": 14000 },
        { "id": 2, "n_ctx": 150000, "is_processing": false, "n_prompt_tokens": 28000 },
        { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 31000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    let ids: Vec<u64> = stats.slot_overview.iter().map(|slot| slot.id).collect();
    assert_eq!(ids, vec![1, 2, 7]);
    assert_eq!(
        stats.slot_overview[0].context_used,
        Some(31000),
        "values must travel with their slot id"
    );
}

#[test]
fn slot_overview_keeps_differing_capacities_per_slot() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 58745 },
        { "id": 3, "n_ctx": 70144, "is_processing": false, "n_prompt_tokens": 34200 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.slot_overview[0].context_size, Some(115200));
    assert_eq!(stats.slot_overview[1].context_size, Some(70144));
    // The main pair still comes from a single slot.
    assert_eq!(stats.context_used, Some(58745));
    assert_eq!(stats.context_size, 115200);
    assert_eq!(stats.context_slot_id, Some(0));
}

#[test]
fn slot_overview_keeps_missing_values_unknown_and_drops_unidentified_slots() {
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 0 },
        { "id": 1, "is_processing": false },
        { "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 5000 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    // The slot without an `id` must not be guessed from its position.
    assert_eq!(
        stats
            .slot_overview
            .iter()
            .map(|slot| slot.id)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        stats.slot_overview[0].context_used,
        Some(0),
        "a real zero is a zero"
    );
    let unknown = &stats.slot_overview[1];
    assert_eq!(
        unknown.context_used, None,
        "absent data must not become a fake zero"
    );
    assert_eq!(unknown.context_size, None);
}

#[test]
fn present_zero_prompt_tokens_is_a_real_zero_not_the_progress() {
    // `n_prompt_tokens: 0` is the slot's direct occupancy saying zero. The
    // retained `n_prompt_tokens_processed` progress must not override it.
    let mut stats = LlmStats::default();
    let slots = json!([
        { "id": 0, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 0, "n_prompt_tokens_processed": 37943 },
        { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 100 }
    ]);
    apply_slots_json(&mut stats, &slots).unwrap();
    // The most-used slot (100) wins the main CTX pair, not the slot whose
    // only large number is processing progress.
    assert_eq!(stats.context_used, Some(100));
    assert_eq!(stats.context_slot_id, Some(1));
    assert_eq!(
        stats.slot_overview[0].context_used,
        Some(0),
        "a present zero is a real zero, not the retained progress"
    );
    assert_eq!(stats.slot_overview[1].context_used, Some(100));
}

#[test]
fn slot_overview_is_empty_when_slots_endpoint_is_unavailable() {
    let stats = LlmStats::default();
    // No apply_slots_json call at all: /slots failed -> default is empty.
    assert!(!stats.slots_available);
    assert!(stats.slot_overview.is_empty());
}

#[test]
fn strata_old_slots_report_capacity_only_never_occupancy() {
    // Strata 0.1.40 /slots: id, n_ctx, is_processing only. Capacity is
    // known; occupancy stays unavailable (never a fabricated zero).
    let mut stats = LlmStats {
        context_size: 196608,
        ..Default::default()
    };
    let slots: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/slots_strata_old.json"
    ))
    .unwrap();
    apply_slots_json(&mut stats, &slots).unwrap();
    assert!(stats.slots_available);
    assert_eq!(stats.slot_count, 1);
    assert_eq!(stats.context_size, 196608);
    assert_eq!(stats.context_used, None);
    assert_eq!(stats.context_slot_id, Some(0));
    assert_eq!(stats.slot_overview[0].context_used, None);
    assert_eq!(stats.slot_overview[0].context_size, Some(196608));
}

#[test]
fn strata_new_slots_report_direct_occupancy() {
    // Newer Strata /slots adds n_prompt_tokens: the slot's context in use,
    // which is the direct occupancy field (a real figure, not derived).
    let mut stats = LlmStats {
        context_size: 196608,
        ..Default::default()
    };
    let slots: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/slots_strata_new.json"
    ))
    .unwrap();
    let counters = apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(113274));
    assert_eq!(stats.context_slot_id, Some(0));
    assert_eq!(stats.slot_overview[0].context_used, Some(113274));
    // The slot is busy but reports no `n_prompt_tokens_processed`: request
    // prompt progress is unknown. The CTX occupancy must not be reused as
    // REQUEST progress (CFA-86), neither for the row nor for the per-slot
    // prefill delta.
    assert_eq!(stats.busy_slots, 1);
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(counters[0].prompt_processed, None);
}

#[test]
fn llama_slot_without_processed_counter_keeps_request_prompt_unknown() {
    // A llama.cpp-shaped busy slot that reports occupancy and decode
    // progress but no `n_prompt_tokens_processed`: CTX stays the direct
    // occupancy, REQUEST prompt stays unknown, REQUEST generated is real.
    let mut stats = LlmStats::default();
    let slots = json!([{
        "id": 0,
        "id_task": 7,
        "n_ctx": 8192,
        "is_processing": true,
        "n_prompt_tokens": 3000,
        "next_token": [{"n_decoded": 40}]
    }]);
    let counters = apply_slots_json(&mut stats, &slots).unwrap();
    assert_eq!(stats.context_used, Some(3000));
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(stats.request_generated_tokens, Some(40));
    assert_eq!(counters[0].prompt_processed, None);
    assert_eq!(counters[0].decoded, Some(40));

    // A growing occupancy across two samples is not prefill progress: the
    // slot path abstains for the prompt dimension instead of deriving a
    // rate from CTX.
    let mut next = LlmStats::default();
    let later = json!([{
        "id": 0,
        "id_task": 7,
        "n_ctx": 8192,
        "is_processing": true,
        "n_prompt_tokens": 3500,
        "next_token": [{"n_decoded": 60}]
    }]);
    let later_counters = apply_slots_json(&mut next, &later).unwrap();
    let (prompt_tps, generation_tps) = slot_delta_tps(&counters, &later_counters, 1.0);
    assert_eq!(prompt_tps, None);
    assert_eq!(generation_tps, Some(20.0));
}

#[test]
fn slot_delta_tps_sums_active_slots_into_server_aggregate() {
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1000),
            decoded: Some(100),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(2000),
            decoded: Some(200),
        },
    ];
    let current = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1100),
            decoded: Some(150),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(2200),
            decoded: Some(300),
        },
    ];

    let (prompt_tps, generation_tps) = slot_delta_tps(&previous, &current, 2.0);
    assert_eq!(prompt_tps, Some(150.0));
    assert_eq!(generation_tps, Some(75.0));
}

#[test]
fn slot_delta_tps_abstains_when_no_slot_reports_counters() {
    // A counter-less /slots (for example Strata's `id,is_processing,n_ctx`)
    // must abstain, not read as a measured zero rate.
    let slot = || SlotCounter {
        slot_id: Some(0),
        task_id: None,
        prompt_processed: None,
        decoded: None,
    };
    assert_eq!(
        slot_delta_tps(&[slot()], &[slot()], 2.0),
        (None, None),
        "no reported counter means unavailable, never a fake zero"
    );
}

#[test]
fn slot_delta_tps_uses_the_slots_that_do_report_counters() {
    // One slot reports both dimensions, another omits them on both sides:
    // the reporting slot's delta is used and the silent slot does not
    // invalidate the dimension.
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(100),
            decoded: Some(10),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: None,
            decoded: None,
        },
    ];
    let current = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(160),
            decoded: Some(30),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: None,
            decoded: None,
        },
    ];
    let (prompt_tps, generation_tps) = slot_delta_tps(&previous, &current, 2.0);
    assert_eq!(prompt_tps, Some(30.0));
    assert_eq!(generation_tps, Some(10.0));
}

#[test]
fn slot_delta_tps_ignores_disappeared_slot_and_restarted_task() {
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1000),
            decoded: Some(500),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(2000),
            decoded: Some(300),
        },
    ];
    // Slot 0 disappeared; slot 1's task finished and a new task started
    // on the same slot — neither may leak a stale delta.
    let current = vec![SlotCounter {
        slot_id: Some(1),
        task_id: Some(3),
        prompt_processed: Some(50),
        decoded: Some(5),
    }];

    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, None));
}

#[test]
fn slot_delta_tps_abstains_when_a_slot_disappears_without_a_task_change() {
    // Regression: the surviving slot keeps the same task, so the task
    // check alone would pair it and return a survivor-only delta. A
    // disappeared slot must force the aggregate fallback regardless.
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(7),
            prompt_processed: Some(1000),
            decoded: Some(500),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(8),
            prompt_processed: Some(2000),
            decoded: Some(300),
        },
    ];
    let current = vec![SlotCounter {
        slot_id: Some(1),
        task_id: Some(8),
        prompt_processed: Some(2100),
        decoded: Some(400),
    }];

    assert_eq!(
        slot_delta_tps(&previous, &current, 1.0),
        (None, None),
        "a disappeared slot must abstain even when the survivor's task is unchanged"
    );
}

#[test]
fn slot_delta_tps_abstains_when_a_previous_slot_has_no_id() {
    // A previous slot that cannot be identified cannot be proven present,
    // so it abstains instead of trusting the identified survivors.
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: None,
            prompt_processed: Some(1000),
            decoded: Some(500),
        },
        SlotCounter {
            slot_id: None,
            task_id: None,
            prompt_processed: Some(2000),
            decoded: Some(300),
        },
    ];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: None,
        prompt_processed: Some(1100),
        decoded: Some(550),
    }];

    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, None));
}

#[test]
fn slot_delta_tps_counter_decrease_is_unavailable() {
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(1000),
        decoded: Some(900),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(900),
        decoded: Some(100),
    }];

    // A decrease in either counter (reset) makes that dimension
    // unavailable; it is never reported as a fabricated 0.
    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, None));
}

#[test]
fn missing_prompt_counter_invalidates_only_prompt() {
    // Same verified id+task; the prompt counter is absent this sample while
    // decoded stays present. Prompt TPS is unavailable; decode TPS stays.
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(100),
        decoded: Some(20),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: None,
        decoded: Some(25),
    }];
    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, Some(5.0)));
}

#[test]
fn missing_decoded_counter_invalidates_only_decode() {
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(100),
        decoded: Some(20),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(150),
        decoded: None,
    }];
    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (Some(50.0), None));
}

#[test]
fn reappearing_prompt_counter_does_not_spike() {
    // B has no prompt counter, C brings it back; pairing B->C must abstain
    // for prompt (no `150 - 0` catch-up spike), while decode still works.
    let b = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: None,
        decoded: Some(25),
    }];
    let c = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(150),
        decoded: Some(30),
    }];
    assert_eq!(slot_delta_tps(&b, &c, 1.0), (None, Some(5.0)));
}

#[test]
fn explicit_zero_slot_counter_is_a_real_zero() {
    // An unchanged explicit zero is a genuine 0/s, not unavailable.
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(0),
        decoded: Some(0),
    }];
    let current = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(0),
        decoded: Some(0),
    }];
    assert_eq!(
        slot_delta_tps(&previous, &current, 1.0),
        (Some(0.0), Some(0.0))
    );
}

#[test]
fn idle_slot_missing_counter_does_not_invalidate_the_dimension() {
    // Slot 1 is idle and omits both counters; slot 0 stays fully valid.
    // The idle gap must not clear either dimension.
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(100),
            decoded: Some(20),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: None,
            prompt_processed: None,
            decoded: None,
        },
    ];
    let current = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(150),
            decoded: Some(25),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: None,
            prompt_processed: None,
            decoded: None,
        },
    ];
    assert_eq!(
        slot_delta_tps(&previous, &current, 1.0),
        (Some(50.0), Some(5.0))
    );
}

#[test]
fn request_totals_require_all_busy_slots_to_report() {
    // One busy slot lacks the prompt counter: the prompt total is
    // incomplete (None), while the decode total (all present) is Some.
    let mut stats = LlmStats::default();
    apply_slots_json(
            &mut stats,
            &json!([
                {"id": 0, "is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 20}},
                {"id": 1, "is_processing": true, "next_token": {"n_decoded": 10}}
            ]),
        )
        .unwrap();
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(stats.request_generated_tokens, Some(30));

    // Mirror case: one busy slot lacks decoded.
    let mut stats = LlmStats::default();
    apply_slots_json(
            &mut stats,
            &json!([
                {"id": 0, "is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 20}},
                {"id": 1, "is_processing": true, "n_prompt_tokens_processed": 50}
            ]),
        )
        .unwrap();
    assert_eq!(stats.request_prompt_tokens, Some(150));
    assert_eq!(stats.request_generated_tokens, None);
}

#[test]
fn request_totals_sum_complete_busy_slots_and_ignore_idle_gaps() {
    // All busy slots report (including a real zero) -> exact sum.
    let mut stats = LlmStats::default();
    apply_slots_json(
            &mut stats,
            &json!([
                {"id": 0, "is_processing": true, "n_prompt_tokens_processed": 0, "next_token": {"n_decoded": 0}},
                {"id": 1, "is_processing": true, "n_prompt_tokens_processed": 5, "next_token": {"n_decoded": 3}}
            ]),
        )
        .unwrap();
    assert_eq!(stats.request_prompt_tokens, Some(5));
    assert_eq!(stats.request_generated_tokens, Some(3));

    // An idle slot with missing counters must not poison the busy total.
    let mut stats = LlmStats::default();
    apply_slots_json(
            &mut stats,
            &json!([
                {"id": 0, "is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 20}},
                {"id": 1, "is_processing": false}
            ]),
        )
        .unwrap();
    assert_eq!(stats.request_prompt_tokens, Some(100));
    assert_eq!(stats.request_generated_tokens, Some(20));

    // No busy slot -> inactive, no request total.
    let mut stats = LlmStats::default();
    apply_slots_json(
            &mut stats,
            &json!([
                {"id": 0, "is_processing": false, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 20}}
            ]),
        )
        .unwrap();
    assert_eq!(stats.request_prompt_tokens, None);
    assert_eq!(stats.request_generated_tokens, None);
}

#[test]
fn reordered_slots_without_ids_abstain_and_fall_back_to_metrics() {
    // Two slots with no `id`: array order is not identity, so a pure
    // reordering (identical counters) must not be read as activity.
    let previous = apply_slots_json(
            &mut LlmStats::default(),
            &json!([
                {"is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 10}},
                {"is_processing": true, "n_prompt_tokens_processed": 1000, "next_token": {"n_decoded": 100}}
            ]),
        )
        .unwrap();
    let current = apply_slots_json(
            &mut LlmStats::default(),
            &json!([
                {"is_processing": true, "n_prompt_tokens_processed": 1000, "next_token": {"n_decoded": 100}},
                {"is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 10}}
            ]),
        )
        .unwrap();
    assert!(previous.iter().all(|slot| slot.slot_id.is_none()));

    let slot_live = slot_delta_tps(&previous, &current, 1.0);
    assert_eq!(slot_live, (None, None), "no identity → no per-slot delta");
    // The decision must fall back to the aggregated metrics, never to the
    // fabricated 900/90 the old index-as-id behavior produced.
    assert_eq!(
        choose_live_throughput(slot_live, (None, None)),
        (None, None)
    );
}

#[test]
fn stable_slot_ids_keep_correct_deltas_when_reordered() {
    let previous = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1000),
            decoded: Some(100),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(2000),
            decoded: Some(200),
        },
    ];
    // Same slots, reordered, each advanced by its own amount.
    let current = vec![
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(2200),
            decoded: Some(300),
        },
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1100),
            decoded: Some(150),
        },
    ];
    assert_eq!(
        slot_delta_tps(&previous, &current, 2.0),
        (Some(150.0), Some(75.0))
    );
}

#[test]
fn duplicate_slot_ids_abstain() {
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(1000),
        decoded: Some(100),
    }];
    let current = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1100),
            decoded: Some(150),
        },
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1200),
            decoded: Some(160),
        },
    ];
    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, None));
}

#[test]
fn new_slot_among_known_ones_abstains() {
    // A new slot (id present, no previous match) makes the totals
    // untrustworthy, so the whole slot-delta path abstains.
    let previous = vec![SlotCounter {
        slot_id: Some(0),
        task_id: Some(1),
        prompt_processed: Some(1000),
        decoded: Some(100),
    }];
    let current = vec![
        SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: Some(1100),
            decoded: Some(150),
        },
        SlotCounter {
            slot_id: Some(1),
            task_id: Some(2),
            prompt_processed: Some(5),
            decoded: Some(1),
        },
    ];
    assert_eq!(slot_delta_tps(&previous, &current, 1.0), (None, None));
}

#[test]
fn rejects_non_array_slots_payload() {
    let mut stats = LlmStats::default();
    let error = apply_slots_json(&mut stats, &json!({"error": "disabled"})).unwrap_err();
    assert!(!stats.slots_available);
    assert!(error.contains("not an array"));
}

#[test]
fn model_path_is_shortened_for_display() {
    assert_eq!(
        model_display_name("/models/Qwen3.8-27B-UD-Q4_K_M.gguf"),
        "Qwen3.8-27B-UD-Q4_K_M"
    );
}

/// A deterministic mock `/props`/`/metrics`/`/slots` backend.
///
/// `respond(path, n)` returns `(status, content_type, body)` for the `n`-th
/// request to that exact path. Counting per path (never globally) keeps a
/// sequenced endpoint deterministic even though `sample()` fetches the
/// three endpoints concurrently. The server thread stops when the handle is
/// dropped.

#[test]
fn prometheus_payload_with_unknown_fields_keeps_known_metrics_and_labels() {
    let metrics = parse_prometheus(include_str!(
        "../../../tests/fixtures/metrics_schema_unknown.prom"
    ));

    // A reordered exposure with HELP/TYPE noise and unknown series still
    // yields the recognised metrics.
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_total"]),
        Some(777.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:tokens_predicted_total"]),
        Some(444.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_cached_total"]),
        Some(123.0)
    );
    assert_eq!(
        pick_metric_opt(&metrics, &["llamacpp:requests_processing"]),
        Some(2.0)
    );
    // An unknown series is retained verbatim with its label intact and does
    // not shadow a recognised metric.
    assert_eq!(
        metrics
            .iter()
            .filter(|sample| sample.name == "llamacpp:a_future_metric_total")
            .count(),
        2
    );
}

#[test]
fn strata_metrics_fixture_tolerates_unknown_and_reordered_fields() {
    let parsed = parse_metrics_body(include_str!(
        "../../../tests/fixtures/strata_metrics_unknown.json"
    ));

    assert_eq!(parsed.format, Some(MetricsFormat::Strata));
    assert_eq!(
        classify_backend(parsed.format, &parsed.samples),
        crate::domain::ServerBackend::Strata
    );
    // Identity and capacity survive unknown sibling fields.
    assert_eq!(parsed.strata_since, Some(1_800_000_000.5));
    assert_eq!(parsed.strata_capacity, Some(131_072));
    // Known counters are normalised (ms -> s); unknown fields never become
    // canonical samples.
    assert_eq!(
        pick_metric_opt(&parsed.samples, &["llamacpp:prompt_tokens_total"]),
        Some(42.0)
    );
    assert_eq!(
        pick_metric_opt(&parsed.samples, &["llamacpp:tokens_predicted_total"]),
        Some(7.0)
    );
    assert_eq!(
        pick_metric_opt(&parsed.samples, &["llamacpp:prompt_seconds_total"]),
        Some(2.1)
    );
    let decode_seconds = pick_metric_opt(
        &parsed.samples,
        &["llamacpp:tokens_predicted_seconds_total"],
    )
    .expect("decode seconds present");
    assert!(
        (decode_seconds - 0.7).abs() < 1e-9,
        "700 ms must normalise to 0.7 s, got {decode_seconds}"
    );
    assert!(!parsed
        .samples
        .iter()
        .any(|sample| sample.name.contains("future") || sample.name.contains("new_metric")));
}
