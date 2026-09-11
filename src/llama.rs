use std::{
    collections::HashMap,
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
    pub busy_slots: u64,
    pub prompt_total: f64,
    pub generated_total: f64,
    pub prompt_tps: f64,
    pub generation_tps: f64,
    pub prompt_avg_tps: f64,
    pub generation_avg_tps: f64,
    pub active_requests: f64,
    pub deferred_requests: f64,
    pub spec_draft_tokens: f64,
    pub spec_accepted_tokens: f64,
    pub spec_acceptance_pct: f64,
    pub error: String,
}

#[derive(Default)]
struct PreviousCounters {
    at: Option<Instant>,
    prompt_total: f64,
    generated_total: f64,
}

#[derive(Default)]
struct CachedProps {
    model: String,
    context_size: u64,
    last_refresh: Option<Instant>,
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
        stats.generated_total = pick_metric(
            &metrics,
            &[
                "llamacpp:tokens_predicted_total",
                "llamacpp_predicted_tokens_total",
                "predicted_tokens_total",
            ],
        );
        stats.prompt_avg_tps = pick_metric(
            &metrics,
            &["llamacpp:prompt_tokens_seconds", "llamacpp_prompt_tokens_seconds"],
        );
        stats.generation_avg_tps = pick_metric(
            &metrics,
            &[
                "llamacpp:predicted_tokens_seconds",
                "llamacpp_predicted_tokens_seconds",
            ],
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
        stats.spec_draft_tokens = pick_metric(
            &metrics,
            &["llamacpp:spec_decode_num_draft_tokens_total"],
        );
        stats.spec_accepted_tokens = pick_metric(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"],
        );
        if stats.spec_draft_tokens > 0.0 {
            stats.spec_acceptance_pct =
                (stats.spec_accepted_tokens / stats.spec_draft_tokens * 100.0).clamp(0.0, 100.0);
        }

        self.update_live_throughput(&mut stats);
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

        let model = json_string(&props, &["model_name", "model_alias", "model_path"])
            .map(|value| model_display_name(&value));
        if let Some(model) = model.filter(|name| !name.is_empty()) {
            self.props.model = model;
        }
    }

    fn update_live_throughput(&mut self, stats: &mut LlmStats) {
        if let Some(previous_at) = self.previous.at {
            let seconds = previous_at.elapsed().as_secs_f64();
            if seconds > 0.0 {
                stats.prompt_tps =
                    ((stats.prompt_total - self.previous.prompt_total) / seconds).max(0.0);
                stats.generation_tps =
                    ((stats.generated_total - self.previous.generated_total) / seconds).max(0.0);
            }
        }

        self.previous.at = Some(Instant::now());
        self.previous.prompt_total = stats.prompt_total;
        self.previous.generated_total = stats.generated_total;
    }

    fn apply_slots(&self, stats: &mut LlmStats) {
        let Ok(response) = self.client.get(format!("{}/slots", self.base)).send() else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        let Ok(value) = response.json::<Value>() else {
            return;
        };

        apply_slots_json(stats, &value);
    }
}

fn apply_slots_json(stats: &mut LlmStats, value: &Value) {
    let Some(slots) = value.as_array() else {
        return;
    };

    stats.slots_available = true;
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
}

fn slot_decoded_tokens(slot: &Value) -> u64 {
    match slot.get("next_token") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("n_decoded"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        Some(Value::Object(map)) => map
            .get("n_decoded")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn parse_prometheus(input: &str) -> HashMap<String, f64> {
    let mut metrics = HashMap::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(name_with_labels) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            continue;
        };

        let name = name_with_labels.split('{').next().unwrap_or(name_with_labels);
        if let Ok(value) = value.parse::<f64>() {
            *metrics.entry(name.to_string()).or_insert(0.0) += value;
        }
    }

    metrics
}

fn pick_metric(metrics: &HashMap<String, f64>, names: &[&str]) -> f64 {
    names
        .iter()
        .find_map(|name| metrics.get(*name).copied())
        .unwrap_or(0.0)
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(ToOwned::to_owned))
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
    fn parses_current_llama_metrics_and_sums_labels() {
        let metrics = parse_prometheus(
            r#"
# HELP ignored ignored
llamacpp:prompt_tokens_total 120
llamacpp:tokens_predicted_total{slot="0"} 20
llamacpp:tokens_predicted_total{slot="1"} 30
llamacpp:predicted_tokens_seconds 42.5
"#,
        );

        assert_eq!(metrics["llamacpp:prompt_tokens_total"], 120.0);
        assert_eq!(metrics["llamacpp:tokens_predicted_total"], 50.0);
        assert_eq!(metrics["llamacpp:predicted_tokens_seconds"], 42.5);
    }

    #[test]
    fn derives_live_context_from_slots() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        let slots = json!([
            {
                "id": 0,
                "n_ctx": 65536,
                "is_processing": true,
                "n_prompt_tokens": 12345,
                "next_token": [{"n_decoded": 42}]
            },
            {
                "id": 1,
                "n_ctx": 65536,
                "is_processing": false,
                "n_prompt_tokens": 777
            }
        ]);

        apply_slots_json(&mut stats, &slots);

        assert!(stats.slots_available);
        assert_eq!(stats.slot_count, 2);
        assert_eq!(stats.busy_slots, 1);
        assert_eq!(stats.context_size, 65536);
        assert_eq!(stats.context_used, 12345);
    }

    #[test]
    fn slot_fallback_uses_processed_plus_decoded() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "n_ctx": 8192,
            "n_prompt_tokens_processed": 4000,
            "next_token": [{"n_decoded": 250}]
        }]);

        apply_slots_json(&mut stats, &slots);
        assert_eq!(stats.context_used, 4250);
    }

    #[test]
    fn model_path_is_shortened_for_display() {
        assert_eq!(
            model_display_name("/models/Qwen3.8-27B-UD-Q4_K_M.gguf"),
            "Qwen3.8-27B-UD-Q4_K_M"
        );
    }
}
