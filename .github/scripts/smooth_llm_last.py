from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"anchor not found: {label}")
    return text.replace(old, new, 1)

path = Path("src/ui.rs")
text = path.read_text()

text = replace_once(
    text,
    "    llm_last_fresh_at: Option<Instant>,\n    llm_connected_since: Option<Instant>,\n",
    "    llm_last_fresh_at: Option<Instant>,\n    llm_sample_interval_ema_ms: Option<f64>,\n    llm_connected_since: Option<Instant>,\n",
    "sample interval field",
)

text = replace_once(
    text,
    "            llm_last_fresh_at: None,\n            llm_connected_since: None,\n",
    "            llm_last_fresh_at: None,\n            llm_sample_interval_ema_ms: None,\n            llm_connected_since: None,\n",
    "sample interval default",
)

old_observe = '''        if llm.connected && !llm.reconnecting {
            if !self.llm_was_connected {
                self.llm_connected_since = Some(now);
                self.llm_connected_flash_until = Some(now + Duration::from_secs(2));
            }
            self.llm_was_connected = true;
            self.llm_last_fresh_at = Some(now);
            push_metric_history_at(&mut self.llm_prefill_history, llm.prompt_tps, now);
            push_metric_history_at(&mut self.llm_decode_history, llm.generation_tps, now);
        } else if !llm.connected {
'''
new_observe = '''        if llm.connected && !llm.reconnecting {
            if !self.llm_was_connected {
                self.llm_connected_since = Some(now);
                self.llm_connected_flash_until = Some(now + Duration::from_secs(2));
                self.llm_sample_interval_ema_ms = None;
            } else if let Some(previous) = self.llm_last_fresh_at {
                let interval_ms = now.saturating_duration_since(previous).as_secs_f64() * 1_000.0;
                self.llm_sample_interval_ema_ms = smooth_llm_sample_interval(
                    self.llm_sample_interval_ema_ms,
                    interval_ms,
                );
            }
            self.llm_was_connected = true;
            self.llm_last_fresh_at = Some(now);
            push_metric_history_at(&mut self.llm_prefill_history, llm.prompt_tps, now);
            push_metric_history_at(&mut self.llm_decode_history, llm.generation_tps, now);
        } else if !llm.connected {
'''
text = replace_once(text, old_observe, new_observe, "observe LLM sample smoothing")

text = replace_once(
    text,
    "        self.llm_last_fresh_at = None;\n        self.llm_connected_since = None;\n",
    "        self.llm_last_fresh_at = None;\n        self.llm_sample_interval_ema_ms = None;\n        self.llm_connected_since = None;\n",
    "reset sample interval",
)

text = replace_once(
    text,
    "                    value_span(&llm_last_sample_text(state), MUTED),\n",
    "                    value_span(&llm_last_sample_text(state, llm), MUTED),\n",
    "offline LAST display",
)

old_link = '''        Line::from(vec![
            label_span(" LINK       "),
            value_span(llm_link_status(state, llm).0, llm_link_status(state, llm).1),
            llm_sep(),
            label_span("LAST "),
            value_span(&llm_last_sample_text(state), MUTED),
            llm_sep(),
            label_span("UPTIME "),
            value_span(&llm_uptime_text(state), MUTED),
        ]),
'''
new_link = '''        Line::from(vec![
            label_span(" LINK       "),
            value_span(llm_link_status(state, llm).0, llm_link_status(state, llm).1),
            llm_sep(),
            label_span("UPTIME "),
            value_span(&llm_uptime_text(state), MUTED),
            llm_sep(),
            label_span("LAST "),
            value_span(&llm_last_sample_text(state, llm), MUTED),
        ]),
'''
text = replace_once(text, old_link, new_link, "reorder UPTIME and LAST")

old_last = '''fn llm_last_sample_text(state: &UiState) -> String {
    state
        .llm_last_fresh_at
        .map(|at| format_sample_age(Instant::now().saturating_duration_since(at)))
        .unwrap_or_else(|| "—".to_string())
}
'''
new_last = '''fn llm_last_sample_text(state: &UiState, llm: &LlmStats) -> String {
    if llm.connected && !llm.reconnecting {
        if let Some(avg_ms) = state.llm_sample_interval_ema_ms {
            return format!("~{avg_ms:.0} ms avg");
        }
    }

    state
        .llm_last_fresh_at
        .map(|at| format!("{} ago", format_sample_age(Instant::now().saturating_duration_since(at))))
        .unwrap_or_else(|| "—".to_string())
}

fn smooth_llm_sample_interval(previous_ms: Option<f64>, current_ms: f64) -> Option<f64> {
    const ALPHA: f64 = 0.2;
    if !current_ms.is_finite() || current_ms <= 0.0 {
        return previous_ms;
    }
    let current_ms = current_ms.clamp(1.0, 60_000.0);
    Some(match previous_ms.filter(|value| value.is_finite() && *value > 0.0) {
        Some(previous) => previous * (1.0 - ALPHA) + current_ms * ALPHA,
        None => current_ms,
    })
}
'''
text = replace_once(text, old_last, new_last, "smoothed LAST helper")

# Add deterministic tests near the existing LLM phase test.
anchor = '''    #[test]
    fn llm_phase_marks_idle_without_activity() {
'''
tests = '''    #[test]
    fn llm_sample_interval_ema_smooths_refresh_jitter() {
        let mut average = None;
        for sample in [100.0, 130.0, 70.0, 115.0, 85.0] {
            average = smooth_llm_sample_interval(average, sample);
        }
        let average = average.unwrap();
        assert!((95.0..=105.0).contains(&average));
    }

    #[test]
    fn llm_last_uses_smoothed_interval_while_online() {
        let state = UiState {
            llm_sample_interval_ema_ms: Some(101.4),
            llm_last_fresh_at: Some(Instant::now()),
            ..UiState::default()
        };
        let llm = LlmStats {
            connected: true,
            ..LlmStats::default()
        };
        assert_eq!(llm_last_sample_text(&state, &llm), "~101 ms avg");
    }

''' + anchor
text = replace_once(text, anchor, tests, "LLM smoothing tests")

path.write_text(text)
