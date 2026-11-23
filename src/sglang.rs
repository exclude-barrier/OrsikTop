//! Native adapter for SGLang's Prometheus `/metrics` exposition.
//!
//! SGLang answers `/metrics` in the Prometheus text format, but with its own
//! metric names and labels (`sglang:gen_throughput`,
//! `sglang:prefill_effective_tokens_total{mode="input"}`,
//! `sglang:num_running_reqs`, ...). The probe in `llama::monitor` reads a fixed
//! set of canonical names, so this module normalises the SGLang series into
//! those names instead of duplicating the mapping logic.
//!
//! Only values the exposition actually provides are mapped; a field SGLang does
//! not expose stays absent (rendered as an em dash), never a fabricated zero.
//! Labels are dropped on the mapped samples because the shared lookup
//! (`pick_metric_opt`) only matches unlabelled samples - duplicated canonical
//! names (e.g. cache sources) are summed into one series.
//!
//! Speculative decoding is folded in from SGLang's round-based reporting:
//! `spec_verify_calls_total` is a true counter and the accept/draft gauges are
//! per-round averages, so the llama-style counters are synthesised as
//! `drafts = calls`, `draft_tokens = calls * num_draft_tokens`,
//! `accepted = calls * accept_length`. These advance monotonically between
//! config changes, which is exactly what the counter-delta path expects.
//!
//! Deliberately *not* mapped:
//! * `sglang:generation_tokens_total` - advances only when a request finishes
//!   (`metrics_collector.observe_one_finished_request`), so a counter delta is
//!   not a live decode rate. The instantaneous rate comes from the
//!   `sglang:gen_throughput` gauge (mapped to `llamacpp:predicted_tokens_seconds`),
//!   which `monitor` promotes to the live rate while requests are running.
//! * per-slot state - SGLang has no `/slots` endpoint; concurrency is covered
//!   by the running/queued request gauges instead.

use crate::llama::MetricSample;

/// SGLang's metric namespace. A `/metrics` body carrying any `sglang:` series
/// is a positive SGLang signal; nothing else is assumed from its shape.
const PREFIX: &str = "sglang:";

/// True when the *raw* exposition carries at least one `sglang:` series.
pub(crate) fn looks_like_sglang(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(PREFIX))
}

/// Model name carried in SGLang's metric labels (`model_name="..."`). Used only
/// when `/props` is absent (SGLang has no such endpoint) so the panel shows the
/// served model instead of the generic llama.cpp fallback. Control characters
/// are stripped: the label is server-provided text rendered in the terminal.
pub(crate) fn model_name(text: &str) -> Option<String> {
    const KEY: &str = "model_name=\"";
    for line in text.lines() {
        if !line.starts_with(PREFIX) {
            continue;
        }
        let Some(start) = line.find(KEY) else {
            continue;
        };
        let rest = &line[start + KEY.len()..];
        let Some(end) = rest.find('"') else { continue };
        let name: String = rest[..end].chars().filter(|c| !c.is_control()).collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Configured context length from SGLang's `sglang:context_len` gauge, used to
/// seed the CTX denominator the way Strata's engine capacity does (SGLang has
/// no `/props`). Capacity only - never occupancy.
pub(crate) fn context_capacity(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("sglang:context_len") else {
            continue;
        };
        if rest.starts_with('_') {
            continue;
        }
        if let Some(tok) = rest.split_whitespace().next_back() {
            if let Ok(value) = tok.parse::<f64>() {
                if value > 0.0 && value.is_finite() {
                    return Some(value as u64);
                }
            }
        }
    }
    None
}

fn canonical(name: &str) -> Option<&'static str> {
    Some(match name {
        "sglang:gen_throughput" => "llamacpp:predicted_tokens_seconds",
        "sglang:num_running_reqs" => "llamacpp:requests_processing",
        "sglang:num_queue_reqs" => "llamacpp:requests_deferred",
        "sglang:max_total_num_tokens" => "llamacpp:n_tokens_max",
        "sglang:cached_tokens_total" => "llamacpp:prompt_tokens_cached_total",
        _ => return None,
    })
}

/// Rename SGLang series into the canonical names the shared probe reads,
/// synthesise the speculative counters, and drop unmapped `sglang:*` series.
/// Non-SGLang samples pass through untouched.
pub(crate) fn normalize(samples: &mut Vec<MetricSample>) {
    let mut out: Vec<MetricSample> = Vec::with_capacity(samples.len());
    let mut was_sglang = false;
    let mut spec_calls: Option<f64> = None;
    let mut spec_accept_len: Option<f64> = None;
    let mut spec_draft_len: Option<f64> = None;
    for sample in samples.drain(..) {
        if sample.name.starts_with(PREFIX) {
            was_sglang = true;
        }
        match sample.name.as_str() {
            "sglang:spec_verify_calls_total" => spec_calls = Some(sample.value),
            "sglang:spec_accept_length" => spec_accept_len = Some(sample.value),
            "sglang:spec_num_draft_tokens" => spec_draft_len = Some(sample.value),
            "sglang:prefill_effective_tokens_total" => {
                // New (non-cached) prompt tokens only: collapse to the
                // mode="input" series, which is the real prefill counter.
                if sample
                    .labels
                    .as_deref()
                    .is_some_and(|labels| labels.contains("mode=\"input\""))
                {
                    out.push(MetricSample {
                        name: "llamacpp:prompt_tokens_total".to_string(),
                        labels: None,
                        value: sample.value,
                    });
                }
            }
            _ => match canonical(&sample.name) {
                Some(name) => out.push(MetricSample {
                    name: name.to_string(),
                    labels: None,
                    value: sample.value,
                }),
                None if sample.name.starts_with(PREFIX) => {}
                None => out.push(sample),
            },
        }
    }
    if was_sglang {
        // Collapse duplicated canonical names (e.g. cache series per source).
        let mut merged: Vec<MetricSample> = Vec::with_capacity(out.len());
        for sample in out.drain(..) {
            match merged.iter_mut().find(|e| e.name == sample.name) {
                Some(existing) => existing.value += sample.value,
                None => merged.push(sample),
            }
        }
        out = merged;
        // Synthesise the llama-style speculative counters (see module docs).
        if let Some(calls) = spec_calls {
            out.push(MetricSample {
                name: "llamacpp:spec_decode_num_drafts_total".to_string(),
                labels: None,
                value: calls,
            });
            if let Some(draft_len) = spec_draft_len {
                out.push(MetricSample {
                    name: "llamacpp:spec_decode_num_draft_tokens_total".to_string(),
                    labels: None,
                    value: calls * draft_len,
                });
            }
            if let Some(accept_len) = spec_accept_len {
                out.push(MetricSample {
                    name: "llamacpp:spec_decode_num_accepted_tokens_total".to_string(),
                    labels: None,
                    value: calls * accept_len,
                });
            }
        }
    }
    *samples = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, labels: Option<&str>, value: f64) -> MetricSample {
        MetricSample {
            name: name.to_string(),
            labels: labels.map(str::to_string),
            value,
        }
    }

    #[test]
    fn detects_sglang_exposition() {
        assert!(looks_like_sglang("# HELP x\nsglang:gen_throughput 12.5\n"));
        assert!(!looks_like_sglang(
            "# HELP x\nllamacpp:predicted_tokens_seconds 12.5\n"
        ));
    }

    #[test]
    fn maps_rates_queues_and_capacity() {
        let mut samples = vec![
            sample("sglang:gen_throughput", Some("model_name=\"m\""), 148.0),
            sample(
                "sglang:prefill_effective_tokens_total",
                Some("mode=\"input\",model_name=\"m\""),
                1_000.0,
            ),
            sample(
                "sglang:prefill_effective_tokens_total",
                Some("mode=\"device_hit\",model_name=\"m\""),
                500.0,
            ),
            sample("sglang:num_running_reqs", Some("model_name=\"m\""), 1.0),
            sample("sglang:num_queue_reqs", Some("model_name=\"m\""), 2.0),
            sample("sglang:max_total_num_tokens", None, 529_984.0),
            sample("sglang:generation_tokens_total", None, 9_352.0),
        ];
        normalize(&mut samples);

        let names: Vec<&str> = samples.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"llamacpp:predicted_tokens_seconds"));
        assert!(names.contains(&"llamacpp:prompt_tokens_total"));
        assert!(names.contains(&"llamacpp:requests_processing"));
        assert!(names.contains(&"llamacpp:requests_deferred"));
        assert!(names.contains(&"llamacpp:n_tokens_max"));
        assert!(!names.iter().any(|name| name.starts_with(PREFIX)));
        assert!(samples.iter().all(|sample| sample.labels.is_none()));
        let prompt = samples
            .iter()
            .filter(|sample| sample.name == "llamacpp:prompt_tokens_total")
            .count();
        assert_eq!(prompt, 1);
    }

    #[test]
    fn sums_cache_sources_and_synthesises_spec_counters() {
        let mut samples = vec![
            sample(
                "sglang:cached_tokens_total",
                Some("cache_source=\"device\""),
                300.0,
            ),
            sample(
                "sglang:cached_tokens_total",
                Some("cache_source=\"host\""),
                100.0,
            ),
            sample(
                "sglang:spec_verify_calls_total",
                Some("model_name=\"m\""),
                50.0,
            ),
            sample("sglang:spec_accept_length", None, 3.0),
            sample("sglang:spec_num_draft_tokens", None, 4.0),
        ];
        normalize(&mut samples);
        let get = |n: &str| samples.iter().find(|s| s.name == n).map(|s| s.value);
        assert_eq!(get("llamacpp:prompt_tokens_cached_total"), Some(400.0));
        assert_eq!(get("llamacpp:spec_decode_num_drafts_total"), Some(50.0));
        assert_eq!(
            get("llamacpp:spec_decode_num_draft_tokens_total"),
            Some(200.0)
        );
        assert_eq!(
            get("llamacpp:spec_decode_num_accepted_tokens_total"),
            Some(150.0)
        );
    }

    #[test]
    fn reads_the_model_label() {
        let body = "# HELP x\nsglang:gen_throughput{engine_type=\"unified\",model_name=\"qwen3.8-flash-next-gptq-nvfp4\"} 1.0\n";
        assert_eq!(
            model_name(body).as_deref(),
            Some("qwen3.8-flash-next-gptq-nvfp4")
        );
        assert_eq!(model_name("llamacpp:x 1\n"), None);
    }

    #[test]
    fn reads_the_context_capacity() {
        let body = "sglang:context_len{engine_type=\"unified\"} 524288.0\n";
        assert_eq!(context_capacity(body), Some(524288));
        assert_eq!(
            context_capacity("sglang:context_len_seconds_count 3\n"),
            None
        );
    }

    #[test]
    fn passes_unknown_metrics_through() {
        let mut samples = vec![sample("other:metric", None, 1.0)];
        normalize(&mut samples);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "other:metric");
    }
}
