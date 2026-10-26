use std::time::{Duration, Instant};

use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;

// Extracted submodules (behavior-preserving refactor).
mod http;
mod metrics;
mod monitor;
mod prometheus;
mod props;
mod slots;
mod speculative;
use http::{endpoint_url, fetch_json, fetch_raw, join_or};
use metrics::{
    average_over_seconds, counter_delta_opt, counter_rate, parse_metrics_body, pick_metric_opt,
};
use prometheus::parse_prometheus;
pub(crate) use prometheus::MetricSample;
use props::{json_string, json_u64_path, model_display_name, sanitize_error_text};
use slots::{apply_slots_json, choose_live_throughput, slot_delta_tps};
use speculative::{
    apply_local_spec_fallback, local_speculative_process_config, speculative_config,
};
#[cfg(test)]
use speculative::{
    command_targets_port, is_llama_server_process, is_loopback_host, speculative_from_process,
};
#[cfg(test)]
use std::thread;

const PROPS_REFRESH: Duration = Duration::from_secs(30);
const SPEC_ACCEPTANCE_HOLD: Duration = Duration::from_secs(3);
/// How often the local llama-server process is re-scanned for its
/// spec-decoding config (CLI args + two env vars). The scan reads every
/// `/proc/<pid>/cmdline`, so it is amortized over a window instead of
/// running on every 250 ms sample. The config of a running server process
/// is static for its lifetime, so a 30 s window only costs a re-scan when
/// the server restarts (a new PID) and picks up the new config promptly.
const SPEC_REFRESH: Duration = Duration::from_secs(30);
/// Connect timeout for a single llama.cpp request. Kept well below the total
/// request timeout so a dead or unrouted endpoint fails fast instead of
/// stalling the poll cycle; the connect phase of a local server takes
/// milliseconds.
const LLM_CONNECT_TIMEOUT_MS: u64 = 750;
/// Total (connect + response) timeout for a single llama.cpp request.
const LLM_REQUEST_TIMEOUT_MS: u64 = 1200;
/// Upper bound on a telemetry response body. `/metrics` on a busy server is
/// tens of KB; this cap keeps a hostile or broken endpoint from exhausting
/// memory while leaving ample headroom.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Placeholder shown when `/props` did not report a model name. It is not a
/// real model, so display paths that have a better label (e.g. the endpoint)
/// must treat it as "no model known".
pub(crate) const UNKNOWN_MODEL_LABEL: &str = "llama.cpp model";

#[derive(Clone, Debug, Default)]
pub struct LlmStats {
    pub connected: bool,
    /// True when `/metrics` answered successfully this sample, so the
    /// metric-derived fields below (counters, throughput, request counts,
    /// timings) carry real values. False when `/metrics` failed or is
    /// disabled — even if `/slots` is reachable and `connected` is true. The
    /// slot/context fields stay valid; the metric fields are unavailable
    /// (`—`) rather than a fabricated zero.
    pub metrics_available: bool,
    pub reconnecting: bool,
    pub model: String,
    pub context_size: u64,
    /// Tokens the currently selected slot is using (the slot's own
    /// `n_prompt_tokens`). `None` when `/slots` reports no usable occupancy or
    /// is unavailable: unknown, rendered `—`, never a fabricated `0`. The
    /// historical `/metrics` high-water mark is deliberately not substituted
    /// here (see `context_high_watermark`).
    pub context_used: Option<u64>,
    /// `/metrics` context high-water mark: the lifetime peak occupancy — a
    /// **historical** value, reported separately (labelled `watermark`) and
    /// never presented as the current CTX occupancy. `None` when the metric is
    /// absent or `/metrics` is unavailable, so a missing watermark can never be
    /// mistaken for a real `0`.
    pub context_high_watermark: Option<u64>,
    /// ID of the slot the displayed (context_used, context_size) pair comes
    /// from, per the slot's own `id` field in /slots. `None` when /slots is
    /// unavailable, empty, or the selected slot reports no `id` — never a
    /// guess. Only meaningful when `slots_available`.
    pub context_slot_id: Option<u64>,
    pub slots_available: bool,
    pub slot_count: u64,
    pub props_slot_count: u64,
    pub busy_slots: u64,
    /// Context occupancy of every slot that reports its own `/slots` `id`,
    /// from the very same /slots response that supplies the selected CTX
    /// pair. Ordered by slot ID (never array position); slots without an
    /// `id` field are omitted rather than guessed. `context_used` /
    /// `context_size` stay `None` when the slot reports no usable value —
    /// never a fake zero. Empty when /slots is unavailable.
    pub slot_overview: Vec<LlmSlotInfo>,
    pub slots_error: String,
    /// Monotonic `/metrics` counters and derived rates. Each is `None` when the
    /// underlying metric was absent from an otherwise reachable `/metrics`
    /// payload, so a missing metric is never shown as a real `0`. A reported
    /// `Some(0.0)` is a genuine measured zero.
    pub prompt_total: Option<f64>,
    pub prompt_cached_total: Option<f64>,
    pub generated_total: Option<f64>,
    /// Per-request token counts from the selected `/slots` entry. `None` when
    /// `/slots` is unavailable, so an absent slot is never shown as `0`.
    pub request_prompt_tokens: Option<u64>,
    pub request_generated_tokens: Option<u64>,
    pub prompt_seconds_total: Option<f64>,
    pub generation_seconds_total: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub generation_tps: Option<f64>,
    pub prompt_avg_tps: Option<f64>,
    pub generation_avg_tps: Option<f64>,
    pub active_requests: Option<f64>,
    pub deferred_requests: Option<f64>,
    /// Explicit request phase reported by the server (Strata's `live.state`),
    /// normalized. `None` for llama.cpp and for an absent/unrecognised state,
    /// so the existing activity heuristics apply unchanged.
    pub strata_phase: Option<crate::strata::StrataPhase>,
    /// Backend inferred from this sample's already-parsed `/metrics` wire body:
    /// a recognised Strata JSON object, or a Prometheus exposition that
    /// actually carries `llamacpp` metric names. Anything else stays
    /// [`ServerBackend::Unknown`](crate::domain::ServerBackend::Unknown)
    /// (rendered `—`) — an assumed backend is never reported.
    pub backend: crate::domain::ServerBackend,
    pub spec_drafts_total: Option<f64>,
    pub spec_draft_tokens: Option<f64>,
    pub spec_accepted_tokens: Option<f64>,
    pub spec_enabled: bool,
    pub spec_is_mtp: bool,
    pub spec_n_max: Option<u64>,
    pub spec_acceptance_pct: Option<f64>,
    pub error: String,
}

/// Classify a server's activity phase from its own telemetry using the shared
/// heuristic: a server-reported Strata phase is authoritative, otherwise the
/// live rates and slot activity decide. `Idle` requires positive evidence of
/// no activity — a valid `/slots` array (whose busy count is then real), an
/// `active requests` gauge reporting `0`, or Strata's `live.state = idle`.
/// Without any of these the phase is `Unknown`, never a guessed `Idle`. This
/// never inspects connection state — callers that need to distinguish
/// "offline" from the activity phase use [`activity_phase`]; the
/// selected-server panel is only drawn with phases while connected.
pub(crate) fn classify_phase(llm: &LlmStats) -> crate::domain::ActivityPhase {
    let strata_phase = llm.strata_phase;
    let generating = strata_phase == Some(crate::strata::StrataPhase::Generating)
        || llm.generation_tps.is_some_and(|value| value > 0.05);
    let reading = strata_phase == Some(crate::strata::StrataPhase::Reading)
        || llm.prompt_tps.is_some_and(|value| value > 0.05);
    if generating {
        crate::domain::ActivityPhase::Generating
    } else if reading {
        crate::domain::ActivityPhase::Prefill
    } else if llm.busy_slots > 0 || llm.active_requests.is_some_and(|value| value > 0.0) {
        crate::domain::ActivityPhase::Processing
    } else if llm.deferred_requests.is_some_and(|value| value > 0.0) {
        crate::domain::ActivityPhase::Queued
    } else if strata_phase == Some(crate::strata::StrataPhase::Idle)
        || llm.slots_available
        || llm.active_requests == Some(0.0)
    {
        crate::domain::ActivityPhase::Idle
    } else {
        crate::domain::ActivityPhase::Unknown
    }
}

/// The activity phase shown per server in the overview. `None` means the
/// server has never answered or is currently offline and must render `—`
/// rather than a guessed `IDLE`; a connected server without activity evidence
/// is `Unknown` (see [`classify_phase`]). A server being reconnected keeps an
/// explicit `RECONNECTING` label (held state, not live activity).
pub(crate) fn activity_phase(llm: &LlmStats) -> Option<crate::domain::ActivityPhase> {
    if llm.reconnecting {
        return Some(crate::domain::ActivityPhase::Reconnecting);
    }
    if !llm.connected {
        return None;
    }
    Some(classify_phase(llm))
}

/// The backend a server appears to be, identified positively from the parsed
/// `/metrics` wire body — never assumed. A recognised Strata JSON object is a
/// positive Strata signal (even when it exposes no `live.state`). A Prometheus
/// exposition counts as llama.cpp only when it actually carries `llamacpp`
/// metric names; an unrelated Prometheus endpoint, or a JSON body that is not
/// Strata, stays `Unknown` (rendered `—`).
fn classify_backend(
    format: Option<MetricsFormat>,
    metrics: &[MetricSample],
) -> crate::domain::ServerBackend {
    use crate::domain::ServerBackend;
    match format {
        Some(MetricsFormat::Strata) => ServerBackend::Strata,
        Some(MetricsFormat::Prometheus)
            if metrics
                .iter()
                .any(|sample| sample.name.contains("llamacpp")) =>
        {
            ServerBackend::LlamaCpp
        }
        _ => ServerBackend::Unknown,
    }
}

/// One visible slot in the per-slot context overview: the slot's own
/// `/slots` `id`, its busy state, and its context usage / capacity. Missing
/// values stay `None` instead of becoming fabricated zeros.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlmSlotInfo {
    pub id: u64,
    pub busy: bool,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

#[derive(Default)]
struct PreviousMetricCounters {
    at: Option<Instant>,
    prompt_total: Option<f64>,
    generated_total: Option<f64>,
    draft_total: Option<f64>,
    accepted_total: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotCounter {
    /// The slot's own `id` from /slots, or `None` when the response did not
    /// provide one. Deliberately not the array position: order is not identity.
    slot_id: Option<u64>,
    task_id: Option<i64>,
    /// Cumulative prompt/decode counters for this slot. `None` when the slot
    /// did not expose that counter in this sample, so a temporary absence is
    /// never stored as a `0` baseline (which would fabricate a catch-up spike
    /// when the counter returns). The two dimensions are independent.
    prompt_processed: Option<u64>,
    decoded: Option<u64>,
}

#[derive(Default)]
struct PreviousSlotCounters {
    at: Option<Instant>,
    slots: Vec<SlotCounter>,
}

#[derive(Default)]
struct CachedProps {
    model: String,
    context_size: u64,
    total_slots: u64,
    spec_enabled: bool,
    /// True when the server's response explicitly carried the `speculative`
    /// boolean. Then the server's enabled/disabled decision is authoritative.
    spec_enabled_known: bool,
    spec_is_mtp: bool,
    /// True when a server response explicitly named a speculative type other
    /// than `none`/empty (`draft-mtp`, `ngram`, …). A named type is
    /// authoritative; a type list that names nothing (`none`, empty or
    /// absent) leaves the field to the local process CLI.
    spec_named_type: bool,
    spec_n_max: Option<u64>,
    /// True when the server's response explicitly carried `speculative.n_max`
    /// (including an explicit `0`, which disables it).
    spec_n_max_known: bool,
    last_refresh: Option<Instant>,
}

pub struct LlamaMonitor {
    client: Client,
    base: String,
    previous_metrics: PreviousMetricCounters,
    previous_slots: PreviousSlotCounters,
    props: CachedProps,
    last_spec_acceptance_pct: Option<f64>,
    last_spec_acceptance_at: Option<Instant>,
    /// Set when a /props fetch failed so the next sample retries it. A
    /// failure must not extend the 30 s refresh window; otherwise the first
    /// failed refresh would delay the model/context data for a full window.
    props_dirty: bool,
    /// Cached result of the local llama-server process scan (spec-decoding
    /// config from CLI args + env vars). `Some(config)` is a successful scan;
    /// `None` is a successful scan that found no local server. The scan is
    /// amortized over `SPEC_REFRESH` (tracked by `local_spec_scanned_at`)
    /// instead of running on every 250 ms sample.
    local_spec: Option<LocalSpeculativeConfig>,
    local_spec_scanned_at: Option<Instant>,
    /// Which `/metrics` wire format the endpoint answered with last sample.
    /// A change means the endpoint switched implementation (llama.cpp ↔
    /// Strata), so the counter baseline is reset rather than diffed across
    /// incomparable values.
    last_metrics_format: Option<MetricsFormat>,
    /// Strata `totals.since` from the last sample. A change means the Strata
    /// process restarted its counters, so the baseline is dropped.
    last_strata_since: Option<f64>,
}

/// The outcome of a /metrics fetch: the raw body on success, the non-success
/// status, or the transport/body error that made the endpoint unreachable or
/// unreadable.
enum MetricsOutcome {
    Ok(String),
    Http(StatusCode),
    Body(String),
    Unreachable(String),
}

/// The wire format a successful `/metrics` body was parsed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricsFormat {
    /// llama.cpp Prometheus text exposition.
    Prometheus,
    /// Strata JSON telemetry object.
    Strata,
}

/// A parsed `/metrics` body: the canonical samples plus enough metadata to
/// decide availability and to reset counter baselines on a format/identity
/// change. `json` records that the body was JSON even when it was not a
/// recognised Strata object, so the error message can be format-specific.
struct ParsedMetrics {
    samples: Vec<MetricSample>,
    format: Option<MetricsFormat>,
    strata_since: Option<f64>,
    /// Strata's live phase / authoritative windowed decode rate / active
    /// request progress, when the body was a recognised Strata object.
    strata_live: Option<crate::strata::StrataLive>,
    /// Strata's configured context capacity (`engine.max_context`), used only to
    /// seed the CTX denominator when `/props` did not supply `n_ctx`.
    strata_capacity: Option<u64>,
    json: bool,
}

/// The outcome of a JSON endpoint fetch (/props, /slots): the parsed value on
/// success, the non-success status, a body-read/parse error, or the transport
/// error that made the endpoint unreachable.
pub(crate) enum JsonOutcome {
    Ok(Value),
    Http(StatusCode),
    Invalid(String),
    Unreachable(String),
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct SpeculativeConfig {
    /// True when the payload actually mentions the speculative config, so a
    /// successful response can be treated as authoritative. An explicit
    /// `null` counts as present (it clears the cached state); a payload with
    /// no speculative keys at all does not.
    present: bool,
    enabled: Option<bool>,
    is_mtp: bool,
    n_max: Option<u64>,
    /// True when the payload carried a `speculative.types` string.
    types_known: bool,
    /// True when that string names a real type (`draft-mtp`, `ngram`, …),
    /// as opposed to being absent, empty or just `none`.
    named_type: bool,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalSpeculativeConfig {
    enabled: bool,
    is_mtp: bool,
    n_max: Option<u64>,
}

/// Longest model display string, in `char`s. A server-controlled name is
/// truncated to this so it cannot bloat the TUI or `orsiktop diag` output.
const MAX_MODEL_DISPLAY_CHARS: usize = 120;

#[cfg(test)]
mod tests;
