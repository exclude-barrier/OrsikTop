from pathlib import Path


def replace_once(text: str, old: str, new: str) -> str:
    if old not in text:
        raise SystemExit(f"expected block not found:\n{old[:400]}")
    return text.replace(old, new, 1)


# --- app.rs: keep the last good LLM sample through short polling hiccups ---
app_path = Path("src/app.rs")
app = app_path.read_text()

app = replace_once(
    app,
    '''const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);\nconst PROCESS_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);\n''',
    '''const SYSTEM_REFRESH_INTERVAL: Duration = Duration::from_millis(250);\nconst PROCESS_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);\nconst LLM_OFFLINE_GRACE: Duration = Duration::from_millis(2500);\n''',
)

app = replace_once(
    app,
    '''        let llama_init_error = if llama.is_none() {\n            "failed to initialize HTTP client".to_string()\n        } else {\n            String::new()\n        };\n\n        while !stop.load(Ordering::Relaxed) {\n''',
    '''        let llama_init_error = if llama.is_none() {\n            "failed to initialize HTTP client".to_string()\n        } else {\n            String::new()\n        };\n        let mut last_good_llm: Option<(LlmStats, Instant)> = None;\n\n        while !stop.load(Ordering::Relaxed) {\n''',
)

app = replace_once(
    app,
    '''            let stats = match llama.as_mut() {\n                Some(monitor) => monitor.sample(),\n                None => LlmStats {\n                    error: llama_init_error.clone(),\n                    ..Default::default()\n                },\n            };\n\n            match tx.try_send(stats) {\n''',
    '''            let raw_stats = match llama.as_mut() {\n                Some(monitor) => monitor.sample(),\n                None => LlmStats {\n                    error: llama_init_error.clone(),\n                    ..Default::default()\n                },\n            };\n            let stats = stabilize_llm_sample(raw_stats, &mut last_good_llm, Instant::now());\n\n            match tx.try_send(stats) {\n''',
)

insert_before = '''fn sleep_until_next_cycle(\n'''
helper = '''fn stabilize_llm_sample(\n    stats: LlmStats,\n    last_good: &mut Option<(LlmStats, Instant)>,\n    now: Instant,\n) -> LlmStats {\n    if stats.connected {\n        *last_good = Some((stats.clone(), now));\n        return stats;\n    }\n\n    let error = stats.error.to_ascii_lowercase();\n    let hard_failure = error.contains("metrics disabled")\n        || error.contains("--metrics")\n        || error.contains("http 501")\n        || error.contains("failed to initialize http client");\n\n    if !hard_failure {\n        if let Some((previous, at)) = last_good.as_ref() {\n            if now.saturating_duration_since(*at) <= LLM_OFFLINE_GRACE {\n                let mut held = previous.clone();\n                held.error.clear();\n                return held;\n            }\n        }\n    }\n\n    stats\n}\n\n'''
if insert_before not in app:
    raise SystemExit("sleep_until_next_cycle anchor not found in app.rs")
app = app.replace(insert_before, helper + insert_before, 1)

# Add focused tests if app.rs has no existing test module.
if "mod tests {" not in app:
    app += '''\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn transient_llm_failure_keeps_last_good_sample_during_grace() {\n        let now = Instant::now();\n        let good = LlmStats {\n            connected: true,\n            model: "test-model".to_string(),\n            generation_tps: 42.0,\n            ..Default::default()\n        };\n        let mut last_good = Some((good.clone(), now));\n        let failed = LlmStats {\n            error: "cannot reach llama.cpp: timed out".to_string(),\n            ..Default::default()\n        };\n\n        let held = stabilize_llm_sample(\n            failed,\n            &mut last_good,\n            now + Duration::from_millis(1000),\n        );\n\n        assert!(held.connected);\n        assert_eq!(held.model, "test-model");\n        assert_eq!(held.generation_tps, 42.0);\n        assert!(held.error.is_empty());\n    }\n\n    #[test]\n    fn llm_failure_becomes_offline_after_grace() {\n        let now = Instant::now();\n        let good = LlmStats {\n            connected: true,\n            ..Default::default()\n        };\n        let mut last_good = Some((good, now));\n        let failed = LlmStats {\n            error: "cannot reach llama.cpp: connection refused".to_string(),\n            ..Default::default()\n        };\n\n        let offline = stabilize_llm_sample(\n            failed,\n            &mut last_good,\n            now + LLM_OFFLINE_GRACE + Duration::from_millis(1),\n        );\n\n        assert!(!offline.connected);\n        assert!(!offline.error.is_empty());\n    }\n\n    #[test]\n    fn metrics_disabled_is_not_hidden_by_grace() {\n        let now = Instant::now();\n        let good = LlmStats {\n            connected: true,\n            ..Default::default()\n        };\n        let mut last_good = Some((good, now));\n        let disabled = LlmStats {\n            error: "/metrics disabled; start llama.cpp with --metrics".to_string(),\n            ..Default::default()\n        };\n\n        let result = stabilize_llm_sample(disabled, &mut last_good, now);\n        assert!(!result.connected);\n        assert!(result.error.contains("--metrics"));\n    }\n}\n'''

app_path.write_text(app)

# --- llama.rs: avoid an overly aggressive timeout while the server is busy ---
llama_path = Path("src/llama.rs")
llama = llama_path.read_text()
llama = replace_once(
    llama,
    '''            .timeout(Duration::from_millis(700))\n''',
    '''            .timeout(Duration::from_millis(1200))\n''',
)
llama_path.write_text(llama)

# --- ui.rs: distinguish true metrics-off from offline and use spare LLM space ---
ui_path = Path("src/ui.rs")
ui = ui_path.read_text()

llm_start = ui.index("fn draw_llm(")
llm_end = ui.index("\nfn draw_system(", llm_start)
section = ui[llm_start:llm_end]

old_offline = '''    if !llm.connected {\n        frame.render_widget(\n            Paragraph::new(vec![\n                Line::from(vec![\n                    label_span(" STATUS   "),\n                    value_span("METRICS OFF", YELLOW),\n                ]),\n                Line::from(vec![\n                    label_span(" LLAMA    "),\n                    Span::styled("restart server with --metrics", Style::default().fg(MUTED)),\n                ]),\n            ]),\n            inner,\n        );\n        return;\n    }\n'''
new_offline = '''    if !llm.connected {\n        let metrics_off = llm_metrics_disabled(&llm.error);\n        let (status, color, message) = if metrics_off {\n            ("METRICS OFF", YELLOW, "restart server with --metrics")\n        } else {\n            ("SERVER OFFLINE", RED, "connection lost · retrying")\n        };\n        frame.render_widget(\n            Paragraph::new(vec![\n                Line::from(vec![label_span(" STATUS   "), value_span(status, color)]),\n                Line::from(vec![\n                    label_span(" LLAMA    "),\n                    Span::styled(message, Style::default().fg(MUTED)),\n                ]),\n            ]),\n            inner,\n        );\n        return;\n    }\n'''
if old_offline not in section:
    raise SystemExit("draw_llm offline block not found")
section = section.replace(old_offline, new_offline, 1)

# Derive stable cumulative detail values for the extra rows.
cache_anchor = '''    let cache_color = if cache_available { CYAN } else { MUTED };\n'''
cache_extra = '''    let cache_share = llm.prompt_cached_total.and_then(|cached| {\n        (llm.prompt_total > 0.0).then_some((cached / llm.prompt_total * 100.0).clamp(0.0, 100.0))\n    });\n    let spec_total_acceptance = (llm.spec_draft_tokens > 0.0).then_some(\n        (llm.spec_accepted_tokens / llm.spec_draft_tokens * 100.0).clamp(0.0, 100.0),\n    );\n'''
if cache_anchor not in section:
    raise SystemExit("cache color anchor not found")
section = section.replace(cache_anchor, cache_anchor + cache_extra, 1)

truncate_anchor = '''    lines.truncate(inner.height as usize);\n'''
extra_rows = '''    // The LLM pane is usually taller than its core metric set because it shares a row\n    // with the system pane. Use that spare vertical space for useful cumulative detail.\n    if inner.height >= 10 {\n        let cache_tokens = llm\n            .prompt_cached_total\n            .map(|value| format!("{} tok", grouped_f64(value)))\n            .unwrap_or_else(|| "—".to_string());\n        let cache_ratio = cache_share\n            .map(|value| format!("{value:.1}% of PP"))\n            .unwrap_or_else(|| "—".to_string());\n        lines.push(Line::from(vec![\n            label_span(" CACHE      "),\n            llm_metric_cell(&cache_tokens, metric_width, cache_color, false),\n            llm_metric_cell(&cache_ratio, metric_width, MUTED, false),\n        ]));\n    }\n\n    if inner.height >= 11 {\n        let draft = if llm.spec_enabled {\n            format!("{} draft", grouped_f64(llm.spec_draft_tokens))\n        } else {\n            "disabled".to_string()\n        };\n        let accepted = if llm.spec_enabled {\n            match spec_total_acceptance {\n                Some(rate) => format!(\n                    "{} accepted · {rate:.1}%",\n                    grouped_f64(llm.spec_accepted_tokens)\n                ),\n                None => format!("{} accepted", grouped_f64(llm.spec_accepted_tokens)),\n            }\n        } else {\n            "—".to_string()\n        };\n        lines.push(Line::from(vec![\n            label_span(" SPEC TOK   "),\n            llm_metric_cell(&draft, metric_width, if llm.spec_enabled { CYAN } else { MUTED }, false),\n            llm_metric_cell(&accepted, metric_width, if llm.spec_enabled { ORK_GREEN } else { MUTED }, false),\n        ]));\n    }\n\n    if inner.height >= 12 {\n        lines.push(Line::from(vec![\n            label_span(" TIME       "),\n            llm_metric_cell(\n                &format!("{:.1} s", llm.prompt_seconds_total),\n                metric_width,\n                MUTED,\n                false,\n            ),\n            llm_metric_cell(\n                &format!("{:.1} s", llm.generation_seconds_total),\n                metric_width,\n                MUTED,\n                false,\n            ),\n        ]));\n    }\n\n'''
if truncate_anchor not in section:
    raise SystemExit("draw_llm truncate anchor not found")
section = section.replace(truncate_anchor, extra_rows + truncate_anchor, 1)

ui = ui[:llm_start] + section + ui[llm_end:]

# Reuse one definition of what a genuine metrics-disabled error looks like.
old_friendly = '''fn friendly_llm_error(error: &str) -> String {\n    let lower = error.to_ascii_lowercase();\n    if lower.contains("501") || lower.contains("--metrics") || lower.contains("metrics endpoint") {\n        "LLAMA METRICS OFF · restart server with --metrics".to_string()\n'''
new_friendly = '''fn llm_metrics_disabled(error: &str) -> bool {\n    let lower = error.to_ascii_lowercase();\n    lower.contains("501") || lower.contains("--metrics") || lower.contains("metrics endpoint")\n}\n\nfn friendly_llm_error(error: &str) -> String {\n    let lower = error.to_ascii_lowercase();\n    if llm_metrics_disabled(error) {\n        "LLAMA METRICS OFF · restart server with --metrics".to_string()\n'''
if old_friendly not in ui:
    raise SystemExit("friendly_llm_error anchor not found")
ui = ui.replace(old_friendly, new_friendly, 1)

ui_path.write_text(ui)
