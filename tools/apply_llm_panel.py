from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"target not found: {label}")
    return text.replace(old, new, 1)


p = Path("src/llama.rs")
s = p.read_text()
s = replace_once(s, "const PROPS_REFRESH: Duration = Duration::from_secs(30);\n", "const PROPS_REFRESH: Duration = Duration::from_secs(30);\nconst SPEC_ACCEPTANCE_HOLD: Duration = Duration::from_secs(3);\n", "spec hold constant")
s = replace_once(s, "    pub prompt_total: f64,\n    pub prompt_cached_total: f64,\n    pub generated_total: f64,\n", "    pub prompt_total: f64,\n    pub prompt_cached_total: Option<f64>,\n    pub generated_total: f64,\n    pub request_prompt_tokens: u64,\n    pub request_generated_tokens: u64,\n", "llm stats token fields")
s = replace_once(s, "    previous_slots: PreviousSlotCounters,\n    props: CachedProps,\n", "    previous_slots: PreviousSlotCounters,\n    props: CachedProps,\n    last_spec_acceptance_pct: Option<f64>,\n    last_spec_acceptance_at: Option<Instant>,\n", "monitor spec fields")
s = replace_once(s, "            previous_slots: PreviousSlotCounters::default(),\n            props: CachedProps::default(),\n", "            previous_slots: PreviousSlotCounters::default(),\n            props: CachedProps::default(),\n            last_spec_acceptance_pct: None,\n            last_spec_acceptance_at: None,\n", "monitor initialization")
s = replace_once(s, '        stats.prompt_cached_total = pick_metric(&metrics, &["llamacpp:prompt_tokens_cached_total"]);\n', '        stats.prompt_cached_total =\n            pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_cached_total"]);\n', "optional cache metric")

old_mtp = '''            let draft_delta =
                counter_delta(stats.spec_draft_tokens, self.previous_metrics.draft_total);
            if draft_delta > 0.0 {
                let accepted_delta = counter_delta(
                    stats.spec_accepted_tokens,
                    self.previous_metrics.accepted_total,
                );
                stats.spec_acceptance_pct =
                    Some((accepted_delta / draft_delta * 100.0).clamp(0.0, 100.0));
            }
'''
new_mtp = '''            let draft_delta =
                counter_delta(stats.spec_draft_tokens, self.previous_metrics.draft_total);
            if draft_delta > 0.0 {
                let accepted_delta = counter_delta(
                    stats.spec_accepted_tokens,
                    self.previous_metrics.accepted_total,
                );
                let acceptance = (accepted_delta / draft_delta * 100.0).clamp(0.0, 100.0);
                self.last_spec_acceptance_pct = Some(acceptance);
                self.last_spec_acceptance_at = Some(now);
                stats.spec_acceptance_pct = Some(acceptance);
            } else if self
                .last_spec_acceptance_at
                .is_some_and(|at| now.saturating_duration_since(at) <= SPEC_ACCEPTANCE_HOLD)
            {
                stats.spec_acceptance_pct = self.last_spec_acceptance_pct;
            }
'''
s = replace_once(s, old_mtp, new_mtp, "mtp hold")
s = replace_once(s, "    let mut max_context_used = 0u64;\n    let mut busy = 0u64;\n    let mut counters = Vec::with_capacity(slots.len());\n", "    let mut max_context_used = 0u64;\n    let mut busy = 0u64;\n    let mut request_prompt_tokens = 0u64;\n    let mut request_generated_tokens = 0u64;\n    let mut counters = Vec::with_capacity(slots.len());\n", "request counters locals")
old_busy = '''        if slot
            .get("is_processing")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            busy += 1;
        }
'''
new_busy = '''        let is_processing = slot
            .get("is_processing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_processing {
            busy += 1;
        }
'''
s = replace_once(s, old_busy, new_busy, "slot processing flag")
s = replace_once(s, "        let decoded = slot_decoded_tokens(slot);\n\n        // Current llama.cpp exposes n_prompt_tokens as the slot's current prompt/context\n", "        let decoded = slot_decoded_tokens(slot);\n\n        if is_processing {\n            request_prompt_tokens = request_prompt_tokens.saturating_add(prompt_processed);\n            request_generated_tokens = request_generated_tokens.saturating_add(decoded);\n        }\n\n        // Current llama.cpp exposes n_prompt_tokens as the slot's current prompt/context\n", "request token aggregation")
s = replace_once(s, "    stats.busy_slots = busy;\n    stats.context_size = max_context_size;\n    stats.context_used = max_context_used;\n", "    stats.busy_slots = busy;\n    stats.request_prompt_tokens = request_prompt_tokens;\n    stats.request_generated_tokens = request_generated_tokens;\n    stats.context_size = max_context_size;\n    stats.context_used = max_context_used;\n", "request token assignment")
old_pick = '''fn pick_metric(metrics: &[MetricSample], names: &[&str]) -> f64 {
    for name in names {
        if let Some(sample) = metrics
            .iter()
            .find(|sample| sample.name == *name && sample.labels.is_none())
        {
            return sample.value;
        }
    }
    0.0
}
'''
new_pick = '''fn pick_metric_opt(metrics: &[MetricSample], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        metrics
            .iter()
            .find(|sample| sample.name == **name && sample.labels.is_none())
            .map(|sample| sample.value)
    })
}

fn pick_metric(metrics: &[MetricSample], names: &[&str]) -> f64 {
    pick_metric_opt(metrics, names).unwrap_or(0.0)
}
'''
s = replace_once(s, old_pick, new_pick, "optional metric helper")
s = replace_once(s, "        assert_eq!(counters[0].prompt_processed, 12000);\n        assert_eq!(counters[0].decoded, 779);\n", "        assert_eq!(counters[0].prompt_processed, 12000);\n        assert_eq!(counters[0].decoded, 779);\n        assert_eq!(stats.request_prompt_tokens, 12000);\n        assert_eq!(stats.request_generated_tokens, 779);\n", "request token fixture assertions")
s = replace_once(s, '        assert_eq!(pick_metric(&metrics, &["llamacpp:test"]), 0.0);\n        assert!(!metrics.iter().any(|metric| metric.name == "not_finite"));\n', '        assert_eq!(pick_metric(&metrics, &["llamacpp:test"]), 0.0);\n        assert_eq!(pick_metric_opt(&metrics, &["llamacpp:test"]), None);\n        assert_eq!(pick_metric_opt(&metrics, &["missing"]), None);\n        assert!(!metrics.iter().any(|metric| metric.name == "not_finite"));\n', "optional metric assertions")
s = replace_once(s, "        assert_eq!(stats.busy_slots, 0);\n        assert_eq!(stats.context_used, 0);\n", "        assert_eq!(stats.busy_slots, 0);\n        assert_eq!(stats.request_prompt_tokens, 0);\n        assert_eq!(stats.request_generated_tokens, 0);\n        assert_eq!(stats.context_used, 0);\n", "idle request assertions")
p.write_text(s)

p = Path("src/ui.rs")
s = p.read_text()
start = s.index("    let context_used = if llm.slots_available {")
end = s.index("    lines.truncate(inner.height as usize);", start)
new_block = '''    let context_used = if llm.slots_available {
        llm.context_used
    } else {
        llm.context_high_watermark
    };
    let context_pct = if llm.context_size > 0 {
        context_used as f64 / llm.context_size as f64 * 100.0
    } else {
        0.0
    };
    let context_bar = inner.width.saturating_sub(46).max(8) as usize;
    let slots = if llm.slots_available {
        format!("{}/{}", llm.busy_slots, llm.slot_count)
    } else if llm.props_slot_count > 0 {
        format!("—/{}", llm.props_slot_count)
    } else {
        "—".to_string()
    };
    let (mtp, mtp_color) = match llm.spec_acceptance_pct {
        Some(value) => (format!("{value:.0}%"), ORK_GREEN),
        None => ("—".to_string(), MUTED),
    };
    let (phase, phase_color) = llm_phase(llm);
    let live_pp = if llm.prompt_tps > 0.05 {
        format!("{:>7.1} tok/s", llm.prompt_tps)
    } else {
        "      — tok/s".to_string()
    };
    let live_tg = if llm.generation_tps > 0.05 {
        format!("{:>7.1} tok/s", llm.generation_tps)
    } else {
        "      — tok/s".to_string()
    };
    let request_pp = if llm.busy_slots > 0 {
        llm.request_prompt_tokens.to_string()
    } else {
        "—".to_string()
    };
    let request_tg = if llm.busy_slots > 0 {
        llm.request_generated_tokens.to_string()
    } else {
        "—".to_string()
    };
    let cache = llm
        .prompt_cached_total
        .map(|value| format!("{value:.0}"))
        .unwrap_or_else(|| "—".to_string());

    let mut lines = vec![
        Line::from(vec![
            label_span(" MODEL    "),
            Span::styled(
                llm.model.clone(),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" STATE    "),
            value_span(phase, phase_color),
            Span::raw("    "),
            label_span("SLOT "),
            value_span(&slots, CYAN),
            Span::raw("    "),
            label_span("QUEUED "),
            value_span(&format!("{:.0}", llm.deferred_requests), WHITE),
            Span::raw("    "),
            label_span("MTP "),
            value_span(&mtp, mtp_color),
        ]),
        Line::from(vec![
            label_span(" LIVE     "),
            Span::styled(format!("PP {live_pp}"), Style::default().fg(CYAN)),
            Span::raw("    "),
            Span::styled(
                format!("TG {live_tg}"),
                Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            label_span(" SERVER   "),
            Span::styled(
                format!("PP {:>7.1} tok/s", llm.prompt_avg_tps),
                Style::default().fg(MUTED),
            ),
            Span::raw("    "),
            Span::styled(
                format!("TG {:>7.1} tok/s", llm.generation_avg_tps),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            label_span(" REQUEST  "),
            value_span(&format!("PP {request_pp}"), WHITE),
            Span::raw("    "),
            value_span(&format!("TG {request_tg}"), WHITE),
        ]),
        Line::from(vec![
            label_span(" TOTAL    "),
            value_span(&format!("PP {:.0}", llm.prompt_total), WHITE),
            Span::raw("    "),
            value_span(&format!("TG {:.0}", llm.generated_total), WHITE),
            Span::raw("    "),
            label_span("CACHE "),
            value_span(&cache, CYAN),
        ]),
        meter_line(
            "CTX",
            context_pct,
            context_bar,
            context_color(context_pct),
            format!("{:>5.1}%", context_pct),
            vec![Span::styled(
                if llm.context_size > 0 {
                    format!(" {context_used} / {} tok", llm.context_size)
                } else {
                    " waiting for context".to_string()
                },
                Style::default().fg(MUTED),
            )],
        ),
    ];

'''
s = s[:start] + new_block + s[end:]
marker = "fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {"
helper = '''fn llm_phase(llm: &LlmStats) -> (&'static str, Color) {
    if llm.generation_tps > 0.05 {
        ("GENERATING", ORK_GREEN)
    } else if llm.prompt_tps > 0.05 {
        ("PREFILL", CYAN)
    } else if llm.busy_slots > 0 || llm.active_requests > 0.0 {
        ("PROCESSING", YELLOW)
    } else if llm.deferred_requests > 0.0 {
        ("QUEUED", YELLOW)
    } else {
        ("IDLE", MUTED)
    }
}

'''
s = replace_once(s, marker, helper + marker, "llm phase helper")
p.write_text(s)
