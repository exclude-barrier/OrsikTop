use std::{
    path::Path,
    time::{Duration, Instant},
};

use reqwest::blocking::Client;
use serde_json::Value;

const PROPS_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct LlmStats {
    pub connected: bool,
    pub model: String,
    pub context_size: u64,
    pub context_used: u64,
    pub context_high_watermark: u64,
    pub slots_available: bool,
    pub slot_count: u64,
    pub props_slot_count: u64,
    pub busy_slots: u64,
    pub slots_error: String,
    pub prompt_total: f64,
    pub prompt_cached_total: f64,
    pub generated_total: f64,
    pub prompt_seconds_total: f64,
    pub generation_seconds_total: f64,
    pub prompt_tps: f64,
    pub generation_tps: f64,
    pub prompt_avg_tps: f64,
    pub generation_avg_tps: f64,
    pub active_requests: f64,
    pub deferred_requests: f64,
    pub spec_drafts_total: f64,
    pub spec_draft_tokens: f64,
    pub spec_accepted_tokens: f64,
    pub spec_acceptance_pct: Option<f64>,
    pub error: String,
}

#[derive(Default)]
struct PreviousCounters {
    at: Option<Instant>,
    prompt_total: f64,
    generated_total: f64,
    draft_total: f64,
    accepted_total: f64,
}

#[derive(Default)]
struct CachedProps {
    model: String,
    context_size: u64,
    total_slots: u64,
    last_refresh: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricSample {
    pub name: String,
    pub labels: Option<String>,
    pub value: f64,
}

pub struct LlamaMonitor {
    client: Client,
    base: String,
    previous: PreviousCounters,
    props: CachedProps,
}

impl LlamaMonitor {
    pub fn new(server: &str) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .timeout(Duration::from_millis(700))
            .build()?;

        Ok(Self {
            client,
            base: server.trim_end_matches('/').to_string(),
            previous: PreviousCounters::default(),
            props: CachedProps::default(),
        })
    }

    pub fn sample(&mut self) -> LlmStats {
        let mut stats = LlmStats::default();

        if self.props_needs_refresh() {
            self.refresh_props();
        }
        stats.model = self.props.model.clone();
        stats.context_size = self.props.context_size;
        stats.props_slot_count = self.props.total_slots;

        let metrics_text = match self.client.get(format!("{}/metrics", self.base)).send() {
            Ok(response) if response.status().is_success() => match response.text() {
                Ok(text) => text,
                Err(err) => {
                    stats.error = format!("metrics response error: {err}");
                    return stats;
                }
            },
            Ok(response) => {
                stats.error = if response.status().as_u16() == 501 {
                    "/metrics disabled; start llama.cpp with --metrics".to_string()
                } else {
                    format!("/metrics returned HTTP {}", response.status())
                };
                return stats;
            }
            Err(err) => {
                stats.error = format!("cannot reach llama.cpp: {err}");
                return stats;
            }
        };

        stats.connected = true;
        let metrics = parse_prometheus(&metrics_text);

        // Current llama.cpp names first, older names retained as compatibility fallbacks.
        stats.prompt_total = pick_metric(
            &metrics,
            &[
                "llamacpp:prompt_tokens_total",
                "llamacpp:tokens_evaluated_total",
                "llamacpp_prompt_tokens_total",
                "prompt_tokens_total",
            ],
        );
        stats.prompt_cached_total = pick_metric(&metrics, &["llamacpp:prompt_tokens_cached_total"]);
        stats.generated_total = pick_metric(
            &metrics,
            &[
                "llamacpp:tokens_predicted_total",
                "llamacpp_predicted_tokens_total",
                "predicted_tokens_total",
            ],
        );
        stats.prompt_seconds_total = pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]);
        stats.generation_seconds_total =
            pick_metric(&metrics, &["llamacpp:tokens_predicted_seconds_total"]);

        // Prefer monotonic token/time counters. The legacy throughput gauges have had
        // upstream regressions where they report 0 while inference is active.
        stats.prompt_avg_tps = safe_ratio(stats.prompt_total, stats.prompt_seconds_total)
            .unwrap_or_else(|| {
                pick_metric(
                    &metrics,
                    &[
                        "llamacpp:prompt_tokens_seconds",
                        "llamacpp_prompt_tokens_seconds",
                    ],
                )
            });
        stats.generation_avg_tps =
            safe_ratio(stats.generated_total, stats.generation_seconds_total).unwrap_or_else(
                || {
                    pick_metric(
                        &metrics,
                        &[
                            "llamacpp:predicted_tokens_seconds",
                            "llamacpp_predicted_tokens_seconds",
                        ],
                    )
                },
            );
        stats.active_requests = pick_metric(
            &metrics,
            &[
                "llamacpp:requests_processing",
                "llamacpp_requests_processing",
                "requests_processing",
            ],
        );
        stats.deferred_requests = pick_metric(
            &metrics,
            &[
                "llamacpp:requests_deferred",
                "llamacpp_requests_deferred",
                "requests_deferred",
            ],
        );
        stats.context_high_watermark = pick_metric(
            &metrics,
            &["llamacpp:n_tokens_max", "llamacpp_n_tokens_max"],
        ) as u64;
        stats.spec_drafts_total = pick_metric(&metrics, &["llamacpp:spec_decode_num_drafts_total"]);
        stats.spec_draft_tokens =
            pick_metric(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]);
        stats.spec_accepted_tokens = pick_metric(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"],
        );

        self.update_live_counters(&mut stats);
        self.apply_slots(&mut stats);

        if stats.model.is_empty() {
            stats.model = "llama.cpp model".to_string();
        }

        stats
    }

    fn props_needs_refresh(&self) -> bool {
        self.props
            .last_refresh
            .map(|at| at.elapsed() >= PROPS_REFRESH)
            .unwrap_or(true)
    }

    fn refresh_props(&mut self) {
        self.props.last_refresh = Some(Instant::now());

        let Ok(response) = self.client.get(format!("{}/props", self.base)).send() else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        let Ok(props) = response.json::<Value>() else {
            return;
        };

        self.props.context_size = json_u64_path(&props, &["default_generation_settings", "n_ctx"])
            .or_else(|| json_u64_path(&props, &["n_ctx"]))
            .unwrap_or(self.props.context_size);
        self.props.total_slots = props
            .get("total_slots")
            .and_then(Value::as_u64)
            .unwrap_or(self.props.total_slots);

        let model = json_string(&props, &["model_name", "model_alias", "model_path"])
            .map(|value| model_display_name(&value));
        if let Some(model) = model.filter(|name| !name.is_empty()) {
            self.props.model = model;
        }
    }

    fn update_live_counters(&mut self, stats: &mut LlmStats) {
        if let Some(previous_at) = self.previous.at {
            let seconds = previous_at.elapsed().as_secs_f64();
            if seconds > 0.0 {
                stats.prompt_tps =
                    counter_delta(stats.prompt_total, self.previous.prompt_total) / seconds;
                stats.generation_tps =
                    counter_delta(stats.generated_total, self.previous.generated_total) / seconds;
            }

            let draft_delta = counter_delta(stats.spec_draft_tokens, self.previous.draft_total);
            if draft_delta > 0.0 {
                let accepted_delta =
                    counter_delta(stats.spec_accepted_tokens, self.previous.accepted_total);
                stats.spec_acceptance_pct =
                    Some((accepted_delta / draft_delta * 100.0).clamp(0.0, 100.0));
            }
        }

        self.previous.at = Some(Instant::now());
        self.previous.prompt_total = stats.prompt_total;
        self.previous.generated_total = stats.generated_total;
        self.previous.draft_total = stats.spec_draft_tokens;
        self.previous.accepted_total = stats.spec_accepted_tokens;
    }

    fn apply_slots(&self, stats: &mut LlmStats) {
        let response = match self.client.get(format!("{}/slots", self.base)).send() {
            Ok(response) => response,
            Err(err) => {
                stats.slots_error = format!("cannot reach /slots: {err}");
                return;
            }
        };

        if !response.status().is_success() {
            stats.slots_error = format!("/slots returned HTTP {}", response.status());
            return;
        }

        let value = match response.json::<Value>() {
            Ok(value) => value,
            Err(err) => {
                stats.slots_error = format!("invalid /slots response: {err}");
                return;
            }
        };

        if let Err(err) = apply_slots_json(stats, &value) {
            stats.slots_error = err;
        }
    }
}

fn apply_slots_json(stats: &mut LlmStats, value: &Value) -> Result<(), String> {
    let slots = value
        .as_array()
        .ok_or_else(|| "/slots response is not an array".to_string())?;

    stats.slots_available = true;
    stats.slots_error.clear();
    stats.slot_count = slots.len() as u64;

    let mut max_context_size = stats.context_size;
    let mut max_context_used = 0u64;
    let mut busy = 0u64;

    for slot in slots {
        let n_ctx = slot.get("n_ctx").and_then(Value::as_u64).unwrap_or(0);
        max_context_size = max_context_size.max(n_ctx);

        if slot
            .get("is_processing")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            busy += 1;
        }

        let prompt_tokens = slot
            .get("n_prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let prompt_processed = slot
            .get("n_prompt_tokens_processed")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let decoded = slot_decoded_tokens(slot);

        // Current llama.cpp exposes n_prompt_tokens as the slot's current prompt/context
        // token count. Older responses may only expose processed prompt + decoded tokens.
        let used = if prompt_tokens > 0 {
            prompt_tokens
        } else {
            prompt_processed.saturating_add(decoded)
        };
        max_context_used = max_context_used.max(used);
    }

    stats.busy_slots = busy;
    stats.context_size = max_context_size;
    stats.context_used = max_context_used;
    Ok(())
}

fn slot_decoded_tokens(slot: &Value) -> u64 {
    match slot.get("next_token") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("n_decoded"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        Some(Value::Object(map)) => map.get("n_decoded").and_then(Value::as_u64).unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn parse_prometheus(input: &str) -> Vec<MetricSample> {
    let mut metrics = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(name_with_labels) = parts.next() else {
            continue;
        };
        let Some(raw_value) = parts.next() else {
            continue;
        };
        let Ok(value) = raw_value.parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }

        let (name, labels) = match name_with_labels.split_once('{') {
            Some((name, raw_labels)) => {
                let labels = raw_labels.trim_end_matches('}');
                let labels = (!labels.is_empty()).then(|| labels.to_string());
                (name, labels)
            }
            None => (name_with_labels, None),
        };

        metrics.push(MetricSample {
            name: name.to_string(),
            labels,
            value,
        });
    }

    metrics
}

fn pick_metric(metrics: &[MetricSample], names: &[&str]) -> f64 {
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

fn safe_ratio(value: f64, seconds: f64) -> Option<f64> {
    (value.is_finite() && seconds.is_finite() && seconds > 0.0).then(|| value / seconds)
}

fn counter_delta(current: f64, previous: f64) -> f64 {
    if current.is_finite() && previous.is_finite() && current >= previous {
        current - previous
    } else {
        0.0
    }
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn json_u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

fn model_display_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .trim_end_matches(".gguf")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_current_llama_metrics_fixture() {
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_current.prom"));

        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_tokens_total"]),
            12000.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_tokens_cached_total"]),
            26000.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]),
            12.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:tokens_predicted_seconds_total"]),
            20.0
        );
        assert_eq!(
            safe_ratio(
                pick_metric(&metrics, &["llamacpp:prompt_tokens_total"]),
                pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]),
            ),
            Some(1000.0)
        );
    }

    #[test]
    fn speculative_fixture_preserves_position_labels() {
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_speculative.prom"));
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]),
            200.0
        );
        assert_eq!(
            pick_metric(
                &metrics,
                &["llamacpp:spec_decode_num_accepted_tokens_total"]
            ),
            140.0
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
        assert_eq!(pick_metric(&metrics, &["llamacpp:test"]), 0.0);
        assert!(!metrics.iter().any(|metric| metric.name == "not_finite"));
    }

    #[test]
    fn derives_average_throughput_from_time_counters() {
        assert_eq!(safe_ratio(1200.0, 2.0), Some(600.0));
        assert_eq!(safe_ratio(1200.0, 0.0), None);
    }

    #[test]
    fn counter_delta_handles_counter_reset() {
        assert_eq!(counter_delta(150.0, 100.0), 50.0);
        assert_eq!(counter_delta(10.0, 100.0), 0.0);
    }

    #[test]
    fn derives_live_context_from_slots_fixture() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_active.json")).unwrap();

        apply_slots_json(&mut stats, &slots).unwrap();

        assert!(stats.slots_available);
        assert_eq!(stats.slot_count, 2);
        assert_eq!(stats.busy_slots, 1);
        assert_eq!(stats.context_size, 196608);
        assert_eq!(stats.context_used, 38779);
    }

    #[test]
    fn idle_slots_fixture_reports_no_busy_slots() {
        let mut stats = LlmStats::default();
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_idle.json")).unwrap();
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.slot_count, 1);
        assert_eq!(stats.busy_slots, 0);
        assert_eq!(stats.context_used, 0);
    }

    #[test]
    fn props_fixture_exposes_context_slots_and_model() {
        let props: Value =
            serde_json::from_str(include_str!("../tests/fixtures/props.json")).unwrap();
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
    fn slot_fallback_uses_processed_plus_decoded() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "n_ctx": 8192,
            "n_prompt_tokens_processed": 4000,
            "next_token": [{"n_decoded": 250}]
        }]);

        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 4250);
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
}
