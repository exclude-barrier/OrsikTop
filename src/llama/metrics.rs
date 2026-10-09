//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use super::*;

/// Dispatch a `/metrics` body by its actual shape instead of assuming
/// Prometheus. A body that starts with `{`/`[` is JSON: it is parsed once and
/// accepted only when it is a recognised Strata object, so pretty-printed JSON
/// can never reach the line-oriented Prometheus parser (which would otherwise
/// manufacture bogus samples from `"key": value` lines). Everything else is
/// treated as Prometheus text.
pub(super) fn parse_metrics_body(text: &str) -> ParsedMetrics {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) if crate::strata::is_strata_metrics(&value) => ParsedMetrics {
                samples: crate::strata::extract_samples(&value),
                format: Some(MetricsFormat::Strata),
                strata_since: crate::strata::totals_since(&value),
                strata_live: Some(crate::strata::live_rates(&value)),
                strata_capacity: crate::strata::engine_capacity(&value),
                json: true,
            },
            _ => ParsedMetrics {
                samples: Vec::new(),
                format: None,
                strata_since: None,
                strata_live: None,
                strata_capacity: None,
                json: true,
            },
        }
    } else {
        ParsedMetrics {
            samples: parse_prometheus(text),
            format: Some(MetricsFormat::Prometheus),
            strata_since: None,
            strata_live: None,
            strata_capacity: None,
            json: false,
        }
    }
}

pub(super) fn pick_metric_opt(metrics: &[MetricSample], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        metrics
            .iter()
            .find(|sample| sample.name == **name && sample.labels.is_none())
            .map(|sample| sample.value)
    })
}

/// Lifetime average rate from a monotonic token counter and a seconds counter.
/// `None` when either is absent or the denominator is not positive.
pub(super) fn average_over_seconds(tokens: Option<f64>, seconds: Option<f64>) -> Option<f64> {
    let tokens = tokens?;
    let seconds = seconds?;
    (tokens.is_finite() && seconds.is_finite() && seconds > 0.0).then(|| tokens / seconds)
}

/// Delta of a monotonic counter. `None` when a value is non-finite or the
/// counter decreased (reset): unavailable for that sample, never a fake `0`.
pub(super) fn counter_delta_opt(current: f64, previous: f64) -> Option<f64> {
    if current.is_finite() && previous.is_finite() && current >= previous {
        Some(current - previous)
    } else {
        None
    }
}

/// Live throughput from a counter pair over `seconds`. `None` when either value
/// is absent (also covers a reset via `counter_delta_opt`).
pub(super) fn counter_rate(
    current: Option<f64>,
    previous: Option<f64>,
    seconds: f64,
) -> Option<f64> {
    let (current, previous) = (current?, previous?);
    counter_delta_opt(current, previous).map(|delta| delta / seconds)
}
