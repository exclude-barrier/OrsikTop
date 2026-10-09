use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;

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
    /// Tokens the selected slot is using, or the `/metrics` high-water mark
    /// without `/slots`. `None` when `/slots` reports no usable occupancy:
    /// unknown, rendered `—`, never a fabricated `0`.
    pub context_used: Option<u64>,
    /// `/metrics` context high-water mark. `None` when the metric is absent, so
    /// a missing watermark can never be mistaken for a real `0` occupancy.
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
    pub spec_drafts_total: Option<f64>,
    pub spec_draft_tokens: Option<f64>,
    pub spec_accepted_tokens: Option<f64>,
    pub spec_enabled: bool,
    pub spec_is_mtp: bool,
    pub spec_n_max: Option<u64>,
    pub spec_acceptance_pct: Option<f64>,
    pub error: String,
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
                json: false,
            });
        let metrics = parsed.samples;
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

/// Apply the display-safe text boundary to the free-text error fields of a
/// sample. This is the single place server-influenced error text enters
/// [`LlmStats`], so stripping terminal control characters and bounding the
/// length here covers every consumer (TUI and `orsiktop diag`). URLs were
/// already redacted when the error strings were built.
fn sanitize_error_text(stats: &mut LlmStats) {
    stats.error = crate::redact::sanitize_display_text(
        &stats.error,
        crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
    );
    stats.slots_error = crate::redact::sanitize_display_text(
        &stats.slots_error,
        crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
    );
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
    json: bool,
}

/// Dispatch a `/metrics` body by its actual shape instead of assuming
/// Prometheus. A body that starts with `{`/`[` is JSON: it is parsed once and
/// accepted only when it is a recognised Strata object, so pretty-printed JSON
/// can never reach the line-oriented Prometheus parser (which would otherwise
/// manufacture bogus samples from `"key": value` lines). Everything else is
/// treated as Prometheus text.
fn parse_metrics_body(text: &str) -> ParsedMetrics {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) if crate::strata::is_strata_metrics(&value) => ParsedMetrics {
                samples: crate::strata::extract_samples(&value),
                format: Some(MetricsFormat::Strata),
                strata_since: crate::strata::totals_since(&value),
                json: true,
            },
            _ => ParsedMetrics {
                samples: Vec::new(),
                format: None,
                strata_since: None,
                json: true,
            },
        }
    } else {
        ParsedMetrics {
            samples: parse_prometheus(text),
            format: Some(MetricsFormat::Prometheus),
            strata_since: None,
            json: false,
        }
    }
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

/// Join a scoped fetch handle, converting a thread panic into the caller's
/// error outcome instead of propagating it. A panic in one endpoint's fetch
/// must not kill the whole LLM worker and stop all telemetry.
fn join_or<T>(handle: std::thread::ScopedJoinHandle<'_, T>, on_panic: T) -> T {
    handle.join().unwrap_or(on_panic)
}

/// Build an API endpoint URL from the configured base, appending `segment`
/// (`metrics`/`props`/`slots`) to the base *path* and preserving any query
/// (e.g. a token). A plain `format!("{base}/{segment}")` would fold the segment
/// into a base query (`?token=x/metrics`) or a path segment (`/v1/metrics`)
/// instead, so `/metrics` was never actually requested.
fn endpoint_url(base: &str, segment: &str) -> String {
    match reqwest::Url::parse(base) {
        Ok(mut url) => {
            let path = url.path().trim_end_matches('/');
            url.set_path(&format!("{path}/{segment}"));
            url.to_string()
        }
        Err(_) => format!("{}/{segment}", base.trim_end_matches('/')),
    }
}

/// Fetches a raw text endpoint (currently only /metrics).
fn fetch_raw(client: &Client, url: String) -> MetricsOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        // Structured redaction: strip the request URL (which may carry
        // credentials/query secrets) before it becomes a telemetry string.
        Err(err) => return MetricsOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return MetricsOutcome::Http(status);
    }
    match read_body_limited(response, MAX_RESPONSE_BYTES) {
        Ok(body) => MetricsOutcome::Ok(body),
        Err(err) => MetricsOutcome::Body(err),
    }
}

/// Read a response body up to `limit` bytes, refusing an oversized one instead
/// of allocating without bound. The error carries no request URL, so it is safe
/// to surface.
fn read_body_limited(response: reqwest::blocking::Response, limit: u64) -> Result<String, String> {
    use std::io::Read as _;
    let mut buffer = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(|err| err.to_string())?;
    if buffer.len() as u64 > limit {
        return Err(format!("response body exceeded {limit} bytes"));
    }
    String::from_utf8(buffer).map_err(|err| err.to_string())
}

/// Fetches a JSON endpoint (/props, /slots).
fn fetch_json(client: &Client, url: String) -> JsonOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(err) => return JsonOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return JsonOutcome::Http(status);
    }
    match read_body_limited(response, MAX_RESPONSE_BYTES) {
        Ok(body) => match serde_json::from_str::<Value>(&body) {
            Ok(value) => JsonOutcome::Ok(value),
            Err(err) => JsonOutcome::Invalid(err.to_string()),
        },
        Err(err) => JsonOutcome::Invalid(err),
    }
}

fn apply_slots_json(stats: &mut LlmStats, value: &Value) -> Result<Vec<SlotCounter>, String> {
    let slots = value
        .as_array()
        .ok_or_else(|| "/slots response is not an array".to_string())?;

    stats.slots_available = true;
    stats.slots_error.clear();
    stats.slot_count = slots.len() as u64;

    // The context pair (used, size) and the slot ID it belongs to are always
    // read from a single slot: the most-used busy slot, or the most-used slot
    // overall when no slot is busy. Taking max(used) and max(n_ctx)
    // independently would mix values from different slots once slot
    // capacities differ.
    let mut best_busy: Option<(u64, u64, Option<u64>)> = None;
    let mut best_any: Option<(u64, u64, Option<u64>)> = None;
    // Carries (n_ctx, slot id) for the size/id when no slot reports a usable
    // occupancy, so the pair still has a real capacity while the used count
    // stays unknown.
    let mut fallback_busy: Option<(u64, Option<u64>)> = None;
    let mut fallback_any: Option<(u64, Option<u64>)> = None;
    let mut busy = 0u64;
    let mut request_prompt_tokens: Option<u64> = None;
    let mut request_generated_tokens: Option<u64> = None;
    // A busy slot that lacks a counter makes that total incomplete, not a
    // smaller number: the aggregate must then be unavailable rather than a
    // silent partial sum.
    let mut request_prompt_complete = true;
    let mut request_generated_complete = true;
    let mut counters = Vec::with_capacity(slots.len());
    let mut overview = Vec::with_capacity(slots.len());

    for slot in slots.iter() {
        let n_ctx = slot.get("n_ctx").and_then(Value::as_u64).unwrap_or(0);
        // The slot's own identity from /slots. Absent in older responses —
        // kept as None rather than guessed.
        let slot_id = slot.get("id").and_then(Value::as_u64);

        let is_processing = slot
            .get("is_processing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_processing {
            busy += 1;
        }

        let slot_spec = speculative_config(slot);
        if slot_spec.enabled.unwrap_or(false) || slot_spec.n_max.is_some_and(|value| value > 0) {
            stats.spec_enabled = true;
        }
        stats.spec_is_mtp |= slot_spec.is_mtp;
        if let Some(n_max) = slot_spec.n_max.filter(|value| *value > 0) {
            stats.spec_n_max = Some(stats.spec_n_max.map_or(n_max, |current| current.max(n_max)));
        }

        let prompt_tokens = slot.get("n_prompt_tokens").and_then(Value::as_u64);
        let prompt_processed = slot
            .get("n_prompt_tokens_processed")
            .and_then(Value::as_u64)
            .or(prompt_tokens);
        let decoded = slot_decoded_tokens_opt(slot);

        if is_processing {
            // Only a reported count contributes; a busy slot that reports no
            // prompt/decoded count makes that total incomplete (None), not 0.
            match prompt_processed {
                Some(value) => {
                    request_prompt_tokens =
                        Some(request_prompt_tokens.unwrap_or(0).saturating_add(value));
                }
                None => request_prompt_complete = false,
            }
            match decoded {
                Some(value) => {
                    request_generated_tokens =
                        Some(request_generated_tokens.unwrap_or(0).saturating_add(value));
                }
                None => request_generated_complete = false,
            }
        }

        // Current llama.cpp exposes n_prompt_tokens as the slot's current prompt/context
        // token count. Older responses may only expose processed prompt + decoded
        // tokens. A present-but-zero n_prompt_tokens still falls back to the
        // retained occupancy (validated CTX selection semantics). When the slot
        // reports none of them, the occupancy stays unknown (None) so the
        // per-slot overview does not print a fake zero.
        let used_opt = match prompt_tokens {
            Some(value) if value > 0 => Some(value),
            _ => match (prompt_processed, decoded) {
                (Some(processed), decoded) => Some(processed.saturating_add(decoded.unwrap_or(0))),
                (None, Some(decoded)) => Some(decoded),
                (None, None) => None,
            },
        };
        // Only a known occupancy may win the CTX row; an unknown slot can at
        // most supply the capacity/id via `fallback_*`.
        if let Some(used) = used_opt {
            if is_processing && best_busy.is_none_or(|best| used > best.0) {
                best_busy = Some((used, n_ctx, slot_id));
            }
            if best_any.is_none_or(|best| used > best.0) {
                best_any = Some((used, n_ctx, slot_id));
            }
        }
        if is_processing && fallback_busy.is_none_or(|best| n_ctx > best.0) {
            fallback_busy = Some((n_ctx, slot_id));
        }
        if fallback_any.is_none_or(|best| n_ctx > best.0) {
            fallback_any = Some((n_ctx, slot_id));
        }

        counters.push(SlotCounter {
            // Never substitute the array position: a slot without a reported
            // id has no trustworthy identity (see `slot_delta_tps`).
            slot_id,
            task_id: slot.get("id_task").and_then(Value::as_i64),
            // Preserve absence per dimension rather than defaulting to 0.
            prompt_processed,
            decoded,
        });
        if let Some(id) = slot_id {
            overview.push(LlmSlotInfo {
                id,
                busy: is_processing,
                context_used: used_opt,
                context_size: (n_ctx > 0).then_some(n_ctx),
            });
        }
    }

    overview.sort_by_key(|slot| slot.id);

    stats.busy_slots = busy;
    // A complete sum only when every busy slot reported the counter; otherwise
    // the total is unknown (None), never a silent partial sum.
    stats.request_prompt_tokens = request_prompt_complete
        .then_some(request_prompt_tokens)
        .flatten();
    stats.request_generated_tokens = request_generated_complete
        .then_some(request_generated_tokens)
        .flatten();
    let (context_used, context_size, context_slot_id) = match best_busy.or(best_any) {
        Some((used, n_ctx, slot_id)) => (Some(used), n_ctx, slot_id),
        None => {
            let (n_ctx, slot_id) = fallback_busy.or(fallback_any).unwrap_or((0, None));
            (None, n_ctx, slot_id)
        }
    };
    stats.context_used = context_used;
    stats.context_slot_id = context_slot_id;
    stats.slot_overview = overview;
    // A slot without n_ctx reports 0; keep the /props-seeded (or previous)
    // size instead of shrinking the pair to 0.
    stats.context_size = if context_size > 0 {
        context_size
    } else {
        stats.context_size
    };
    Ok(counters)
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

fn speculative_config(value: &Value) -> SpeculativeConfig {
    let params = value.get("params");
    let enabled_value = value.get("speculative");
    let n_max_value = params
        .and_then(|params| params.get("speculative.n_max"))
        .or_else(|| value.get("speculative.n_max"));
    let types_value = params
        .and_then(|params| params.get("speculative.types"))
        .or_else(|| value.get("speculative.types"));
    let enabled = enabled_value.and_then(Value::as_bool);
    let n_max = n_max_value.and_then(Value::as_u64);
    let types = types_value.and_then(Value::as_str).unwrap_or_default();
    let named_type = types.split(',').any(|kind| {
        let kind = kind.trim();
        !kind.is_empty() && kind != "none"
    });
    SpeculativeConfig {
        present: enabled_value.is_some() || n_max_value.is_some() || types_value.is_some(),
        enabled,
        is_mtp: types.split(',').any(|kind| kind.trim() == "draft-mtp"),
        n_max,
        types_known: types_value.is_some(),
        named_type,
    }
}

/// Fill the speculative fields the server did not decide from the local
/// server process CLI.
///
/// Precedence (unambiguous):
/// * a server `speculative` boolean is authoritative for enabled/disabled;
/// * a server type list decides the type when it names a real type
///   (`draft-mtp` → MTP; any other named type → not MTP);
/// * a server `speculative.n_max` (including an explicit `0`) is authoritative;
/// * anything the server did not state is filled from the CLI, so a server
///   whose only type report is `speculative.types: "none"` still shows the MTP
///   the process was actually launched with;
/// * a server that explicitly says `speculative: false` is never overridden
///   to MTP by the CLI.
fn apply_local_spec_fallback(
    props: &CachedProps,
    stats: &mut LlmStats,
    local: LocalSpeculativeConfig,
) {
    // The server disabled speculation either explicitly (`speculative: false`)
    // or by reporting `speculative.n_max: 0` (known, but filtered to `None`).
    let n_max_zero = props.spec_n_max_known && props.spec_n_max.is_none();
    let server_disabled = (props.spec_enabled_known && !props.spec_enabled) || n_max_zero;
    if !props.spec_enabled_known && !server_disabled {
        stats.spec_enabled |= local.enabled;
    }
    if !props.spec_named_type && !server_disabled {
        stats.spec_is_mtp |= local.is_mtp;
    }
    if !props.spec_n_max_known && !server_disabled && stats.spec_n_max.is_none() {
        stats.spec_n_max = local.n_max;
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct LocalSpeculativeConfig {
    enabled: bool,
    is_mtp: bool,
    n_max: Option<u64>,
}

/// True when `host` (as returned by [`reqwest::Url::host_str`]) names the
/// local machine. IPv6 hosts come back bracketed (`[::1]`), so brackets are
/// stripped before the address is parsed; a hostname is only `localhost`.
/// This keeps the local SPEC-CLI fallback available for IPv6 loopback
/// endpoints exactly as for IPv4.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn local_speculative_process_config(base: &str) -> Option<LocalSpeculativeConfig> {
    let url = reqwest::Url::parse(base).ok()?;
    let host = url.host_str()?;
    if !is_loopback_host(host) {
        return None;
    }
    let target_port = url.port_or_known_default()?;

    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        if !pid.to_string_lossy().chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if args.is_empty() || !is_llama_server_process(&args) {
            continue;
        }
        if !command_targets_port(&args, target_port) {
            continue;
        }
        let env = fs::read(entry.path().join("environ")).ok();
        return Some(speculative_from_process(&args, env.as_deref()));
    }
    None
}

fn is_llama_server_process(args: &[String]) -> bool {
    let Some(executable) = args.first() else {
        return false;
    };
    let executable = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);

    executable.contains("llama-server")
        || (executable == "llama" && args.get(1).is_some_and(|arg| arg == "serve"))
}

fn command_targets_port(args: &[String], target_port: u16) -> bool {
    let mut saw_port = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--port" {
            saw_port = true;
            if args.get(index + 1).and_then(|v| v.parse::<u16>().ok()) == Some(target_port) {
                return true;
            }
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--port=") {
            saw_port = true;
            if value.parse::<u16>().ok() == Some(target_port) {
                return true;
            }
        }
        index += 1;
    }
    !saw_port && target_port == 8080
}

fn speculative_from_process(args: &[String], env_bytes: Option<&[u8]>) -> LocalSpeculativeConfig {
    let spec_type = cli_value(args, "--spec-type").unwrap_or_default();
    let is_mtp = spec_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    let n_max = cli_value(args, "--spec-draft-n-max")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            env_value(env_bytes, "LLAMA_ARG_SPEC_DRAFT_N_MAX").and_then(|value| value.parse().ok())
        });
    let env_type = env_value(env_bytes, "LLAMA_ARG_SPEC_TYPE").unwrap_or_default();
    let env_mtp = env_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    LocalSpeculativeConfig {
        enabled: is_mtp || env_mtp || n_max.is_some(),
        is_mtp: is_mtp || env_mtp,
        n_max,
    }
}

fn cli_value(args: &[String], flag: &str) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if arg == flag {
            return args.get(index + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn env_value(env_bytes: Option<&[u8]>, key: &str) -> Option<String> {
    env_bytes?
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Per-slot delta throughput, or `None` when the slots cannot be paired
/// safely.
///
/// Array position is never used as identity. Every current slot must carry a
/// unique `id` that also exists exactly once in the previous sample, and the
/// task must not have switched. If any of that fails — missing ids, duplicate
/// ids, a new/disappeared slot, a task change, or the first sample — the whole
/// slot-delta path abstains and the caller falls back to the aggregated
/// `/metrics` counters, so reordered or changed slots cannot fabricate
/// activity.
/// Verified per-slot prompt/decode deltas as `(prompt_tps, generation_tps)`.
/// Each dimension is `None` when it cannot be derived safely (a missing or
/// reset counter on any paired slot); the two dimensions are independent. Any
/// identity problem (missing/duplicate slot id, disappeared slot, task change)
/// makes both dimensions unavailable, preserving the existing safeguards.
fn slot_delta_tps(
    previous: &[SlotCounter],
    current: &[SlotCounter],
    seconds: f64,
) -> (Option<f64>, Option<f64>) {
    let unavailable = (None, None);
    if !seconds.is_finite() || seconds <= 0.0 || current.is_empty() {
        return unavailable;
    }

    let mut prompt_delta = 0u64;
    let mut decoded_delta = 0u64;
    let mut prompt_known = true;
    let mut decoded_known = true;

    for current_slot in current {
        let Some(slot_id) = current_slot.slot_id else {
            return unavailable;
        };
        if current
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
        if previous
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
        let Some(previous_slot) = previous.iter().find(|slot| slot.slot_id == Some(slot_id)) else {
            return unavailable;
        };
        if !task_ids_match(previous_slot.task_id, current_slot.task_id) {
            return unavailable;
        }

        // A dimension is usable when both samples reported it and it did not
        // decrease (reset). A counter missing on exactly one side is a real gap
        // while the slot is active, so that dimension becomes unavailable (no
        // catch-up spike); a counter missing on BOTH sides carries no
        // information and contributes nothing (e.g. an idle slot that omits the
        // field), so it must not invalidate the dimension. The two dimensions
        // are independent.
        match (
            current_slot.prompt_processed,
            previous_slot.prompt_processed,
        ) {
            (Some(now), Some(before)) if now >= before => {
                prompt_delta = prompt_delta.saturating_add(now - before);
            }
            (None, None) => {}
            _ => prompt_known = false,
        }
        match (current_slot.decoded, previous_slot.decoded) {
            (Some(now), Some(before)) if now >= before => {
                decoded_delta = decoded_delta.saturating_add(now - before);
            }
            (None, None) => {}
            _ => decoded_known = false,
        }
    }

    // The loop above only verifies that every *current* slot can be paired.
    // Every *previous* slot must also still be present exactly once — without
    // this, a slot that disappeared would let the survivors' deltas be summed
    // and preferred over the aggregate, silently under-reporting server
    // activity. A previous slot without an `id` cannot be verified either, so
    // it abstains for the same reason.
    for previous_slot in previous {
        let Some(slot_id) = previous_slot.slot_id else {
            return unavailable;
        };
        if current
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
    }

    (
        prompt_known.then(|| prompt_delta as f64 / seconds),
        decoded_known.then(|| decoded_delta as f64 / seconds),
    )
}

/// Live throughput, preferring the verified per-slot delta and otherwise the
/// aggregate `/metrics` counter delta, per dimension. `None` means unavailable
/// — never a fabricated zero.
fn choose_live_throughput(
    slot_live: (Option<f64>, Option<f64>),
    metric_live: (Option<f64>, Option<f64>),
) -> (Option<f64>, Option<f64>) {
    // Per dimension: the verified slot delta wins; otherwise the aggregate
    // metric; otherwise unavailable. Never a fabricated zero.
    (slot_live.0.or(metric_live.0), slot_live.1.or(metric_live.1))
}

fn task_ids_match(previous: Option<i64>, current: Option<i64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => previous == current,
        // Both absent: no task id is available (older responses), so slot-id
        // pairing is the best available identity. A one-sided id is a
        // possible task switch and must not be paired.
        (None, None) => true,
        _ => false,
    }
}

fn slot_decoded_tokens_opt(slot: &Value) -> Option<u64> {
    match slot.get("next_token") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("n_decoded"))
            .and_then(Value::as_u64),
        Some(Value::Object(map)) => map.get("n_decoded").and_then(Value::as_u64),
        _ => None,
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

fn pick_metric_opt(metrics: &[MetricSample], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        metrics
            .iter()
            .find(|sample| sample.name == **name && sample.labels.is_none())
            .map(|sample| sample.value)
    })
}

/// Lifetime average rate from a monotonic token counter and a seconds counter.
/// `None` when either is absent or the denominator is not positive.
fn average_over_seconds(tokens: Option<f64>, seconds: Option<f64>) -> Option<f64> {
    let tokens = tokens?;
    let seconds = seconds?;
    (tokens.is_finite() && seconds.is_finite() && seconds > 0.0).then(|| tokens / seconds)
}

/// Delta of a monotonic counter. `None` when a value is non-finite or the
/// counter decreased (reset): unavailable for that sample, never a fake `0`.
fn counter_delta_opt(current: f64, previous: f64) -> Option<f64> {
    if current.is_finite() && previous.is_finite() && current >= previous {
        Some(current - previous)
    } else {
        None
    }
}

/// Live throughput from a counter pair over `seconds`. `None` when either value
/// is absent (also covers a reset via `counter_delta_opt`).
fn counter_rate(current: Option<f64>, previous: Option<f64>, seconds: f64) -> Option<f64> {
    let (current, previous) = (current?, previous?);
    counter_delta_opt(current, previous).map(|delta| delta / seconds)
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

/// Longest model display string, in `char`s. A server-controlled name is
/// truncated to this so it cannot bloat the TUI or `orsiktop diag` output.
const MAX_MODEL_DISPLAY_CHARS: usize = 120;

fn model_display_name(value: &str) -> String {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .trim_end_matches(".gguf");
    crate::redact::sanitize_display_text(name, MAX_MODEL_DISPLAY_CHARS)
}

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
                            format!(
                                r#"{{"engine":{{"model":"strata"}},"live":{{"queued":0}},"requests_kept":1,"totals":{{"since":1.0,"prompt_tokens":{},"output_tokens":{},"prompt_ms":1000.0,"decode_ms":500.0,"drafts_offered":40,"drafts_accepted":30}}}}"#,
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
        assert_eq!(first.prompt_total, Some(1000.0));
        assert_eq!(first.generated_total, Some(250.0));
        // Lifetime averages from totals + ms-derived seconds.
        assert_eq!(first.prompt_avg_tps, Some(1000.0));
        assert_eq!(first.generation_avg_tps, Some(500.0));
        assert!(first.slots_available);
        assert_eq!(first.slot_count, 1);

        let second = monitor.sample();
        assert!(second.metrics_available);
        assert_eq!(second.prompt_total, Some(2000.0));
        // A later sample derives a live delta rate without a fabricated zero.
        assert!(second
            .generation_tps
            .is_some_and(|value| value.is_finite() && value >= 0.0));

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
    fn slot_fallback_uses_processed_plus_decoded() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "n_ctx": 8192,
            "n_prompt_tokens_processed": 4000,
            "next_token": [{"n_decoded": 250}]
        }]);

        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, Some(4250));
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
    fn present_zero_prompt_tokens_falls_back_to_retained_occupancy() {
        // Validated CTX selection semantics: n_prompt_tokens present-but-zero
        // falls back to the retained processed occupancy. The overview must
        // inherit the exact same value the main CTX row shows.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 0, "n_prompt_tokens_processed": 37943 },
            { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 100 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, Some(37943));
        assert_eq!(stats.context_slot_id, Some(0));
        assert_eq!(stats.slot_overview[0].context_used, Some(37943));
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
