use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;

// Extracted submodules (behavior-preserving refactor).
mod http;
mod metrics;
mod props;
mod slots;
mod speculative;
#[allow(unused_imports)]
use http::*;
#[allow(unused_imports)]
use metrics::*;
#[allow(unused_imports)]
use props::*;
#[allow(unused_imports)]
use slots::*;
#[allow(unused_imports)]
use speculative::*;

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
/// live rates and slot activity decide. This never inspects connection state —
/// callers that need to distinguish "offline" from "idle" use
/// [`activity_phase`]; the selected-server panel uses it directly (an offline
/// server keeps its last-known idle presentation there).
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
    } else {
        crate::domain::ActivityPhase::Idle
    }
}

/// The activity phase shown per server in the overview. `None` means there is
/// no reliable evidence — a server that has never answered, or is currently
/// offline — and must render `—` rather than a guessed `IDLE`. A server being
/// reconnected keeps an explicit `RECONNECTING` label (held state, not live
/// activity).
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
struct SlotCounter {
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricSample {
    pub name: String,
    pub labels: Option<String>,
    pub value: f64,
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

impl LlamaMonitor {
    pub fn new(server: &str) -> Result<Self, reqwest::Error> {
        // Proxy behavior is deliberately left at reqwest's default: the
        // standard HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY environment
        // variables are honored, with no automatic loopback bypass. That keeps
        // an explicitly configured remote endpoint usable behind a corporate
        // proxy, at the cost that a user with a proxy env var set (and no
        // matching NO_PROXY) would route even the default 127.0.0.1 endpoint
        // through it. Disabling proxies here would break the intentional remote
        // case, and adding a dedicated setting is not justified by a proven
        // requirement, so this stays a documented future configurable behavior.
        // Timeouts and redirect policy below remain fully effective either way.
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(LLM_CONNECT_TIMEOUT_MS))
            .timeout(Duration::from_millis(LLM_REQUEST_TIMEOUT_MS))
            // Monitor exactly the configured endpoint: never silently follow
            // a redirect to another host/port.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            client,
            base: server.trim_end_matches('/').to_string(),
            previous_metrics: PreviousMetricCounters::default(),
            previous_slots: PreviousSlotCounters::default(),
            props: CachedProps::default(),
            last_spec_acceptance_pct: None,
            last_spec_acceptance_at: None,
            props_dirty: true,
            local_spec: None,
            local_spec_scanned_at: None,
            last_metrics_format: None,
            last_strata_since: None,
        })
    }

    pub fn sample(&mut self) -> LlmStats {
        let mut stats = LlmStats::default();

        // /props, /metrics and /slots are independent endpoints, so fetch them
        // concurrently: the poll cycle is bounded by the slowest single request
        // instead of the sum of the three. /props is amortized to the 30 s
        // refresh window; when it is due it runs alongside the other two.
        let fetch_props = self.props_needs_refresh();
        let (props_result, metrics_result, slots_result) = thread::scope(|scope| {
            let client = &self.client;
            let base = &self.base;
            let props_handle = if fetch_props {
                Some(scope.spawn(move || fetch_json(client, endpoint_url(base, "props"))))
            } else {
                None
            };
            let metrics_handle =
                scope.spawn(move || fetch_raw(client, endpoint_url(base, "metrics")));
            let slots_handle = scope.spawn(move || fetch_json(client, endpoint_url(base, "slots")));

            (
                props_handle.map(|handle| {
                    join_or(
                        handle,
                        JsonOutcome::Unreachable("props fetch task panicked".to_string()),
                    )
                }),
                join_or(
                    metrics_handle,
                    MetricsOutcome::Unreachable("metrics fetch task panicked".to_string()),
                ),
                join_or(
                    slots_handle,
                    JsonOutcome::Unreachable("slots fetch task panicked".to_string()),
                ),
            )
        });

        if let Some(props_result) = props_result {
            self.apply_props_result(props_result);
        }
        stats.model = self.props.model.clone();
        stats.context_size = self.props.context_size;
        stats.props_slot_count = self.props.total_slots;
        stats.spec_enabled = self.props.spec_enabled;
        stats.spec_is_mtp = self.props.spec_is_mtp;
        stats.spec_n_max = self.props.spec_n_max;

        // Fill in the fields the server's own responses did not decide from
        // the local server process CLI (see `apply_local_spec_fallback`).
        if let Some(local_spec) = self.local_spec_config() {
            apply_local_spec_fallback(&self.props, &mut stats, local_spec);
        }

        // /metrics and /slots are independent endpoints: a metrics-disabled or
        // failing server must not hide a healthy /slots response. The metrics
        // outcome decides only whether the metric-derived fields are
        // available; /slots is applied below regardless.
        let metrics_text = match metrics_result {
            MetricsOutcome::Ok(text) => Some(text),
            MetricsOutcome::Http(status) => {
                stats.error = if status == StatusCode::NOT_IMPLEMENTED {
                    "/metrics disabled; start llama.cpp with --metrics".to_string()
                } else {
                    format!("/metrics returned HTTP {status}")
                };
                None
            }
            MetricsOutcome::Body(err) => {
                stats.error = format!("metrics response error: {err}");
                None
            }
            MetricsOutcome::Unreachable(err) => {
                stats.error = format!("cannot reach llama.cpp: {err}");
                None
            }
        };
        // A 2xx body carries metric data only when it parses to samples, or is
        // a recognised Strata JSON telemetry object whose cumulative counters
        // are normalised by the adapter. A body with no usable telemetry must
        // not be presented as a real set of zeroes.
        let parsed = metrics_text
            .as_deref()
            .map(parse_metrics_body)
            .unwrap_or_else(|| ParsedMetrics {
                samples: Vec::new(),
                format: None,
                strata_since: None,
                strata_live: None,
                strata_capacity: None,
                json: false,
            });
        let metrics = parsed.samples;
        let strata_live = parsed.strata_live;
        stats.metrics_available = match parsed.format {
            // A recognised Strata endpoint is available even when a particular
            // sample exposes no counters: the fields stay `—`, not fabricated.
            Some(MetricsFormat::Strata) => true,
            Some(MetricsFormat::Prometheus) => metrics_text.is_some() && !metrics.is_empty(),
            None => false,
        };
        if metrics_text.is_some() && !stats.metrics_available {
            stats.error = if parsed.json {
                "/metrics returned an unrecognized JSON format".to_string()
            } else {
                "/metrics returned no metrics".to_string()
            };
        }
        // Backend is identified from the parsed wire body, never assumed (a
        // reachable `/metrics` alone does not imply a backend). Computed from
        // the already-parsed sample; no extra request.
        stats.backend = classify_backend(parsed.format, &metrics);
        // Capacity fallback: if /props did not supply n_ctx, a recognised Strata
        // engine reports its configured context; use it as the CTX denominator
        // (capacity only — never occupancy). /slots n_ctx still refines it below.
        if stats.context_size == 0 {
            if let Some(capacity) = parsed.strata_capacity {
                stats.context_size = capacity;
            }
        }

        // Slot state is served by its own endpoint and stays valid (and
        // visible) when /metrics is disabled or failing.
        let slots_outcome = self.apply_slots_outcome(&mut stats, slots_result);

        // The server is reachable when either endpoint answered successfully;
        // missing metric values are unavailable, not zero.
        stats.connected = stats.metrics_available || stats.slots_available;

        // Current llama.cpp names first, older names retained as compatibility fallbacks.
        stats.prompt_total = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:prompt_tokens_total",
                "llamacpp:tokens_evaluated_total",
                "llamacpp_prompt_tokens_total",
                "prompt_tokens_total",
            ],
        );
        stats.prompt_cached_total =
            pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_cached_total"]);
        stats.generated_total = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:tokens_predicted_total",
                "llamacpp_predicted_tokens_total",
                "predicted_tokens_total",
            ],
        );
        stats.prompt_seconds_total = pick_metric_opt(&metrics, &["llamacpp:prompt_seconds_total"]);
        stats.generation_seconds_total =
            pick_metric_opt(&metrics, &["llamacpp:tokens_predicted_seconds_total"]);

        // Prefer monotonic token/time counters. The legacy throughput gauges have
        // had upstream regressions where they report 0 while inference is active,
        // so they are only a fallback when the counters are absent.
        stats.prompt_avg_tps = average_over_seconds(stats.prompt_total, stats.prompt_seconds_total)
            .or_else(|| {
                pick_metric_opt(
                    &metrics,
                    &[
                        "llamacpp:prompt_tokens_seconds",
                        "llamacpp_prompt_tokens_seconds",
                    ],
                )
            });
        stats.generation_avg_tps =
            average_over_seconds(stats.generated_total, stats.generation_seconds_total).or_else(
                || {
                    pick_metric_opt(
                        &metrics,
                        &[
                            "llamacpp:predicted_tokens_seconds",
                            "llamacpp_predicted_tokens_seconds",
                        ],
                    )
                },
            );
        stats.active_requests = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:requests_processing",
                "llamacpp_requests_processing",
                "requests_processing",
            ],
        );
        stats.deferred_requests = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:requests_deferred",
                "llamacpp_requests_deferred",
                "requests_deferred",
            ],
        );
        stats.context_high_watermark = pick_metric_opt(
            &metrics,
            &["llamacpp:n_tokens_max", "llamacpp_n_tokens_max"],
        )
        .map(|value| value as u64);
        stats.spec_drafts_total =
            pick_metric_opt(&metrics, &["llamacpp:spec_decode_num_drafts_total"]);
        stats.spec_draft_tokens =
            pick_metric_opt(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]);
        stats.spec_accepted_tokens = pick_metric_opt(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"],
        );
        if stats.spec_drafts_total.is_some_and(|value| value > 0.0)
            || stats.spec_draft_tokens.is_some_and(|value| value > 0.0)
            || stats.spec_accepted_tokens.is_some_and(|value| value > 0.0)
        {
            stats.spec_enabled = true;
        }

        // A wire-format switch (llama.cpp ↔ Strata) or a Strata counter reset
        // (`totals.since` change) makes the stored baseline incomparable, so
        // drop it instead of diffing across implementations or a restart.
        let format_changed =
            self.last_metrics_format.is_some() && self.last_metrics_format != parsed.format;
        let since_changed = parsed.format == Some(MetricsFormat::Strata)
            && parsed.strata_since.is_some()
            && self.last_strata_since.is_some()
            && parsed.strata_since != self.last_strata_since;
        if format_changed || since_changed {
            self.previous_metrics = PreviousMetricCounters::default();
        }
        self.last_metrics_format = parsed.format;
        if parsed.format == Some(MetricsFormat::Strata) {
            self.last_strata_since = parsed.strata_since;
        }

        let metric_live = if stats.metrics_available {
            self.update_metric_counters(&mut stats)
        } else {
            // No /metrics: leave the metric-derived throughput unavailable
            // rather than deriving it from zeroed counters, and drop the
            // counter baseline so the first sample after recovery cannot
            // report a delta averaged over the whole outage.
            self.previous_metrics = PreviousMetricCounters::default();
            (None, None)
        };
        match slots_outcome {
            Some(slots) => {
                // A formally successful /slots array is not enough: if its
                // entries cannot be paired by a verified identity, the
                // aggregated /metrics counters are used instead of fabricating
                // per-slot deltas. A dimension the slot path cannot derive falls
                // back to the aggregate counter for that dimension only.
                let slot_live = self.update_live_slot_throughput(slots);
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput(slot_live, metric_live);
            }
            None => {
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput((None, None), metric_live);
                self.previous_slots = PreviousSlotCounters::default();
            }
        }

        // Strata's cumulative `totals` only advance at/near request completion,
        // so counter deltas are not a live rate. Use Strata's authoritative
        // windowed decode rate while it explicitly reports `state=generating`;
        // otherwise report unavailable (`—`) rather than a stale or fabricated
        // speed. Strata exposes no instantaneous prefill field (its
        // `prefill_tok_s_mean` is a per-request mean), so the live prefill rate
        // stays unavailable; the lifetime averages from `totals` are unaffected.
        if parsed.format == Some(MetricsFormat::Strata) {
            let live = strata_live.unwrap_or_default();
            stats.prompt_tps = None;
            stats.generation_tps = live.decode_tps;
            stats.strata_phase = live.phase;
            // Reuse the already-fetched /metrics for active request progress on
            // a version whose /slots carries no progress counters (Strata
            // 0.1.40). Only fills a gap; a newer /slots value is not overridden.
            if stats.request_prompt_tokens.is_none() {
                stats.request_prompt_tokens = live.request_prompt_tokens;
            }
            if stats.request_generated_tokens.is_none() {
                stats.request_generated_tokens = live.request_generated;
            }
        }

        if stats.model.is_empty() {
            stats.model = UNKNOWN_MODEL_LABEL.to_string();
        }

        // Free-text errors can carry server-influenced transport/framing detail
        // (reqwest source chain, malformed-payload diagnostics). Strip terminal
        // control characters and bound the length here, at the single boundary
        // where untrusted text enters `LlmStats`, so no consumer (TUI or
        // `orsiktop diag`) can be injected through the error string. URLs were
        // already redacted at construction.
        sanitize_error_text(&mut stats);

        stats
    }

    fn props_needs_refresh(&self) -> bool {
        self.props_dirty
            || self
                .props
                .last_refresh
                .map(|at| at.elapsed() >= PROPS_REFRESH)
                .unwrap_or(true)
    }

    /// Returns the cached local-server spec config, re-scanning `/proc` only
    /// when the `SPEC_REFRESH` window has elapsed (or never scanned). For
    /// non-loopback endpoints the scan is skipped entirely (returns `None`),
    /// matching `local_speculative_process_config`. The result — including
    /// "no local server found" — is cached for the window, so a dead local
    /// server does not cost a `/proc` walk on every 250 ms sample.
    fn local_spec_config(&mut self) -> Option<LocalSpeculativeConfig> {
        let due = self
            .local_spec_scanned_at
            .map(|at| at.elapsed() >= SPEC_REFRESH)
            .unwrap_or(true);
        if due {
            self.local_spec = local_speculative_process_config(&self.base);
            self.local_spec_scanned_at = Some(Instant::now());
        }
        self.local_spec
    }

    fn apply_props_result(&mut self, outcome: JsonOutcome) {
        let props = match outcome {
            JsonOutcome::Ok(props) => props,
            // A failed /props refresh must not extend the 30 s window: the
            // next sample retries while the cached props keep being shown.
            JsonOutcome::Http(..) | JsonOutcome::Unreachable(..) | JsonOutcome::Invalid(..) => {
                self.props_dirty = true;
                return;
            }
        };

        self.props_dirty = false;
        self.props.last_refresh = Some(Instant::now());

        self.props.context_size = json_u64_path(&props, &["default_generation_settings", "n_ctx"])
            .or_else(|| json_u64_path(&props, &["n_ctx"]))
            .unwrap_or(self.props.context_size);
        self.props.total_slots = props
            .get("total_slots")
            .and_then(Value::as_u64)
            .unwrap_or(self.props.total_slots);

        // Merge the nested (`default_generation_settings`) and root variants,
        // nested field taking precedence. A successful response that mentions
        // the speculative config is then authoritative: the cache is replaced,
        // not accumulated, so an explicit disable (`false`/`0`/empty) clears
        // the previous state and a changed `n_max` (3 → 8) is adopted. A
        // response with no speculative information at all leaves the cache
        // untouched, so older servers do not erase what was already known.
        let nested = props
            .get("default_generation_settings")
            .map(speculative_config);
        let root = speculative_config(&props);
        // The nested variant wins per field; root fills what it omits. The
        // type is taken from whichever source actually carried a `types`
        // string, so a root `speculative.types` is not silently mixed with a
        // nested variant that describes the type differently.
        let type_source = nested.filter(|spec| spec.types_known);
        let merged = SpeculativeConfig {
            present: nested.is_some_and(|spec| spec.present) || root.present,
            enabled: nested.and_then(|spec| spec.enabled).or(root.enabled),
            is_mtp: type_source.map_or(root.is_mtp, |spec| spec.is_mtp),
            n_max: nested.and_then(|spec| spec.n_max).or(root.n_max),
            types_known: type_source.is_some() || root.types_known,
            named_type: type_source.map_or(root.named_type, |spec| spec.named_type),
        };
        if merged.present {
            let n_max = merged.n_max.filter(|value| *value > 0);
            // A named MTP type implies speculative decoding is enabled even
            // when the server did not send the `speculative` boolean.
            self.props.spec_enabled = merged.enabled.unwrap_or(n_max.is_some() || merged.is_mtp);
            self.props.spec_enabled_known = merged.enabled.is_some();
            self.props.spec_is_mtp = merged.is_mtp;
            self.props.spec_named_type = merged.named_type;
            self.props.spec_n_max_known = merged.n_max.is_some();
            self.props.spec_n_max = n_max;
        }

        let model = json_string(&props, &["model_name", "model_alias", "model_path"])
            .map(|value| model_display_name(&value));
        if let Some(model) = model.filter(|name| !name.is_empty()) {
            self.props.model = model;
        }
    }

    fn update_metric_counters(&mut self, stats: &mut LlmStats) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let mut live = (None, None);

        if let Some(previous_at) = self.previous_metrics.at {
            let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
            if seconds > 0.0 {
                live.0 = counter_rate(
                    stats.prompt_total,
                    self.previous_metrics.prompt_total,
                    seconds,
                );
                live.1 = counter_rate(
                    stats.generated_total,
                    self.previous_metrics.generated_total,
                    seconds,
                );
            }

            // Acceptance needs an identified draft delta from present counters.
            // Otherwise (absent counters, reset, or no progress) the last value
            // is held briefly, then dropped — never fabricated as 0.
            let draft_delta = match (stats.spec_draft_tokens, self.previous_metrics.draft_total) {
                (Some(now_drafts), Some(previous_drafts)) => {
                    counter_delta_opt(now_drafts, previous_drafts).filter(|delta| *delta > 0.0)
                }
                _ => None,
            };
            // Acceptance needs a *present* draft delta AND a *present* accepted
            // delta; a missing accepted counter is unknown, not "0 accepted".
            let accepted_delta = match (
                stats.spec_accepted_tokens,
                self.previous_metrics.accepted_total,
            ) {
                (Some(now_accepted), Some(previous_accepted)) => {
                    counter_delta_opt(now_accepted, previous_accepted)
                }
                _ => None,
            };
            if let (Some(draft_delta), Some(accepted_delta)) = (draft_delta, accepted_delta) {
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
        }

        // Only a *present* sample may become the baseline: storing a fabricated
        // `None`/0 here would make the first real sample after a gap look like a
        // spike.
        self.previous_metrics.at = Some(now);
        self.previous_metrics.prompt_total = stats.prompt_total;
        self.previous_metrics.generated_total = stats.generated_total;
        self.previous_metrics.draft_total = stats.spec_draft_tokens;
        self.previous_metrics.accepted_total = stats.spec_accepted_tokens;
        live
    }

    /// Update the slot-counter baseline and return the verified per-dimension
    /// live throughput `(prompt_tps, generation_tps)`. Each element is `None`
    /// when it cannot be derived safely; the caller then uses the aggregated
    /// `/metrics` value for that dimension.
    fn update_live_slot_throughput(
        &mut self,
        slots: Vec<SlotCounter>,
    ) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let live = match self.previous_slots.at {
            Some(previous_at) => {
                let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
                slot_delta_tps(&self.previous_slots.slots, &slots, seconds)
            }
            None => (None, None),
        };

        self.previous_slots.at = Some(now);
        self.previous_slots.slots = slots;
        live
    }

    fn apply_slots_outcome(
        &self,
        stats: &mut LlmStats,
        outcome: JsonOutcome,
    ) -> Option<Vec<SlotCounter>> {
        let value = match outcome {
            JsonOutcome::Ok(value) => value,
            JsonOutcome::Unreachable(err) => {
                stats.slots_error = format!("cannot reach /slots: {err}");
                return None;
            }
            JsonOutcome::Http(status) => {
                stats.slots_error = format!("/slots returned HTTP {status}");
                return None;
            }
            JsonOutcome::Invalid(err) => {
                stats.slots_error = format!("invalid /slots response: {err}");
                return None;
            }
        };

        match apply_slots_json(stats, &value) {
            Ok(slots) => Some(slots),
            Err(err) => {
                stats.slots_error = err;
                None
            }
        }
    }
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
enum JsonOutcome {
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
struct LocalSpeculativeConfig {
    enabled: bool,
    is_mtp: bool,
    n_max: Option<u64>,
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

/// Longest model display string, in `char`s. A server-controlled name is
/// truncated to this so it cannot bloat the TUI or `orsiktop diag` output.
const MAX_MODEL_DISPLAY_CHARS: usize = 120;

#[cfg(test)]
mod tests {
    use super::*;
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
    fn props_failure_retries_next_sample() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        // A failed /props fetch must not extend the 30 s refresh window, so
        // the next sample retries while the cached props keep being shown.
        assert!(monitor.props_needs_refresh());
        monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
        assert!(monitor.props_dirty);
        assert!(monitor.props_needs_refresh());

        // A successful fetch clears the dirty flag and populates the cache.
        let props = json!({
            "model_name": "/models/Qwen3-4B-Q4_K_M.gguf",
            "total_slots": 2,
            "default_generation_settings": { "n_ctx": 4096 }
        });
        monitor.apply_props_result(JsonOutcome::Ok(props));
        assert!(!monitor.props_dirty);
        assert_eq!(monitor.props.model, "Qwen3-4B-Q4_K_M");
        assert_eq!(monitor.props.context_size, 4096);
        assert_eq!(monitor.props.total_slots, 2);
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
    fn local_mtp_is_shown_when_the_server_only_reports_types_none() {
        // Live shape on cf-desktop: /props carries only
        // `speculative.types: "none"`, the CLI is launched with draft-mtp/8.
        // The server did not name a type, so the CLI fills it in.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "default_generation_settings": {"params": {"speculative.types": "none"}}
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(stats.spec_enabled);
        assert!(stats.spec_is_mtp, "CLI draft-mtp must surface");
        assert_eq!(stats.spec_n_max, Some(8));
    }

    #[test]
    fn named_mtp_type_implies_enabled_from_the_server_alone() {
        // A server that names draft-mtp but omits the `speculative` boolean
        // still means speculative decoding is on.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative.types": "draft-mtp"
        })));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
    }

    #[test]
    fn nested_type_none_takes_precedence_over_root_mtp() {
        // Documented precedence: the nested default_generation_settings wins
        // per field, so a nested `types: "none"` overrides a root draft-mtp.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative.types": "draft-mtp",
            "default_generation_settings": {"params": {"speculative.types": "none"}}
        })));
        assert!(!monitor.props.spec_is_mtp);
        assert!(!monitor.props.spec_named_type);
    }

    #[test]
    fn standalone_n_max_zero_suppresses_local_mtp() {
        // An explicit server `speculative.n_max: 0` (no `speculative` boolean)
        // disables speculation: the CLI MTP must not override it.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "params": {"speculative.n_max": 0}
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(!stats.spec_enabled);
        assert!(!stats.spec_is_mtp);
        assert_eq!(stats.spec_n_max, None);
    }

    #[test]
    fn local_mtp_does_not_override_an_explicit_server_disable() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": false,
            "speculative.types": ""
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(!stats.spec_enabled);
        assert!(!stats.spec_is_mtp);
        assert_eq!(stats.spec_n_max, None);
    }

    #[test]
    fn local_mtp_does_not_override_a_server_named_non_mtp_type() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.types": "ngram"
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(stats.spec_enabled);
        assert!(!stats.spec_is_mtp, "server named a non-MTP type");
    }

    #[test]
    fn successful_props_response_replaces_spec_state_instead_of_accumulating() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "model_name": "model-A",
            "speculative": true,
            "speculative.n_max": 3,
            "speculative.types": "draft-mtp"
        })));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
        assert_eq!(monitor.props.spec_n_max, Some(3));

        // Explicit disable on the same endpoint (new model / restarted server):
        // the previous active state must be removed, not OR-ed into.
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "model_name": "model-B",
            "speculative": false,
            "speculative.n_max": 0,
            "speculative.types": ""
        })));
        assert!(!monitor.props.spec_enabled, "stale enabled flag");
        assert!(!monitor.props.spec_is_mtp, "stale mtp flag");
        assert_eq!(monitor.props.spec_n_max, None, "stale n_max");
        assert_eq!(monitor.props.model, "model-B");
    }

    #[test]
    fn successful_props_response_adopts_a_changed_n_max() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "params": {"speculative.n_max": 3}
        })));
        assert_eq!(monitor.props.spec_n_max, Some(3));
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "params": {"speculative.n_max": 8}
        })));
        assert_eq!(monitor.props.spec_n_max, Some(8));
    }

    #[test]
    fn props_without_spec_info_keeps_the_cached_state() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 4,
            "speculative.types": "draft-mtp"
        })));
        // An older response that says nothing about spec must not erase it...
        monitor.apply_props_result(JsonOutcome::Ok(json!({"model_name": "model-C"})));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
        assert_eq!(monitor.props.spec_n_max, Some(4));

        // ...and neither must a failed refresh.
        monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
        assert!(monitor.props.spec_enabled);
        assert_eq!(monitor.props.spec_n_max, Some(4));
    }

    #[test]
    fn default_generation_settings_takes_precedence_over_root() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 3,
            "default_generation_settings": {
                "speculative": false,
                "speculative.n_max": 0
            }
        })));
        assert!(!monitor.props.spec_enabled, "nested disable must win");
        assert_eq!(monitor.props.spec_n_max, None);

        // Root fields fill what the nested variant does not mention.
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 6,
            "default_generation_settings": {"n_ctx": 4096}
        })));
        assert!(monitor.props.spec_enabled);
        assert_eq!(monitor.props.spec_n_max, Some(6));
    }

    #[test]
    fn local_spec_config_is_cached_within_the_refresh_window() {
        // A non-loopback endpoint never triggers the /proc scan: the result
        // is None and the window starts on the first call. A second call
        // within the window returns the cached None without rescanning
        // (the timestamp is unchanged, proving no new scan ran).
        let mut monitor = LlamaMonitor::new("http://10.0.0.7:9090").unwrap();
        assert!(monitor.local_spec_config().is_none());
        let first = monitor.local_spec_scanned_at.expect("scan timestamp set");
        assert!(monitor.local_spec_config().is_none());
        assert_eq!(
            monitor.local_spec_scanned_at,
            Some(first),
            "cached value, no rescan"
        );
    }

    #[test]
    fn slots_outcome_maps_to_error_or_counters() {
        let monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        // A failed /slots fetch degrades gracefully: it sets slots_error and
        // does not touch the previous-slot counters or mark the slot set
        // unavailable.
        let mut stats = LlmStats::default();
        let slots =
            monitor.apply_slots_outcome(&mut stats, JsonOutcome::Http(StatusCode::NOT_FOUND));
        assert!(slots.is_none());
        assert!(!stats.slots_available);
        assert_eq!(stats.slots_error, "/slots returned HTTP 404 Not Found");

        let mut stats = LlmStats::default();
        let slots = monitor.apply_slots_outcome(
            &mut stats,
            JsonOutcome::Unreachable("connection refused".into()),
        );
        assert!(slots.is_none());
        assert!(!stats.slots_available);
        assert_eq!(stats.slots_error, "cannot reach /slots: connection refused");

        // A valid /slots payload populates the counters and clears any error.
        let value = json!([{ "id": 0, "task_id": 1, "state": "busy" }]);
        let mut stats = LlmStats::default();
        let slots = monitor.apply_slots_outcome(&mut stats, JsonOutcome::Ok(value));
        assert!(slots.is_some());
        assert!(stats.slots_available);
        assert_eq!(stats.slot_count, 1);
        assert!(stats.slots_error.is_empty());
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
    fn sample_reads_strata_json_metrics_end_to_end() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let metrics_calls = Arc::new(AtomicU32::new(0));
        let calls = Arc::clone(&metrics_calls);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        let body = if path.contains("/props") {
                            r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_alias":"strata-model"}"#.to_string()
                        } else if path.contains("/slots") {
                            r#"[{"id":0,"n_ctx":4096,"is_processing":false}]"#.to_string()
                        } else if path.contains("/metrics") {
                            let n = u64::from(calls.fetch_add(1, Ordering::SeqCst) + 1);
                            // First sample reports an active decode phase; the
                            // second is idle. The phase gates whether a live
                            // decode rate is surfaced at all.
                            let (state, tok_s) = if n == 1 {
                                ("generating", "42.0")
                            } else {
                                ("idle", "null")
                            };
                            format!(
                                r#"{{"engine":{{"model":"strata"}},"live":{{"state":"{state}","queued":0,"tok_s":{tok_s}}},"requests_kept":1,"totals":{{"since":1.0,"prompt_tokens":{},"output_tokens":{},"prompt_ms":1000.0,"decode_ms":500.0,"drafts_offered":40,"drafts_accepted":30}}}}"#,
                                1000 * n,
                                250 * n
                            )
                        } else {
                            "{}".to_string()
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let first = monitor.sample();
        assert!(first.connected, "strata server must be connected");
        assert!(
            first.metrics_available,
            "strata JSON /metrics must be available, error={:?}",
            first.error
        );
        assert_eq!(first.error, "", "no metrics-off error expected");
        assert_eq!(first.model, "strata-model");
        assert_eq!(
            first.backend,
            crate::domain::ServerBackend::Strata,
            "a recognised Strata body must be identified as Strata"
        );
        assert_eq!(first.prompt_total, Some(1000.0));
        assert_eq!(first.generated_total, Some(250.0));
        // Lifetime averages from totals + ms-derived seconds.
        assert_eq!(first.prompt_avg_tps, Some(1000.0));
        assert_eq!(first.generation_avg_tps, Some(500.0));
        // The live decode rate is Strata's authoritative windowed value, not a
        // counter delta; Strata has no instantaneous prefill rate.
        assert_eq!(first.generation_tps, Some(42.0));
        assert_eq!(first.prompt_tps, None);
        assert!(first.slots_available);
        assert_eq!(first.slot_count, 1);

        // Idle: no stale decode speed is carried over.
        let second = monitor.sample();
        assert!(second.metrics_available);
        assert_eq!(second.prompt_total, Some(2000.0));
        assert_eq!(second.generation_tps, None);
        assert_eq!(second.prompt_tps, None);

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_wire_format_switch_resets_the_counter_baseline() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let calls = Arc::new(AtomicU32::new(0));
        let calls_flag = Arc::clone(&calls);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        // /slots uses the exact Strata shape (no counters), so
                        // the slot path must abstain and the counter baseline
                        // (not a fabricated slot zero) is the only live-rate
                        // source under test.
                        let response = if path.contains("/props") {
                            let body = r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_alias":"m"}"#;
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                        } else if path.contains("/slots") {
                            let body = r#"[{"id":0,"n_ctx":4096,"is_processing":false}]"#;
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                        } else if path.contains("/metrics") {
                            let n = calls_flag.fetch_add(1, Ordering::SeqCst);
                            let body = if n == 0 {
                                // First: a Strata sample with tiny counters.
                                r#"{"engine":{},"live":{"state":"idle"},"requests_kept":1,"totals":{"since":1.0,"prompt_tokens":5,"output_tokens":5,"prompt_ms":1000.0,"decode_ms":1000.0}}"#.to_string()
                            } else {
                                // Then: llama.cpp Prometheus with much larger counters.
                                "# fixture\nllamacpp:prompt_tokens_total 100000\nllamacpp:tokens_predicted_total 100000\nllamacpp:prompt_seconds_total 10\nllamacpp:tokens_predicted_seconds_total 10\n".to_string()
                            };
                            format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                                .to_string()
                        };
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let strata = monitor.sample();
        assert!(strata.metrics_available);
        assert!(strata.slots_available);
        assert_eq!(strata.prompt_total, Some(5.0));
        assert_eq!(strata.prompt_tps, None);
        assert_eq!(strata.generation_tps, None);

        // Switching to Prometheus must not diff the llama counter against the
        // Strata baseline (which would read as a huge spike). The counter-less
        // /slots abstains, so a `None` proves the counter baseline was reset; a
        // leaked baseline would produce `Some(≈100000/seconds)`.
        let prometheus = monitor.sample();
        assert!(prometheus.metrics_available);
        assert_eq!(prometheus.prompt_total, Some(100000.0));
        assert_eq!(prometheus.generation_avg_tps, Some(10000.0));
        assert_eq!(
            prometheus.prompt_tps, None,
            "baseline must reset across a wire-format switch"
        );
        assert_eq!(
            prometheus.generation_tps, None,
            "baseline must reset across a wire-format switch"
        );

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn sample_keeps_strata_context_occupancy_unknown_but_tracks_progress() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        // /props has no n_ctx and /slots is Strata 0.1.40-shaped
                        // (no counters, no occupancy); /metrics carries the
                        // engine capacity and live active-request progress.
                        let body = if path.contains("/props") {
                            r#"{"total_slots":1,"model_alias":"strata-model"}"#.to_string()
                        } else if path.contains("/slots") {
                            r#"[{"id":0,"is_processing":true}]"#.to_string()
                        } else if path.contains("/metrics") {
                            r#"{"engine":{"max_context":196608},"live":{"state":"generating","prompt_tokens":500,"generated":20,"tok_s":42.0},"requests_kept":1,"totals":{"since":1.0,"prompt_tokens":5,"output_tokens":5,"prompt_ms":1000.0,"decode_ms":1000.0}}"#.to_string()
                        } else {
                            "{}".to_string()
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut monitor = LlamaMonitor::new(&format!("http://{addr}")).unwrap();
        let stats = monitor.sample();
        assert!(stats.connected && stats.metrics_available);
        // Capacity comes from the Strata engine when /props omitted n_ctx.
        assert_eq!(stats.context_size, 196608);
        // Occupancy is not fabricated from the active request length.
        assert_eq!(stats.context_used, None);
        // Active request progress is surfaced from the already-fetched /metrics.
        assert_eq!(stats.request_prompt_tokens, Some(500));
        assert_eq!(stats.request_generated_tokens, Some(20));
        assert_eq!(stats.generation_tps, Some(42.0));

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn parses_current_llama_metrics_fixture() {
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_current.prom"));

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
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_speculative.prom"));
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
    fn metric_counter_absence_does_not_poison_the_baseline() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        let mut stats = LlmStats {
            prompt_total: Some(1000.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Partial payload: the prompt counter is absent this sample.
        let mut stats = LlmStats {
            prompt_total: None,
            ..Default::default()
        };
        let live = monitor.update_metric_counters(&mut stats);
        assert_eq!(live.0, None, "absent counter -> unavailable rate");

        // It reappears: the absent sample must not be stored as a `0` baseline,
        // so this establishes a fresh baseline instead of a huge spike.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut stats = LlmStats {
            prompt_total: Some(1100.0),
            ..Default::default()
        };
        let live = monitor.update_metric_counters(&mut stats);
        assert_eq!(live.0, None, "reappearance must re-baseline, never spike");
    }

    #[test]
    fn derives_live_context_from_slots_fixture() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_active.json")).unwrap();

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
            serde_json::from_str(include_str!("../tests/fixtures/slots_idle.json")).unwrap();
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
        assert_eq!(stats.request_prompt_tokens, Some(6000));
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
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_strata_old.json")).unwrap();
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
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_strata_new.json")).unwrap();
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, Some(113274));
        assert_eq!(stats.context_slot_id, Some(0));
        assert_eq!(stats.slot_overview[0].context_used, Some(113274));
        // The same field (as prompt_processed) feeds the busy REQUEST row.
        assert_eq!(stats.request_prompt_tokens, Some(113274));
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
    fn spec_acceptance_computes_from_deltas_and_holds_after_quiet() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: Some(200.0),
            spec_accepted_tokens: Some(140.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Second sample: 100 new draft tokens, 60 accepted -> 60%.
        let mut stats = LlmStats {
            spec_draft_tokens: Some(300.0),
            spec_accepted_tokens: Some(200.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(60.0));

        // Third sample with no new draft tokens: the last value is held for
        // SPEC_ACCEPTANCE_HOLD instead of dropping to None.
        let mut stats = LlmStats {
            spec_draft_tokens: Some(300.0),
            spec_accepted_tokens: Some(200.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(60.0));
    }

    #[test]
    fn spec_acceptance_expires_after_the_hold_window() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: Some(100.0),
            spec_accepted_tokens: Some(50.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        let mut stats = LlmStats {
            spec_draft_tokens: Some(200.0),
            spec_accepted_tokens: Some(100.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(50.0));

        let mut stats = LlmStats {
            spec_draft_tokens: Some(200.0),
            spec_accepted_tokens: Some(100.0),
            ..Default::default()
        };
        let expired = Instant::now() - (SPEC_ACCEPTANCE_HOLD + Duration::from_secs(1));
        monitor.last_spec_acceptance_at = Some(expired);
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, None);
    }

    #[test]
    fn spec_acceptance_clamps_to_hundred_percent() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: Some(100.0),
            spec_accepted_tokens: Some(100.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Accepted delta larger than the draft delta must clamp, not exceed.
        let mut stats = LlmStats {
            spec_draft_tokens: Some(200.0),
            spec_accepted_tokens: Some(250.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(100.0));
    }

    #[test]
    fn spec_counter_reset_does_not_create_fake_acceptance() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: Some(1000.0),
            spec_accepted_tokens: Some(900.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Server restart: counters reset below the previous values. No draft
        // progress and no fresh hold -> no acceptance value, no NaN.
        let mut stats = LlmStats {
            spec_draft_tokens: Some(10.0),
            spec_accepted_tokens: Some(5.0),
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, None);
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
    struct MockBackend {
        base: String,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MockBackend {
        fn base(&self) -> &str {
            &self.base
        }
    }

    impl Drop for MockBackend {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn spawn_backend<F>(respond: F) -> MockBackend
    where
        F: Fn(&str, usize) -> (u16, &'static str, String) + Send + 'static,
    {
        use std::collections::HashMap;
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock backend");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let calls: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stop_flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        let index = {
                            let mut calls = calls.lock().expect("mock call counter");
                            let entry = calls.entry(path.clone()).or_insert(0);
                            let index = *entry;
                            *entry += 1;
                            index
                        };
                        let (status, content_type, body) = respond(&path, index);
                        let reason = match status {
                            200 => "OK",
                            404 => "Not Found",
                            501 => "Not Implemented",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        MockBackend {
            base: format!("http://{addr}"),
            stop,
        }
    }

    #[test]
    fn a_missing_props_endpoint_leaves_the_model_unknown_but_keeps_telemetry() {
        // /props 404 while /metrics and /slots answer: the server is still
        // reachable, so metric and slot telemetry must stay available and the
        // model stays unknown rather than guessed.
        let backend = spawn_backend(|path, _n| {
            if path.contains("/props") {
                (
                    404,
                    "application/json",
                    "{\"error\":\"not found\"}".to_string(),
                )
            } else if path.contains("/metrics") {
                (
                    200,
                    "text/plain",
                    include_str!("../tests/fixtures/metrics_current.prom").to_string(),
                )
            } else if path.contains("/slots") {
                (
                    200,
                    "application/json",
                    include_str!("../tests/fixtures/slots_idle.json").to_string(),
                )
            } else {
                (404, "text/plain", String::new())
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
        let stats = monitor.sample();

        assert!(stats.connected);
        assert!(stats.metrics_available);
        assert!(stats.slots_available);
        assert_eq!(stats.prompt_total, Some(12000.0));
        assert_eq!(stats.model, UNKNOWN_MODEL_LABEL, "no props -> no model");
        // Capacity still comes from the /slots payload, not from a guess.
        assert_eq!(stats.context_size, 196608);
        assert_eq!(stats.slot_count, 1);
    }

    #[test]
    fn a_missing_slots_endpoint_keeps_metrics_visible() {
        // /slots 404 while /metrics answers: metric-derived values stay real,
        // the slot-derived values stay unavailable (never a fabricated zero).
        let backend = spawn_backend(|path, _n| {
            if path.contains("/props") {
                (
                    200,
                    "application/json",
                    include_str!("../tests/fixtures/props.json").to_string(),
                )
            } else if path.contains("/metrics") {
                (
                    200,
                    "text/plain",
                    include_str!("../tests/fixtures/metrics_current.prom").to_string(),
                )
            } else {
                (404, "application/json", "[]".to_string())
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
        let stats = monitor.sample();

        assert!(stats.connected);
        assert!(stats.metrics_available);
        assert!(!stats.slots_available);
        assert_eq!(stats.prompt_total, Some(12000.0));
        assert!(
            stats.slots_error.contains("404"),
            "the /slots failure must be surfaced: {:?}",
            stats.slots_error
        );
        assert_eq!(stats.model, "Qwen3.8-27B-UD-Q4_K_M");
        // Context size is seeded by /props; occupancy stays unknown.
        assert_eq!(stats.context_size, 196608);
        assert_eq!(stats.context_used, None);
        assert_eq!(stats.slot_count, 0);
    }

    #[test]
    fn unrecognized_metrics_json_goes_unavailable_and_recovers_without_a_spike() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Sequence: valid llama.cpp -> unrecognised JSON -> valid llama.cpp
        // with a larger counter. The invalid sample must clear availability and
        // drop the counter baseline so recovery cannot report a delta averaged
        // across the gap.
        let metrics_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&metrics_calls);
        let backend = spawn_backend(move |path, _n| {
            if path.contains("/metrics") {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                if index == 1 {
                    (
                        200,
                        "application/json",
                        "{\"unexpected\": true}".to_string(),
                    )
                } else {
                    (
                        200,
                        "text/plain",
                        format!(
                            "llamacpp:prompt_tokens_total {}\n",
                            12000 + 50 * index as u64
                        ),
                    )
                }
            } else if path.contains("/slots") {
                (
                    200,
                    "application/json",
                    "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
                )
            } else {
                (
                    200,
                    "application/json",
                    "{\"model_name\":\"m\",\"total_slots\":1}".to_string(),
                )
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

        let first = monitor.sample();
        assert!(first.metrics_available);
        assert_eq!(first.prompt_total, Some(12000.0));
        assert_eq!(
            first.prompt_tps, None,
            "first sample only sets the baseline"
        );

        thread::sleep(Duration::from_millis(2));

        let invalid = monitor.sample();
        assert!(!invalid.metrics_available);
        assert_eq!(invalid.prompt_total, None, "no fabricated zero");
        assert_eq!(invalid.prompt_tps, None);
        assert_eq!(invalid.backend, crate::domain::ServerBackend::Unknown);
        assert!(
            invalid.error.contains("unrecognized JSON"),
            "the reason must be format-specific: {:?}",
            invalid.error
        );

        thread::sleep(Duration::from_millis(2));

        let recovered = monitor.sample();
        assert!(recovered.metrics_available);
        assert_eq!(recovered.prompt_total, Some(12100.0));
        assert_eq!(
            recovered.prompt_tps, None,
            "the baseline was dropped during the invalid sample; recovery re-baselines"
        );
    }

    #[test]
    fn a_prometheus_counter_reset_after_a_restart_is_unavailable_then_recovers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // A restarted llama.cpp restarts every cumulative counter at zero. That
        // is a reset, not a negative rate: the live value must be unavailable,
        // and the renewed climb establishes a fresh baseline.
        let metrics_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&metrics_calls);
        let backend = spawn_backend(move |path, _n| {
            if path.contains("/metrics") {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let body = match index {
                    0 => "llamacpp:prompt_tokens_total 12000\nllamacpp:prompt_seconds_total 12\nllamacpp:tokens_predicted_total 800\nllamacpp:tokens_predicted_seconds_total 20\n".to_string(),
                    1 => "llamacpp:prompt_tokens_total 0\nllamacpp:tokens_predicted_total 0\n".to_string(),
                    _ => "llamacpp:prompt_tokens_total 200\nllamacpp:tokens_predicted_total 50\n".to_string(),
                };
                (200, "text/plain", body)
            } else if path.contains("/slots") {
                // Counter-less /slots: the aggregate /metrics delta is the only
                // live-rate source under test.
                (
                    200,
                    "application/json",
                    "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
                )
            } else {
                (
                    200,
                    "application/json",
                    "{\"model_name\":\"m\",\"total_slots\":1}".to_string(),
                )
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

        let first = monitor.sample();
        assert!(first.metrics_available);
        assert_eq!(first.prompt_total, Some(12000.0));
        // Lifetime averages are unaffected by the reset semantics.
        assert_eq!(first.prompt_avg_tps, Some(1000.0));
        assert_eq!(first.generation_avg_tps, Some(40.0));
        assert_eq!(first.prompt_tps, None);

        thread::sleep(Duration::from_millis(2));

        let restarted = monitor.sample();
        assert_eq!(restarted.prompt_total, Some(0.0));
        assert_eq!(restarted.prompt_tps, None, "a counter reset is not a rate");
        assert_eq!(restarted.generation_tps, None);

        thread::sleep(Duration::from_millis(2));

        let recovered = monitor.sample();
        assert_eq!(recovered.prompt_total, Some(200.0));
        assert!(
            recovered.prompt_tps.is_some_and(|rate| rate > 0.0),
            "a counter climbing from the new baseline is a real rate: {:?}",
            recovered.prompt_tps
        );
    }

    #[test]
    fn legacy_llama_metric_names_are_accepted_as_fallbacks() {
        let backend = spawn_backend(|path, _n| {
            if path.contains("/metrics") {
                (
                    200,
                    "text/plain",
                    include_str!("../tests/fixtures/metrics_legacy_names.prom").to_string(),
                )
            } else if path.contains("/slots") {
                (
                    200,
                    "application/json",
                    "[{\"id\":0,\"n_ctx\":4096,\"is_processing\":false}]".to_string(),
                )
            } else {
                (
                    200,
                    "application/json",
                    "{\"model_name\":\"legacy\",\"total_slots\":1}".to_string(),
                )
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();
        let stats = monitor.sample();

        assert!(stats.metrics_available);
        assert_eq!(
            stats.prompt_total,
            Some(1000.0),
            "llamacpp:tokens_evaluated_total is a legacy prompt name"
        );
        assert_eq!(
            stats.generated_total,
            Some(250.0),
            "predicted_tokens_total is a legacy generation name"
        );
        // The legacy gauges are only a fallback average source when no seconds
        // counter is present.
        assert_eq!(stats.prompt_avg_tps, Some(0.0));
        assert_eq!(stats.generation_avg_tps, Some(0.0));
    }

    #[test]
    fn prometheus_payload_with_unknown_fields_keeps_known_metrics_and_labels() {
        let metrics = parse_prometheus(include_str!(
            "../tests/fixtures/metrics_schema_unknown.prom"
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
            "../tests/fixtures/strata_metrics_unknown.json"
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

    #[test]
    fn an_invalid_props_refresh_keeps_the_cached_model() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let props_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&props_calls);
        let backend = spawn_backend(move |path, _n| {
            if path.contains("/metrics") {
                (
                    200,
                    "text/plain",
                    "llamacpp:prompt_tokens_total 10\n".to_string(),
                )
            } else if path.contains("/slots") {
                (200, "application/json", "[]".to_string())
            } else {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                if index == 0 {
                    (
                        200,
                        "application/json",
                        "{\"model_name\":\"model-A\",\"default_generation_settings\":{\"n_ctx\":4096}}"
                            .to_string(),
                    )
                } else {
                    (200, "application/json", "this is not json".to_string())
                }
            }
        });
        let mut monitor = LlamaMonitor::new(backend.base()).unwrap();

        let first = monitor.sample();
        assert_eq!(first.model, "model-A");
        assert_eq!(first.context_size, 4096);

        // Force a /props refresh; the server now returns a malformed body.
        monitor.props_dirty = true;
        thread::sleep(Duration::from_millis(2));
        let second = monitor.sample();

        assert_eq!(
            second.model, "model-A",
            "a malformed refresh must not erase the cache"
        );
        assert_eq!(second.context_size, 4096);
        // The refresh stays pending so the next sample retries.
        assert!(monitor.props_dirty);
    }
}

#[cfg(test)]
mod speculative_status_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_speculative_status_and_n_max_from_slot_shape() {
        let slot = json!({
            "speculative": true,
            "params": {"speculative.n_max": 3}
        });
        assert_eq!(
            speculative_config(&slot),
            SpeculativeConfig {
                present: true,
                enabled: Some(true),
                is_mtp: false,
                n_max: Some(3),
                types_known: false,
                named_type: false,
            }
        );
    }

    #[test]
    fn slot_telemetry_marks_speculative_enabled() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "id": 0,
            "id_task": 11,
            "n_ctx": 4096,
            "speculative": true,
            "is_processing": true,
            "params": {"speculative.n_max": 3},
            "n_prompt_tokens": 128,
            "n_prompt_tokens_processed": 128,
            "next_token": {"n_decoded": 9}
        }]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert!(stats.spec_enabled);
        assert_eq!(stats.spec_n_max, Some(3));
    }
}

#[cfg(test)]
mod local_speculative_process_tests {
    use super::*;

    #[test]
    fn loopback_host_matches_ipv4_ipv6_and_localhost_equally() {
        // IPv6 hosts come back from Url::host_str() bracketed; the local
        // SPEC-CLI fallback must treat `[::1]` exactly like `127.0.0.1`.
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        // Any loopback address in the reserved ranges counts.
        assert!(is_loopback_host("127.0.0.2"));
        // A remote or wildcard host must not trigger a local /proc scan.
        assert!(!is_loopback_host("192.168.1.20"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("::"));
        assert!(!is_loopback_host("[::]"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn reads_draft_mtp_type_from_custom_slot_shape() {
        let value = serde_json::json!({
            "speculative": true,
            "params": {"speculative.types": "none,draft-mtp"}
        });
        let spec = speculative_config(&value);
        assert_eq!(spec.enabled, Some(true));
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, None);
    }

    #[test]
    fn recognizes_llama_serve_subcommand_and_reads_n_max() {
        let args = vec![
            "/home/christopherf/.local/bin/llama".to_string(),
            "serve".to_string(),
            "-hf".to_string(),
            "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".to_string(),
            "--port".to_string(),
            "8081".to_string(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--spec-draft-n-max".to_string(),
            "3".to_string(),
        ];

        assert!(is_llama_server_process(&args));
        assert!(command_targets_port(&args, 8081));
        let spec = speculative_from_process(&args, None);
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
    }

    #[test]
    fn reads_n_max_from_cli() {
        let args = vec![
            "llama-server".to_string(),
            "--port".to_string(),
            "8081".to_string(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--spec-draft-n-max".to_string(),
            "3".to_string(),
        ];
        let spec = speculative_from_process(&args, None);
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
        assert!(command_targets_port(&args, 8081));
    }

    #[test]
    fn reads_n_max_from_environment() {
        let args = vec!["llama-server".to_string(), "--port=8081".to_string()];
        let env = b"LLAMA_ARG_SPEC_TYPE=draft-mtp\0LLAMA_ARG_SPEC_DRAFT_N_MAX=3\0";
        let spec = speculative_from_process(&args, Some(env));
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
    }

    #[test]
    fn sanitize_error_text_neutralizes_injected_control_characters() {
        // The display boundary `sample()` applies to every error field. A
        // synthetic server-influenced error carries ANSI/CR/LF/tab/NUL; after
        // the boundary it must be terminal-safe (no injection, no forged report
        // line) while the useful diagnostic context survives.
        let mut stats = LlmStats {
            error: "transport\u{1b}[31mboom\r\nforged line\ttail".to_string(),
            slots_error: "slots\u{0}error".to_string(),
            ..Default::default()
        };
        sanitize_error_text(&mut stats);
        for text in [&stats.error, &stats.slots_error] {
            assert!(
                !text.chars().any(char::is_control),
                "control character survived the boundary: {text:?}"
            );
        }
        assert!(stats.error.contains("transport"), "{:?}", stats.error);
        assert!(stats.error.contains("boom"), "{:?}", stats.error);
        assert!(stats.error.contains("forged line"), "{:?}", stats.error);
        assert!(
            stats.slots_error.contains("slots"),
            "{:?}",
            stats.slots_error
        );
        assert!(
            stats.slots_error.contains("error"),
            "{:?}",
            stats.slots_error
        );

        // The length is bounded so a hostile response cannot bloat the output.
        let mut long = LlmStats {
            error: "x".repeat(1000),
            ..Default::default()
        };
        sanitize_error_text(&mut long);
        assert_eq!(
            long.error.chars().count(),
            crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS + 1
        );
    }
}
