//! Native adapter for the Strata inference server's JSON `/metrics` API.
//!
//! llama.cpp exposes `/metrics` in the Prometheus text exposition format.
//! Strata answers the same path with HTTP 200 and a JSON object
//! (`engine`, `live`, `requests`, `conversation_cache`, `requests_kept`,
//! `totals`, `hardware`, `hardware_static`, `history`, `time`). Treating that
//! body as Prometheus yields no samples, which OrsikTop used to report as
//! "metrics off" even though the endpoint answered.
//!
//! This module recognises the Strata object and normalises the cumulative
//! counters into the same canonical [`MetricSample`] names the Prometheus path
//! produces, so the existing field mapping, counter-reset handling and
//! live-throughput logic in `llama` are reused unchanged. Only values the
//! verified schema actually provides are mapped; a field Strata does not
//! expose (context high-water mark, active requests, cache tokens) stays
//! absent, and an absent/null metric is never fabricated as `0`.
//!
//! Verified against Strata build `0.1.40`. No field is assumed to be present,
//! the resolver tolerates nesting changes by matching a leaf key or its last
//! two path segments, and arrays (`requests`, `history`) are ignored so
//! per-request / rolling-sample numbers cannot masquerade as aggregates.

use serde_json::Value;

use crate::llama::MetricSample;

/// A top-level group that, together with `totals`, identifies Strata telemetry.
/// Used only for recognition; none is required to be present individually.
const MARKER_GROUPS: &[&str] = &[
    "engine",
    "live",
    "requests_kept",
    "hardware",
    "hardware_static",
    "conversation_cache",
];

/// One canonical metric name and the Strata leaf aliases that may carry it.
struct Canonical {
    /// The llama.cpp-style metric name the existing mapping logic looks up.
    name: &'static str,
    /// Accepted leaf key spellings, most canonical first. Matched against a
    /// leaf's own key or its last two joined path segments.
    aliases: &'static [&'static str],
    /// Multiplier applied to the raw value (for example ms → s).
    scale: f64,
    /// Preferred enclosing group when more than one group could match.
    group: Option<&'static str>,
}

/// Canonical names must match the lookup lists in `llama::LlamaMonitor::sample`.
///
/// Strata's `totals` object holds the cumulative counters; `prompt_ms` /
/// `decode_ms` are accumulated **milliseconds**, so they are scaled to seconds.
/// The same canonical name may appear twice with different units (seconds first
/// so an explicit seconds field wins over a milliseconds field).
const CANONICAL: &[Canonical] = &[
    Canonical {
        name: "llamacpp:prompt_tokens_total",
        aliases: &[
            "prompt_tokens",
            "prompt_tokens_total",
            "total_prompt_tokens",
            "prompt_token_count",
            "prompt_eval_count",
            "prefill_tokens",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:tokens_predicted_total",
        aliases: &[
            "output_tokens",
            "generated_tokens",
            "generated_tokens_total",
            "completion_tokens",
            "predicted_tokens",
            "tokens_predicted",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:prompt_seconds_total",
        aliases: &[
            "prompt_seconds",
            "prompt_seconds_total",
            "prompt_time_seconds",
            "prompt_eval_seconds",
            "prefill_seconds",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:prompt_seconds_total",
        aliases: &["prompt_ms", "prompt_time_ms", "prompt_eval_ms"],
        scale: 0.001,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:tokens_predicted_seconds_total",
        aliases: &[
            "generation_seconds",
            "generation_seconds_total",
            "generation_time_seconds",
            "decode_seconds",
            "predicted_seconds",
            "eval_seconds",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:tokens_predicted_seconds_total",
        aliases: &["decode_ms", "generation_ms", "generation_time_ms"],
        scale: 0.001,
        group: Some("totals"),
    },
    // Strata has no active-request gauge; `queued` is the deferred count.
    Canonical {
        name: "llamacpp:requests_deferred",
        aliases: &[
            "queued",
            "requests_queued",
            "queue_depth",
            "deferred_requests",
            "requests_deferred",
        ],
        scale: 1.0,
        group: Some("live"),
    },
    // Speculative decoding: offered/accepted draft counters give the existing
    // acceptance math (accepted / draft) without inventing a draft *count*.
    Canonical {
        name: "llamacpp:spec_decode_num_draft_tokens_total",
        aliases: &[
            "drafts_offered",
            "draft_tokens_offered",
            "draft_tokens_total",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
    Canonical {
        name: "llamacpp:spec_decode_num_accepted_tokens_total",
        aliases: &[
            "drafts_accepted",
            "accepted_tokens_total",
            "num_accepted_tokens",
        ],
        scale: 1.0,
        group: Some("totals"),
    },
];

/// True when a parsed `/metrics` body is a Strata telemetry object rather than
/// some other JSON payload. Recognition requires the `totals` group plus at
/// least one further Strata marker, so an unrelated JSON object cannot be
/// mistaken for Strata metrics.
pub(crate) fn is_strata_metrics(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    if !map.get("totals").is_some_and(Value::is_object) {
        return false;
    }
    MARKER_GROUPS.iter().any(|group| map.contains_key(*group))
}

/// The `totals.since` epoch-seconds timestamp, used to detect a Strata counter
/// reset (process restart) so a fresh baseline is taken instead of a delta
/// across the gap.
pub(crate) fn totals_since(value: &Value) -> Option<f64> {
    value.get("totals")?.get("since")?.as_f64()
}

/// Normalise a Strata `/metrics` object into canonical `MetricSample`s. The
/// result may be empty when the counters are absent; callers still treat a
/// recognised Strata endpoint as available (see `llama::parse_metrics_body`).
pub(crate) fn extract_samples(value: &Value) -> Vec<MetricSample> {
    let mut leaves = Vec::new();
    let mut path = Vec::new();
    flatten(value, &mut path, &mut leaves);

    let mut samples: Vec<MetricSample> = Vec::new();
    for canonical in CANONICAL {
        if samples.iter().any(|sample| sample.name == canonical.name) {
            continue;
        }
        // Whether the canonical group exists at the top level. When it does,
        // the metric must be found inside it; a cache/live look-alike must not
        // stand in for a missing cumulative counter.
        let group_present = canonical
            .group
            .is_some_and(|group| value.get(group).is_some());
        if let Some(found) = resolve(&leaves, canonical.aliases, canonical.group, group_present) {
            samples.push(MetricSample {
                name: canonical.name.to_string(),
                labels: None,
                value: found.value * canonical.scale,
            });
        }
    }
    samples
}

/// A finite numeric leaf and the normalised path that reached it.
struct Leaf {
    path: Vec<String>,
    value: f64,
}

/// Lower-case ASCII alphanumerics only, so `prompt_tokens`, `promptTokens`
/// and `prompt-tokens` all normalise to `prompttokens`.
fn normalize(segment: &str) -> String {
    segment
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// Collect finite numeric object leaves. Arrays are skipped: Strata's
/// `requests` and `history` arrays hold per-request / rolling-sample numbers
/// that must not be read as aggregate counters.
fn flatten(value: &Value, path: &mut Vec<String>, out: &mut Vec<Leaf>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                path.push(normalize(key));
                flatten(child, path, out);
                path.pop();
            }
        }
        Value::Number(number) => {
            if let Some(found) = number.as_f64() {
                if found.is_finite() {
                    out.push(Leaf {
                        path: path.clone(),
                        value: found,
                    });
                }
            }
        }
        _ => {}
    }
}

/// Resolve the first alias that matches any leaf. Alias order encodes priority,
/// so the most canonical spelling wins.
///
/// When a canonical group is given:
/// - if the group exists at the top level (`group_present`), only a leaf inside
///   it may match — a look-alike in another group is not a substitute, so an
///   absent canonical counter stays absent;
/// - only when the group is entirely absent (a schema without that grouping)
///   does the match fall back to the shallowest leaf anywhere.
///
/// The shallowest path breaks ties so a direct `group.key` wins over a nested
/// `group.sub.key`.
fn resolve<'a>(
    leaves: &'a [Leaf],
    aliases: &[&str],
    group: Option<&str>,
    group_present: bool,
) -> Option<&'a Leaf> {
    for alias in aliases {
        let wanted = normalize(alias);
        if wanted.is_empty() {
            continue;
        }
        let mut matches: Vec<&Leaf> = leaves
            .iter()
            .filter(|leaf| leaf_matches(leaf, &wanted))
            .collect();
        if matches.is_empty() {
            continue;
        }
        if let Some(group) = group {
            let group = normalize(group);
            let mut in_group: Vec<&Leaf> = matches
                .iter()
                .copied()
                .filter(|leaf| leaf.path.contains(&group))
                .collect();
            if !in_group.is_empty() {
                in_group.sort_by_key(|leaf| leaf.path.len());
                return Some(in_group[0]);
            }
            if group_present {
                // The canonical group exists but does not carry this metric:
                // treat it as absent rather than borrowing another group.
                return None;
            }
            // Group absent from this schema: fall through to a flat match.
        }
        matches.sort_by_key(|leaf| leaf.path.len());
        return Some(matches[0]);
    }
    None
}

/// A leaf matches when its own key or its last two joined path segments equal
/// the alias, so both `totals.prompt_tokens` and `totals.prompt.tokens` map to
/// the `prompt_tokens` alias.
fn leaf_matches(leaf: &Leaf, wanted: &str) -> bool {
    let len = leaf.path.len();
    if len == 0 {
        return false;
    }
    if leaf.path[len - 1] == wanted {
        return true;
    }
    len >= 2 && format!("{}{}", leaf.path[len - 2], leaf.path[len - 1]) == wanted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn value_of(samples: &[MetricSample], name: &str) -> Option<f64> {
        samples
            .iter()
            .find(|sample| sample.name == name)
            .map(|sample| sample.value)
    }

    #[test]
    fn recognizes_strata_objects_only() {
        assert!(is_strata_metrics(&json!({
            "totals": {}, "engine": {}, "live": {}, "requests_kept": 3
        })));
        // `totals` alone is not enough, and non-objects never match.
        assert!(!is_strata_metrics(&json!({ "totals": {} })));
        assert!(!is_strata_metrics(&json!({ "engine": "strata" })));
        assert!(!is_strata_metrics(&json!([1, 2, 3])));
        assert!(!is_strata_metrics(&json!("totals")));
    }

    #[test]
    fn maps_the_verified_strata_schema() {
        // Shape captured from Strata 0.1.40 (values synthetic).
        let value = json!({
            "engine": { "model": "qwen3.8", "context": 196608, "spec": 6, "mtp_max": 4 },
            "live": {
                "state": "generating",
                "queued": 2,
                "tok_s": 123.9,
                "tok_s_mean": 130.1,
                "prefill_tok_s_mean": 1027.4
            },
            "requests_kept": 13,
            "totals": {
                "since": 1791570652.70886,
                "requests": 13,
                "prompt_tokens": 160961,
                "reused": 70688,
                "output_tokens": 698,
                "prompt_ms": 43698.2,
                "decode_ms": 5853.0,
                "drafts_offered": 592,
                "drafts_accepted": 479
            },
            "conversation_cache": { "enabled": false, "requests": 13, "prompt_tokens": 160961 },
            "hardware": { "tok_s": 123.9, "tok_s_mean": 130.1, "gpu_util": 41 },
            "history": { "tok_s": [1, 2, 3], "gpu_power": [50.0, null, 56.89] }
        });
        let samples = extract_samples(&value);
        assert_eq!(
            value_of(&samples, "llamacpp:prompt_tokens_total"),
            Some(160961.0)
        );
        assert_eq!(
            value_of(&samples, "llamacpp:tokens_predicted_total"),
            Some(698.0)
        );
        // ms → s.
        assert_eq!(
            value_of(&samples, "llamacpp:prompt_seconds_total"),
            Some(43.6982)
        );
        assert_eq!(
            value_of(&samples, "llamacpp:tokens_predicted_seconds_total"),
            Some(5.853)
        );
        assert_eq!(value_of(&samples, "llamacpp:requests_deferred"), Some(2.0));
        assert_eq!(
            value_of(&samples, "llamacpp:spec_decode_num_draft_tokens_total"),
            Some(592.0)
        );
        assert_eq!(
            value_of(&samples, "llamacpp:spec_decode_num_accepted_tokens_total"),
            Some(479.0)
        );
        // No Strata equivalent: these must stay absent, never fabricated.
        assert_eq!(value_of(&samples, "llamacpp:n_tokens_max"), None);
        assert_eq!(value_of(&samples, "llamacpp:requests_processing"), None);
        assert_eq!(
            value_of(&samples, "llamacpp:prompt_tokens_cached_total"),
            None
        );
        // Hardware / live gauges must not be mapped as averages.
        assert_eq!(
            value_of(&samples, "llamacpp:predicted_tokens_seconds"),
            None
        );
        assert_eq!(value_of(&samples, "llamacpp:prompt_tokens_seconds"), None);
        assert_eq!(
            value_of(&samples, "llamacpp:spec_decode_num_drafts_total"),
            None
        );
    }

    #[test]
    fn totals_since_is_exposed() {
        let value = json!({ "totals": { "since": 1791570652.70886 }, "engine": {} });
        assert_eq!(totals_since(&value), Some(1791570652.70886));
        assert_eq!(totals_since(&json!({ "totals": {} })), None);
        assert_eq!(totals_since(&json!({})), None);
    }

    #[test]
    fn ignores_arrays_and_null_leaves() {
        // `requests`/`history` arrays and null gauges must not be read as
        // aggregate counters; only `totals` counts.
        let value = json!({
            "engine": {},
            "totals": { "prompt_tokens": 10, "output_tokens": 4 },
            "requests": [ { "prompt_tokens": 999, "output_tokens": 888 } ],
            "history": { "tok_s": [1, 2, 3] },
            "live": { "queued": null }
        });
        let samples = extract_samples(&value);
        assert_eq!(
            value_of(&samples, "llamacpp:prompt_tokens_total"),
            Some(10.0)
        );
        assert_eq!(
            value_of(&samples, "llamacpp:tokens_predicted_total"),
            Some(4.0)
        );
        assert_eq!(value_of(&samples, "llamacpp:requests_deferred"), None);
    }

    #[test]
    fn an_explicit_seconds_field_wins_over_milliseconds() {
        let value = json!({
            "engine": {},
            "totals": { "prompt_seconds": 20.0, "prompt_ms": 99999.0 }
        });
        assert_eq!(
            value_of(&extract_samples(&value), "llamacpp:prompt_seconds_total"),
            Some(20.0)
        );
    }

    #[test]
    fn tolerates_split_keys_and_camel_case() {
        let value = json!({
            "engine": {},
            "totals": { "prompt": { "tokens": 7 }, "outputTokens": 3 }
        });
        let samples = extract_samples(&value);
        assert_eq!(
            value_of(&samples, "llamacpp:prompt_tokens_total"),
            Some(7.0)
        );
        assert_eq!(
            value_of(&samples, "llamacpp:tokens_predicted_total"),
            Some(3.0)
        );
    }

    #[test]
    fn prefers_totals_over_other_groups() {
        // `conversation_cache.prompt_tokens` and `live.queued` must not shadow
        // the canonical `totals` counters.
        let value = json!({
            "engine": {},
            "totals": { "prompt_tokens": 500 },
            "conversation_cache": { "prompt_tokens": 3 },
            "live": { "queued": 1 }
        });
        assert_eq!(
            value_of(&extract_samples(&value), "llamacpp:prompt_tokens_total"),
            Some(500.0)
        );
    }

    #[test]
    fn a_missing_totals_counter_is_not_borrowed_from_another_group() {
        // `totals` exists but omits prompt_tokens: a cache/live look-alike must
        // not become the cumulative counter. The metric stays absent.
        let value = json!({
            "engine": {},
            "totals": { "output_tokens": 4 },
            "conversation_cache": { "prompt_tokens": 999 },
            "live": { "prompt_tokens": 7 }
        });
        let samples = extract_samples(&value);
        assert_eq!(value_of(&samples, "llamacpp:prompt_tokens_total"), None);
        assert_eq!(
            value_of(&samples, "llamacpp:tokens_predicted_total"),
            Some(4.0)
        );
    }
}
